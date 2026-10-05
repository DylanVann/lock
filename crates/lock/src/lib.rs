//! Shared pieces of lock: the on-disk queue, process liveness checks, and
//! formatting helpers used by both the `lock` CLI and the GUI.
//!
//! There is no daemon. The queue lives in `state.json` inside [`state_dir`], and every
//! change happens while holding an exclusive `flock` on `state.lock`. Whoever holds
//! that lock runs [`State::schedule`], which drops entries whose process has died
//! and grants leases in FIFO order. Waiting `lock` processes poll their own entry.
//! Readers (status views, the GUI) can read `state.json` without locking because
//! it is always replaced atomically.

use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub type TaskId = u64;

/// How many finished tasks to keep for display.
const HISTORY_LEN: usize = 50;

/// Successful run times remembered per task, for estimating the next run.
const RUN_TIMES_PER_TASK: usize = 5;
/// How many distinct tasks to remember run times for; the least recently run are dropped.
const RUN_TIMES_TASKS: usize = 500;

/// How long past its deadline a task may keep running before other processes
/// assume its `lock` process is stuck and kill it themselves.
pub const DEADLINE_GRACE_MS: u64 = 30_000;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// CPU-heavy but can share the machine with other shared tasks (builds, tests).
    Shared,
    /// Needs the machine to itself (benchmarks, profiling).
    Exclusive,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Shared => "shared",
            Kind::Exclusive => "exclusive",
        }
    }
}

/// A process identity that survives PID reuse: the PID plus its start time.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Proc {
    pub pid: u32,
    /// Start time as reported by the OS, if it could be read.
    pub started: Option<u64>,
}

impl Proc {
    pub fn of(pid: u32) -> Proc {
        Proc {
            pid,
            started: match process_start_time(pid) {
                Some(ProcStart::At(t)) => Some(t),
                _ => None,
            },
        }
    }

    pub fn is_alive(&self) -> bool {
        // SAFETY: signal 0 only checks for existence and permission.
        let exists = unsafe { libc::kill(self.pid as libc::pid_t, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        if !exists {
            return false;
        }
        match (self.started, process_start_time(self.pid)) {
            // A zombie has exited; it just hasn't been reaped yet.
            (_, Some(ProcStart::Zombie)) => false,
            (Some(recorded), Some(ProcStart::At(current))) => recorded == current,
            _ => true,
        }
    }
}

/// What a `lock` invocation registers about itself.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TaskSpec {
    pub kind: Kind,
    pub name: Option<String>,
    /// Who is asking, e.g. "claude-code" or "codex".
    pub agent: Option<String>,
    pub cwd: String,
    /// Root of the git checkout containing `cwd`, if any.
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub command: Vec<String>,
    /// Maximum run time once the lease is granted.
    pub timeout_ms: Option<u64>,
    /// A long-lived, mostly idle command (a dev server, a debugging host). It takes no
    /// shared slot, and it is paused while an exclusive task runs instead of blocking it.
    #[serde(default)]
    pub light: bool,
}

impl TaskSpec {
    /// "light", "shared" or "exclusive".
    pub fn kind_label(&self) -> &'static str {
        if self.light { "light" } else { self.kind.label() }
    }

    /// The name if given, otherwise the command line.
    pub fn title(&self) -> String {
        match &self.name {
            Some(n) if !n.is_empty() => n.clone(),
            _ => self.command.join(" "),
        }
    }

    /// Identifies "the same task" across runs, for run-time estimates: the repo
    /// (or cwd) plus the name, or the command if there's no name.
    pub fn run_key(&self) -> String {
        let place = self.repo.as_deref().unwrap_or(&self.cwd);
        let what = match &self.name {
            Some(n) if !n.is_empty() => n.clone(),
            _ => self.command.join(" "),
        };
        format!("{place}\t{what}")
    }

    /// Repository (or cwd) name plus branch, e.g. `lock@main`.
    pub fn location(&self) -> String {
        let path = self.repo.as_deref().unwrap_or(&self.cwd);
        let name = Path::new(path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());
        match &self.branch {
            Some(b) => format!("{name}@{b}"),
            None => name,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Waiting,
    Running,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Task {
    pub id: TaskId,
    pub spec: TaskSpec,
    pub state: TaskState,
    /// The `lock` process that owns this entry.
    pub owner: Proc,
    /// The command started under the lease. It leads its own process group.
    pub child: Option<Proc>,
    pub enqueued_at_ms: u64,
    pub started_at_ms: Option<u64>,
    pub deadline_ms: Option<u64>,
    /// Set by `lock cancel` just before it signals the owner.
    #[serde(default)]
    pub cancelled: bool,
    /// How long this task usually takes: the median of its recent successful runs.
    #[serde(default)]
    pub expected_ms: Option<u64>,
    /// The task whose command started this one (a `lock` inside a locked command). While
    /// that task runs, this one works under its lease instead of queueing for a new one.
    #[serde(default)]
    pub parent: Option<TaskId>,
    /// When a light task was paused for an exclusive one. Its timeout doesn't run meanwhile.
    #[serde(default)]
    pub paused_at_ms: Option<u64>,
    /// Set by the scheduler for a nested exclusive task that could never start: another
    /// one is waiting to take the machine from inside a different running task, and each
    /// would wait for the other's outer task to finish. Its `lock` gives up.
    #[serde(default)]
    pub refused: bool,
    /// The jobserver pool ([`Pool::epoch`]) this task's command was given.
    #[serde(default)]
    pub pool_epoch: Option<u64>,
}

impl Task {
    pub fn elapsed_ms(&self, now: u64) -> u64 {
        self.started_at_ms.map_or(0, |s| now.saturating_sub(s))
    }

    pub fn progress(&self, now: u64) -> Progress {
        match self.expected_ms {
            Some(expected) if expected > 0 => {
                let fraction = self.elapsed_ms(now) as f64 / expected as f64;
                if fraction < 1.0 {
                    Progress::Estimated(fraction)
                } else {
                    Progress::Overdue
                }
            }
            _ => Progress::Unknown,
        }
    }

    /// "times out in 40s" during the last fifth of the timeout; `None` before then.
    pub fn timeout_warning(&self, now: u64) -> Option<String> {
        let deadline = self.deadline_ms?;
        let timeout = self.spec.timeout_ms.unwrap_or(0);
        let left = deadline.saturating_sub(now);
        if left == 0 {
            Some("past its timeout".into())
        } else if left <= timeout / 5 {
            Some(format!("times out in {}", fmt_duration_ms(left)))
        } else {
            None
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Outcome {
    /// The command exited. `exit_code` is None if it was killed by a signal.
    Completed { exit_code: Option<i32> },
    TimedOut,
    Cancelled,
    /// The `lock` process died without cleaning up.
    Vanished,
    /// Gave up before getting the lease (wait timeout or interrupted).
    Abandoned,
}

impl Outcome {
    pub fn label(&self) -> String {
        match self {
            Outcome::Completed { exit_code: Some(0) } => "ok".into(),
            Outcome::Completed { exit_code: Some(c) } => format!("exit {c}"),
            Outcome::Completed { exit_code: None } => "killed".into(),
            Outcome::TimedOut => "timed out".into(),
            Outcome::Cancelled => "cancelled".into(),
            Outcome::Vanished => "vanished".into(),
            Outcome::Abandoned => "gave up".into(),
        }
    }

    pub fn is_success(&self) -> bool {
        matches!(self, Outcome::Completed { exit_code: Some(0) })
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Finished {
    pub task: Task,
    pub ended_at_ms: u64,
    pub outcome: Outcome,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct State {
    pub next_id: TaskId,
    /// How many shared tasks may run at once.
    pub shared_slots: u32,
    /// Run timeout for tasks that don't pass `--timeout`.
    #[serde(default = "default_timeout_ms")]
    pub default_timeout_ms: u64,
    /// Running and waiting tasks, in arrival order.
    pub tasks: Vec<Task>,
    /// Most recent first.
    pub history: VecDeque<Finished>,
    /// Recent successful run times, keyed by [`TaskSpec::run_key`].
    #[serde(default)]
    pub run_times: BTreeMap<String, RunTimes>,
    /// Size of the jobserver pool given to shared tasks; 0 turns the jobserver off.
    #[serde(default = "default_jobserver_tokens")]
    pub jobserver_tokens: u32,
    /// The most recent jobserver pool.
    #[serde(default)]
    pub pool: Option<Pool>,
}

/// A machine-wide GNU make jobserver: a FIFO holding one byte per token. Jobserver-aware
/// tools (ninja 1.13+, make 4.4+, cargo, LTO linkers) take a token before starting each
/// job and put it back after, so all the shared tasks running at once share `tokens` jobs
/// between them instead of each starting one job per core.
///
/// A FIFO only keeps its contents while some process has it open, so every task using the
/// pool keeps it open while it runs (see [`join_pool`]), and when none is running the next
/// one starts a new pool with a full set of tokens.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Pool {
    pub epoch: u64,
    pub tokens: u32,
    /// A task using the pool was killed, so its tools may have died holding tokens, which
    /// are then lost for good. The next task starts a new pool instead of joining this one.
    #[serde(default)]
    pub retired: bool,
}

pub fn pool_path(epoch: u64) -> PathBuf {
    state_dir().join(format!("jobserver-{epoch}.fifo"))
}

/// Tokens in a new pool, unless configured with `lock jobs`: one per core. Each tool also
/// runs one job without a token, so a handful of concurrent builds overshoot this slightly.
pub fn default_jobserver_tokens() -> u32 {
    std::thread::available_parallelism().map_or(4, |n| n.get() as u32)
}

/// Where a task's jobserver comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum PoolAssignment {
    /// Join the pool other running tasks are using.
    Join { epoch: u64 },
    /// Nobody is using a usable pool: make a new one with this many tokens.
    Create { epoch: u64, tokens: u32 },
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct RunTimes {
    /// Oldest first.
    pub durations_ms: Vec<u64>,
    pub updated_at_ms: u64,
}

impl RunTimes {
    pub fn median(&self) -> Option<u64> {
        let mut sorted = self.durations_ms.clone();
        sorted.sort_unstable();
        let mid = sorted.len() / 2;
        match sorted.len() {
            0 => None,
            n if n % 2 == 1 => Some(sorted[mid]),
            _ => Some((sorted[mid - 1] + sorted[mid]) / 2),
        }
    }
}

/// How far along a running task is, judged against its usual run time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Progress {
    /// Fraction of the usual run time so far, below 1.
    Estimated(f64),
    /// Running longer than usual.
    Overdue,
    /// No previous runs to go by.
    Unknown,
}

impl Default for State {
    fn default() -> Self {
        State {
            next_id: 1,
            shared_slots: default_shared_slots(),
            default_timeout_ms: default_timeout_ms(),
            tasks: Vec::new(),
            history: VecDeque::new(),
            run_times: BTreeMap::new(),
            jobserver_tokens: default_jobserver_tokens(),
            pool: None,
        }
    }
}

/// Deliberately short: omitting `--timeout` is only meant for quick commands.
/// Anything longer should pass an estimate.
pub fn default_timeout_ms() -> u64 {
    5 * 1000
}

pub fn default_shared_slots() -> u32 {
    let cpus = std::thread::available_parallelism().map_or(4, |n| n.get() as u32);
    (cpus / 3).max(2)
}

impl State {
    pub fn running(&self) -> impl Iterator<Item = &Task> {
        self.tasks.iter().filter(|t| t.state == TaskState::Running)
    }

    pub fn waiting(&self) -> impl Iterator<Item = &Task> {
        self.tasks.iter().filter(|t| t.state == TaskState::Waiting)
    }

    pub fn get(&self, id: TaskId) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    pub fn get_mut(&mut self, id: TaskId) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }

    /// 1-based position among waiting tasks.
    pub fn position(&self, id: TaskId) -> Option<usize> {
        self.waiting().position(|t| t.id == id).map(|p| p + 1)
    }

    /// The running tasks whose lease `task` works under, innermost first: its parent,
    /// the parent's parent and so on, as long as each is running and holds a lease of
    /// its own (light tasks don't). Empty for a task that needs its own lease.
    pub fn lease_chain(&self, task: &Task) -> Vec<TaskId> {
        let mut chain = Vec::new();
        if task.spec.light {
            return chain;
        }
        let mut next = task.parent;
        while let Some(id) = next {
            match self.get(id) {
                Some(p) if p.state == TaskState::Running && !p.spec.light && !chain.contains(&id) => {
                    chain.push(id);
                    next = p.parent;
                }
                _ => break,
            }
        }
        chain
    }

    /// Running tasks that hold a lease of their own and count against the shared slots:
    /// not light, not nested inside another running task.
    pub fn slot_holders(&self) -> impl Iterator<Item = &Task> {
        self.running()
            .filter(|t| t.spec.kind == Kind::Shared && !t.spec.light && self.lease_chain(t).is_empty())
    }

    pub fn enqueue(&mut self, spec: TaskSpec, owner: Proc, parent: Option<TaskId>) -> TaskId {
        let spec_key = spec.run_key();
        let id = self.next_id;
        self.next_id += 1;
        self.tasks.push(Task {
            id,
            spec,
            state: TaskState::Waiting,
            owner,
            child: None,
            enqueued_at_ms: now_ms(),
            started_at_ms: None,
            deadline_ms: None,
            cancelled: false,
            expected_ms: self.run_times.get(&spec_key).and_then(RunTimes::median),
            parent,
            paused_at_ms: None,
            refused: false,
            pool_epoch: None,
        });
        id
    }

    /// Remove a task and record how it ended.
    pub fn finish(&mut self, id: TaskId, outcome: Outcome) -> Option<Finished> {
        let index = self.tasks.iter().position(|t| t.id == id)?;
        let task = self.tasks.remove(index);
        let finished = Finished {
            task,
            ended_at_ms: now_ms(),
            outcome,
        };
        // A command that was killed may have taken jobserver tokens down with it.
        let killed = !matches!(outcome, Outcome::Completed { exit_code: Some(_) } | Outcome::Abandoned);
        if let (true, Some(pool)) = (killed, self.pool.as_mut())
            && finished.task.pool_epoch == Some(pool.epoch)
        {
            pool.retired = true;
        }
        if let (Outcome::Completed { exit_code: Some(0) }, Some(started)) =
            (outcome, finished.task.started_at_ms)
        {
            self.record_run_time(
                finished.task.spec.run_key(),
                finished.ended_at_ms.saturating_sub(started),
            );
        }
        self.history.push_front(finished.clone());
        self.history.truncate(HISTORY_LEN);
        Some(finished)
    }

    fn record_run_time(&mut self, key: String, duration_ms: u64) {
        let entry = self.run_times.entry(key).or_default();
        entry.durations_ms.push(duration_ms);
        if entry.durations_ms.len() > RUN_TIMES_PER_TASK {
            entry.durations_ms.remove(0);
        }
        entry.updated_at_ms = now_ms();
        if self.run_times.len() > RUN_TIMES_TASKS {
            let stalest = self
                .run_times
                .iter()
                .min_by_key(|(_, r)| r.updated_at_ms)
                .map(|(k, _)| k.clone());
            if let Some(key) = stalest {
                self.run_times.remove(&key);
            }
        }
    }

    /// Whether a task's command gets the jobserver: running shared tasks with a lease of
    /// their own. Nested tasks inherit their outer task's jobserver through the environment;
    /// an exclusive task has the machine to itself; a light task may be paused (SIGSTOP)
    /// while holding tokens, which would starve everyone else.
    pub fn uses_pool(&self, task: &Task) -> bool {
        self.jobserver_tokens > 0
            && task.state == TaskState::Running
            && task.spec.kind == Kind::Shared
            && !task.spec.light
            && self.lease_chain(task).is_empty()
    }

    /// Put a running task in the jobserver pool: the current one if another running task is
    /// using it (and it hasn't been retired), otherwise a new one. `None` if the task doesn't
    /// get a jobserver. The caller creates or opens the FIFO; see [`join_pool`].
    pub fn assign_pool(&mut self, id: TaskId) -> Option<PoolAssignment> {
        if !self.uses_pool(self.get(id)?) {
            return None;
        }
        let live = self.pool.as_ref().filter(|p| !p.retired).map(|p| p.epoch);
        let in_use = live.is_some_and(|epoch| {
            self.running()
                .any(|t| t.id != id && t.pool_epoch == Some(epoch))
        });
        let assignment = match live {
            Some(epoch) if in_use => PoolAssignment::Join { epoch },
            _ => {
                let epoch = self.pool.as_ref().map_or(1, |p| p.epoch + 1);
                let tokens = self.jobserver_tokens;
                self.pool = Some(Pool { epoch, tokens, retired: false });
                PoolAssignment::Create { epoch, tokens }
            }
        };
        let epoch = match assignment {
            PoolAssignment::Join { epoch } | PoolAssignment::Create { epoch, .. } => epoch,
        };
        self.get_mut(id)?.pool_epoch = Some(epoch);
        Some(assignment)
    }

    /// The pool running tasks are using, with how many of them use it.
    pub fn active_pool(&self) -> Option<(&Pool, usize)> {
        let pool = self.pool.as_ref()?;
        let users = self
            .running()
            .filter(|t| t.pool_epoch == Some(pool.epoch))
            .count();
        (users > 0).then_some((pool, users))
    }

    /// Drop tasks whose owner died (killing any command it left behind), kill
    /// commands whose owner failed to enforce their deadline, then grant leases.
    pub fn schedule(&mut self) {
        let now = now_ms();
        let mut dead = Vec::new();
        for task in &self.tasks {
            // A paused task's timeout is on hold; its deadline moves when it resumes.
            let overdue = task.paused_at_ms.is_none()
                && task.deadline_ms.is_some_and(|d| now > d + DEADLINE_GRACE_MS);
            if overdue {
                if task.owner.is_alive() {
                    signal(task.owner.pid, libc::SIGKILL);
                }
                dead.push((task.id, Outcome::TimedOut));
            } else if !task.owner.is_alive() {
                dead.push((task.id, Outcome::Vanished));
            } else {
                continue;
            }
            // An orphaned command would keep using the CPU without holding the lock.
            if let Some(child) = task.child.filter(Proc::is_alive) {
                signal_group(child.pid, libc::SIGKILL);
            }
        }
        for (id, outcome) in dead {
            self.finish(id, outcome);
        }
        self.grant(now);
    }

    /// Hand out leases, then pause or resume light tasks.
    ///
    /// A task nested in a running task (see [`State::lease_chain`]) works under that
    /// task's lease, so it starts without queueing and takes no slot: making it wait
    /// behind tasks that wait for its parent would deadlock. A nested exclusive task
    /// under a shared parent (an upgrade) waits until nothing but its parents runs, and
    /// holds back everything else in the queue meanwhile. Two upgrades from inside
    /// different running tasks would each wait for the other's outer task, so the later
    /// one is refused.
    ///
    /// Everything else is strict FIFO: walk the queue granting each waiting task that
    /// fits alongside what's running, and stop at the first one that doesn't. Stopping
    /// there is what keeps an exclusive task from being starved by a stream of shared ones.
    fn grant(&mut self, now: u64) {
        let mut upgrade_waiting = false;
        // The lease chains of the upgrades that are waiting.
        let mut upgrades: Vec<Vec<TaskId>> = Vec::new();
        for i in 0..self.tasks.len() {
            let task = &self.tasks[i];
            if task.state != TaskState::Waiting || task.refused {
                continue;
            }
            let chain = self.lease_chain(task);
            if chain.is_empty() {
                continue;
            }
            let exclusive_elsewhere = self
                .running()
                .any(|t| t.spec.kind == Kind::Exclusive && !chain.contains(&t.id));
            let inside_exclusive = chain
                .iter()
                .any(|id| self.get(*id).is_some_and(|p| p.spec.kind == Kind::Exclusive));
            let kind = task.spec.kind;
            let fits = match kind {
                Kind::Shared => !exclusive_elsewhere,
                Kind::Exclusive if inside_exclusive => !exclusive_elsewhere,
                Kind::Exclusive => self.running().all(|t| t.spec.light || chain.contains(&t.id)),
            };
            if fits {
                Self::start(&mut self.tasks[i], now);
            } else if kind == Kind::Exclusive && !inside_exclusive {
                // Deadlocked with an earlier upgrade if each waits for a task only the other runs inside.
                let deadlocked = upgrades.iter().any(|other| {
                    other.iter().any(|id| !chain.contains(id)) && chain.iter().any(|id| !other.contains(id))
                });
                if deadlocked {
                    self.tasks[i].refused = true;
                } else {
                    upgrades.push(chain);
                    upgrade_waiting = true;
                }
            } else if kind == Kind::Exclusive {
                upgrade_waiting = true;
            }
        }

        if !upgrade_waiting {
            let mut exclusive_running = self.running().any(|t| t.spec.kind == Kind::Exclusive);
            let mut holders = self.running().filter(|t| !t.spec.light && self.lease_chain(t).is_empty()).count() as u32;
            let slots = self.shared_slots.max(1);
            for i in 0..self.tasks.len() {
                let task = &self.tasks[i];
                if task.state != TaskState::Waiting || task.refused || !self.lease_chain(task).is_empty() {
                    continue;
                }
                let fits = match task.spec.kind {
                    _ if task.spec.light => !exclusive_running,
                    Kind::Exclusive => holders == 0,
                    Kind::Shared => !exclusive_running && holders < slots,
                };
                if !fits {
                    break;
                }
                let task = &mut self.tasks[i];
                Self::start(task, now);
                holders += u32::from(!task.spec.light);
                exclusive_running |= task.spec.kind == Kind::Exclusive;
            }
        }

        // Light tasks don't hold up an exclusive one; they sit it out stopped instead.
        let exclusive_running = self.running().any(|t| t.spec.kind == Kind::Exclusive);
        for task in self.tasks.iter_mut() {
            if task.state != TaskState::Running || !task.spec.light {
                continue;
            }
            let child = task.child.filter(Proc::is_alive);
            match (exclusive_running, task.paused_at_ms) {
                (true, None) => {
                    if let Some(child) = child {
                        signal_group(child.pid, libc::SIGSTOP);
                        task.paused_at_ms = Some(now);
                    }
                }
                (false, Some(paused_at)) => {
                    if let Some(child) = child {
                        signal_group(child.pid, libc::SIGCONT);
                    }
                    task.deadline_ms = task.deadline_ms.map(|d| d + now.saturating_sub(paused_at));
                    task.paused_at_ms = None;
                }
                _ => {}
            }
        }
    }

    fn start(task: &mut Task, now: u64) {
        task.state = TaskState::Running;
        task.started_at_ms = Some(now);
        task.deadline_ms = task.spec.timeout_ms.map(|t| now + t);
    }
}

// ---------------------------------------------------------------------------
// Storage

/// State directory: `$LOCK_DIR`, or `~/.local/state/lock`. Deliberately not
/// `$XDG_STATE_HOME`: the macOS app, started from Finder, doesn't see variables set in a
/// shell profile, and it has to find the same queue as the CLI.
pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("LOCK_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(home).join(".local/state/lock")
}

pub fn state_path() -> PathBuf {
    state_dir().join("state.json")
}

fn lock_path() -> PathBuf {
    state_dir().join("state.lock")
}

/// Read the current state without locking. Tasks whose owner has died are
/// filtered out so views don't show them while waiting for someone to clean up.
pub fn read_state() -> std::io::Result<State> {
    let mut state = read_state_raw()?;
    state.tasks.retain(|t| t.owner.is_alive());
    Ok(state)
}

fn read_state_raw() -> std::io::Result<State> {
    match std::fs::read(state_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(e),
    }
}

/// Run `f` on the state while holding the exclusive lock, then save it.
/// [`State::schedule`] runs before `f`, so `f` always sees a cleaned-up queue,
/// and again after `f`, so any slot `f` frees up is handed out straight away.
pub fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> std::io::Result<R> {
    std::fs::create_dir_all(state_dir())?;
    let lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path())?;
    // SAFETY: flock on a file descriptor we own; released when `lock` is dropped.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut state = match read_state_raw() {
        Ok(state) => state,
        // A corrupt file shouldn't wedge every agent on the machine.
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => State::default(),
        Err(e) => return Err(e),
    };
    let before = serde_json::to_vec_pretty(&state)?;
    state.schedule();
    let result = f(&mut state);
    state.schedule();
    let after = serde_json::to_vec_pretty(&state)?;
    // Waiters poll several times a second; don't rewrite the file when nothing changed.
    if after == before {
        return Ok(result);
    }
    let tmp = state_dir().join(format!("state.json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, after)?;
    std::fs::rename(&tmp, state_path())?;
    drop(lock);
    Ok(result)
}

/// An open jobserver pool. Keep it until the task has finished: the FIFO only holds its
/// tokens while some process has it open.
pub struct PoolHandle {
    pub path: PathBuf,
    _fifo: File,
}

/// Give a running task its jobserver: assign it a pool and create or open the FIFO. Call it
/// inside [`with_state`], so no other task can join a pool before its FIFO is filled.
/// `None` if the task doesn't get a jobserver.
pub fn join_pool(state: &mut State, id: TaskId) -> Option<std::io::Result<PoolHandle>> {
    let handle = match state.assign_pool(id)? {
        PoolAssignment::Join { epoch } => match open_fifo(&pool_path(epoch)) {
            Ok(fifo) => Ok(PoolHandle { path: pool_path(epoch), _fifo: fifo }),
            // Its FIFO is gone somehow; start over with a new pool.
            Err(_) => {
                if let Some(pool) = state.pool.as_mut() {
                    pool.retired = true;
                }
                match state.assign_pool(id)? {
                    PoolAssignment::Create { epoch, tokens } => create_pool(epoch, tokens),
                    PoolAssignment::Join { .. } => unreachable!("a retired pool is never joined"),
                }
            }
        },
        PoolAssignment::Create { epoch, tokens } => create_pool(epoch, tokens),
    };
    Some(handle)
}

fn create_pool(epoch: u64, tokens: u32) -> std::io::Result<PoolHandle> {
    let path = pool_path(epoch);
    // Older pools' FIFOs can go: whoever still uses one has it open already.
    if let Ok(entries) = std::fs::read_dir(state_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("jobserver-") && name.ends_with(".fifo") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // SAFETY: mkfifo with a valid NUL-terminated path.
    if unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut fifo = open_fifo(&path)?;
    // A pipe buffer holds at least 4 KiB, far more tokens than anyone needs.
    let tokens = vec![b'+'; tokens.clamp(1, 4096) as usize];
    std::io::Write::write_all(&mut fifo, &tokens)?;
    Ok(PoolHandle { path, _fifo: fifo })
}

/// Open a jobserver FIFO for reading and writing without blocking, as its clients do.
fn open_fifo(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    File::options()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

/// How many tokens are free in a pool right now, or `None` if it can't be checked.
pub fn pool_free_tokens(epoch: u64) -> Option<u32> {
    use std::os::unix::fs::OpenOptionsExt;
    let fifo = File::options()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(pool_path(epoch))
        .ok()?;
    let mut available: libc::c_int = 0;
    // SAFETY: FIONREAD writes the number of readable bytes into an int.
    let ok = unsafe { libc::ioctl(fifo.as_raw_fd(), libc::FIONREAD, &mut available) } == 0;
    ok.then_some(available.max(0) as u32)
}

/// `MAKEFLAGS` that point jobserver-aware tools at `path`, keeping any other flags already
/// set. `None` when `existing` already names a jobserver: one set up by an enclosing `make`
/// is more specific than ours, so it's left alone.
pub fn makeflags_with_jobserver(existing: Option<&str>, path: &Path) -> Option<String> {
    let existing = existing.unwrap_or("").trim();
    if existing.contains("--jobserver-auth") || existing.contains("--jobserver-fds") {
        return None;
    }
    let auth = format!("--jobserver-auth=fifo:{}", path.display());
    Some(if existing.is_empty() { auth } else { format!("{existing} {auth}") })
}

/// Mark a task cancelled and signal its owner, which stops the command and
/// records the outcome. Returns false if there is no such task.
pub fn cancel(id: TaskId) -> std::io::Result<bool> {
    let owner = with_state(|state| {
        state.get_mut(id).map(|task| {
            task.cancelled = true;
            task.owner
        })
    })?;
    match owner {
        Some(owner) => {
            signal(owner.pid, libc::SIGTERM);
            Ok(true)
        }
        None => Ok(false),
    }
}

// ---------------------------------------------------------------------------
// Processes

pub fn signal(pid: u32, sig: libc::c_int) {
    // SAFETY: plain kill(2).
    unsafe { libc::kill(pid as libc::pid_t, sig) };
}

/// Signal the process group led by `pgid`.
pub fn signal_group(pgid: u32, sig: libc::c_int) {
    // SAFETY: plain killpg(2).
    unsafe { libc::killpg(pgid as libc::pid_t, sig) };
}

/// Whether any process is left in the process group `pgid`.
pub fn group_exists(pgid: u32) -> bool {
    // SAFETY: signal 0 only checks for existence and permission.
    let found = unsafe { libc::killpg(pgid as libc::pid_t, 0) } == 0;
    found || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Whether `ancestor` is `pid` itself or one of its parents, grandparents and so on.
pub fn is_ancestor(ancestor: u32, pid: u32) -> bool {
    let mut current = pid;
    for _ in 0..256 {
        if current == ancestor {
            return true;
        }
        match parent_pid(current) {
            Some(parent) if parent > 1 && parent != current => current = parent,
            _ => return false,
        }
    }
    false
}

enum ProcStart {
    At(u64),
    Zombie,
}

#[cfg(target_os = "macos")]
fn parent_pid(pid: u32) -> Option<u32> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: the buffer is a correctly sized proc_bsdinfo.
    let n = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (n == size).then_some(info.pbi_ppid)
}

#[cfg(target_os = "linux")]
fn parent_pid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesised command name: state is field 3, ppid field 4.
    stat[stat.rfind(')')? + 1..].split_whitespace().nth(1)?.parse().ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn parent_pid(_pid: u32) -> Option<u32> {
    None
}

#[cfg(target_os = "macos")]
fn process_start_time(pid: u32) -> Option<ProcStart> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: the buffer is a correctly sized proc_bsdinfo.
    let n = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if n == size {
        return Some(ProcStart::At(
            info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec,
        ));
    }
    // For a zombie, kill(pid, 0) succeeds but the kernel has no task info left.
    let esrch = std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    esrch.then_some(ProcStart::Zombie)
}

#[cfg(target_os = "linux")]
fn process_start_time(pid: u32) -> Option<ProcStart> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesised command name: state is field 3, starttime field 22.
    let mut fields = stat[stat.rfind(')')? + 1..].split_whitespace();
    if fields.next()? == "Z" {
        return Some(ProcStart::Zombie);
    }
    fields.nth(18)?.parse().ok().map(ProcStart::At)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_start_time(_pid: u32) -> Option<ProcStart> {
    None
}

// ---------------------------------------------------------------------------
// Environment detection

/// Walk up from `dir` to find the enclosing git checkout and its current branch.
/// Understands worktrees, where `.git` is a file pointing at the real git dir.
pub fn git_info(dir: &Path) -> (Option<PathBuf>, Option<String>) {
    for ancestor in dir.ancestors() {
        let dot_git = ancestor.join(".git");
        let git_dir = if dot_git.is_dir() {
            dot_git
        } else if let Some(p) = std::fs::read_to_string(&dot_git)
            .ok()
            .and_then(|s| s.trim().strip_prefix("gitdir:").map(|p| p.trim().to_string()))
        {
            ancestor.join(p)
        } else {
            continue;
        };
        let branch = std::fs::read_to_string(git_dir.join("HEAD"))
            .ok()
            .and_then(|head| {
                let head = head.trim();
                match head.strip_prefix("ref: refs/heads/") {
                    Some(b) => Some(b.to_string()),
                    None => head.get(..8).map(str::to_string), // detached: short sha
                }
            });
        return (Some(ancestor.to_path_buf()), branch);
    }
    (None, None)
}

/// Best guess at which agent is running us, from well-known environment variables.
pub fn detect_agent() -> Option<String> {
    if let Some(agent) = std::env::var("LOCK_AGENT").ok().filter(|a| !a.is_empty()) {
        return Some(agent);
    }
    let known = [
        ("CLAUDECODE", "claude-code"),
        ("CODEX_SANDBOX", "codex"),
        ("CODEX_MANAGED_BY_NPM", "codex"),
        ("CURSOR_AGENT", "cursor"),
        ("GEMINI_CLI", "gemini"),
        ("OPENCODE", "opencode"),
    ];
    known
        .iter()
        .find(|(var, _)| std::env::var_os(var).is_some())
        .map(|(_, name)| name.to_string())
}

// ---------------------------------------------------------------------------
// Formatting

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Compact duration: `42s`, `3m05s`, `1h02m`.
pub fn fmt_duration_ms(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

/// How long ago something happened, rounded down to a human unit:
/// "just now", "1 minute ago", "5 minutes ago", "2 hours ago", "3 days ago".
pub fn fmt_ago(ms: u64) -> String {
    let (n, unit) = match ms / 1000 {
        s if s < 60 => return "just now".into(),
        s if s < 3600 => (s / 60, "minute"),
        s if s < 86400 => (s / 3600, "hour"),
        s => (s / 86400, "day"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// Parse `90`, `90s`, `10m`, `1h30m`, `1h 30m`. A bare number means seconds.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if let Ok(secs) = s.parse::<u64>() {
        return Ok(Duration::from_secs(secs));
    }
    humantime::parse_duration(s).map_err(|e| format!("invalid duration {s:?}: {e}"))
}

/// Replace the home directory prefix with `~`.
pub fn tildify(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&home) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    fn spec(kind: Kind) -> TaskSpec {
        TaskSpec {
            kind,
            name: None,
            agent: None,
            cwd: "/".into(),
            repo: None,
            branch: None,
            command: vec!["true".into()],
            timeout_ms: None,
            light: false,
        }
    }

    fn light() -> TaskSpec {
        TaskSpec {
            light: true,
            timeout_ms: Some(60_000),
            ..spec(Kind::Shared)
        }
    }

    fn states(state: &State) -> Vec<TaskState> {
        state.tasks.iter().map(|t| t.state).collect()
    }

    #[test]
    fn fifo_with_exclusive_barrier() {
        use TaskState::*;
        let me = Proc::of(std::process::id());
        let mut state = State {
            shared_slots: 2,
            ..State::default()
        };
        let a = state.enqueue(spec(Kind::Shared), me, None);
        let b = state.enqueue(spec(Kind::Exclusive), me, None);
        state.enqueue(spec(Kind::Shared), me, None);
        state.schedule();
        // The exclusive task blocks the shared task queued behind it.
        assert_eq!(states(&state), [Running, Waiting, Waiting]);
        state.finish(a, Outcome::Completed { exit_code: Some(0) });
        state.schedule();
        assert_eq!(states(&state), [Running, Waiting]);
        state.finish(b, Outcome::Completed { exit_code: Some(0) });
        state.schedule();
        assert_eq!(states(&state), [Running]);
    }

    #[test]
    fn shared_slots_limit() {
        use TaskState::*;
        let me = Proc::of(std::process::id());
        let mut state = State {
            shared_slots: 2,
            ..State::default()
        };
        for _ in 0..3 {
            state.enqueue(spec(Kind::Shared), me, None);
        }
        state.schedule();
        assert_eq!(states(&state), [Running, Running, Waiting]);
    }

    #[test]
    fn nested_tasks_share_their_parents_lease() {
        use TaskState::*;
        let me = Proc::of(std::process::id());
        let mut state = State {
            shared_slots: 1,
            ..State::default()
        };
        let parent = state.enqueue(spec(Kind::Shared), me, None);
        state.schedule();
        // An exclusive task queues behind the parent; a nested shared task must not
        // queue behind it (it would wait for its own parent), nor need a second slot.
        state.enqueue(spec(Kind::Exclusive), me, None);
        let inner = state.enqueue(spec(Kind::Shared), me, Some(parent));
        state.schedule();
        assert_eq!(states(&state), [Running, Waiting, Running]);
        assert_eq!(state.slot_holders().count(), 1);
        // A nested task whose parent isn't running queues like any other.
        let stray = state.enqueue(spec(Kind::Shared), me, Some(999));
        state.schedule();
        assert_eq!(state.get(stray).unwrap().state, Waiting);
        state.finish(inner, Outcome::Completed { exit_code: Some(0) });
        state.finish(stray, Outcome::Abandoned);
    }

    #[test]
    fn nested_exclusive_upgrades_its_shared_parent() {
        use TaskState::*;
        let me = Proc::of(std::process::id());
        let mut state = State {
            shared_slots: 2,
            ..State::default()
        };
        let parent = state.enqueue(spec(Kind::Shared), me, None);
        let other = state.enqueue(spec(Kind::Shared), me, None);
        state.schedule();
        let waiting_exclusive = state.enqueue(spec(Kind::Exclusive), me, None);
        let inner = state.enqueue(spec(Kind::Exclusive), me, Some(parent));
        state.schedule();
        // The nested exclusive waits for the other shared task, not for its own parent.
        assert_eq!(states(&state), [Running, Running, Waiting, Waiting]);
        state.finish(other, Outcome::Completed { exit_code: Some(0) });
        let late = state.enqueue(spec(Kind::Shared), me, None);
        state.schedule();
        assert_eq!(state.get(inner).unwrap().state, Running);
        assert_eq!(state.get(waiting_exclusive).unwrap().state, Waiting);
        assert_eq!(state.get(late).unwrap().state, Waiting);
        // Inside the exclusive task, further nested tasks run too.
        let deeper = state.enqueue(spec(Kind::Shared), me, Some(inner));
        state.schedule();
        assert_eq!(state.get(deeper).unwrap().state, Running);
    }

    #[test]
    fn crossed_upgrades_refuse_the_later_one() {
        use TaskState::*;
        let me = Proc::of(std::process::id());
        let mut state = State {
            shared_slots: 3,
            ..State::default()
        };
        let first = state.enqueue(spec(Kind::Shared), me, None);
        let second = state.enqueue(spec(Kind::Shared), me, None);
        state.schedule();
        // Each nested exclusive would wait for the other's outer task.
        let a = state.enqueue(spec(Kind::Exclusive), me, Some(first));
        let b = state.enqueue(spec(Kind::Exclusive), me, Some(second));
        state.schedule();
        assert!(!state.get(a).unwrap().refused);
        assert!(state.get(b).unwrap().refused);
        // Once the refused task is gone and the other outer task ends, the first upgrade runs.
        state.finish(b, Outcome::Abandoned);
        state.finish(second, Outcome::Completed { exit_code: Some(0) });
        state.schedule();
        assert_eq!(state.get(a).unwrap().state, Running);
        // Two upgrades inside the same task don't deadlock: the second waits for the first.
        let c = state.enqueue(spec(Kind::Exclusive), me, Some(first));
        state.schedule();
        assert!(!state.get(c).unwrap().refused);
        assert_eq!(state.get(c).unwrap().state, Waiting);
    }

    #[test]
    fn light_tasks_take_no_slot_and_pause_for_exclusive() {
        use TaskState::*;
        let me = Proc::of(std::process::id());
        let mut state = State {
            shared_slots: 1,
            ..State::default()
        };
        let mut sleeper = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let idle = state.enqueue(light(), me, None);
        let build = state.enqueue(spec(Kind::Shared), me, None);
        state.schedule();
        assert_eq!(states(&state), [Running, Running]);
        state.get_mut(idle).unwrap().child = Some(Proc::of(sleeper.id()));
        let deadline = state.get(idle).unwrap().deadline_ms.unwrap();

        // The exclusive task waits for the build but not for the light task, which is
        // paused while it runs.
        let bench = state.enqueue(spec(Kind::Exclusive), me, None);
        state.schedule();
        assert_eq!(state.get(bench).unwrap().state, Waiting);
        assert_eq!(state.get(idle).unwrap().paused_at_ms, None);
        state.finish(build, Outcome::Completed { exit_code: Some(0) });
        state.schedule();
        assert_eq!(state.get(bench).unwrap().state, Running);
        assert!(state.get(idle).unwrap().paused_at_ms.is_some());
        // Pretend the pause lasted ten seconds.
        state.get_mut(idle).unwrap().paused_at_ms = Some(now_ms() - 10_000);

        // Resumed with its deadline pushed back by the pause.
        state.finish(bench, Outcome::Completed { exit_code: Some(0) });
        state.schedule();
        let task = state.get(idle).unwrap();
        assert_eq!(task.paused_at_ms, None);
        assert!(task.deadline_ms.unwrap() >= deadline + 10_000);
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
    }

    #[test]
    fn ancestry() {
        let me = std::process::id();
        assert!(is_ancestor(me, me));
        let parent = parent_pid(me).unwrap();
        assert!(is_ancestor(parent, me));
        assert!(!is_ancestor(me, parent));
    }

    #[test]
    fn dead_owner_is_dropped() {
        let mut state = State::default();
        let ghost = Proc {
            pid: 999_999,
            started: None,
        };
        state.enqueue(spec(Kind::Exclusive), ghost, None);
        state.schedule();
        assert!(state.tasks.is_empty());
        assert_eq!(state.history[0].outcome, Outcome::Vanished);
    }

    #[test]
    fn exited_unreaped_process_is_dead() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let proc = Proc::of(child.id());
        std::thread::sleep(Duration::from_millis(200));
        assert!(!proc.is_alive());
        child.wait().unwrap();
        assert!(Proc::of(std::process::id()).is_alive());
    }

    #[test]
    fn expected_duration_is_median_of_successful_runs() {
        let me = Proc::of(std::process::id());
        let mut state = State::default();
        let run = |state: &mut State, ms: u64, exit: i32| {
            let id = state.enqueue(spec(Kind::Shared), me, None);
            state.schedule();
            state.get_mut(id).unwrap().started_at_ms = Some(now_ms() - ms);
            state.finish(id, Outcome::Completed { exit_code: Some(exit) });
        };
        let first = state.enqueue(spec(Kind::Shared), me, None);
        assert_eq!(state.get(first).unwrap().expected_ms, None);
        state.finish(first, Outcome::Abandoned);
        for ms in [10_000, 30_000, 20_000] {
            run(&mut state, ms, 0);
        }
        run(&mut state, 1_000, 1); // failures don't count
        let next = state.enqueue(spec(Kind::Shared), me, None);
        let expected = state.get(next).unwrap().expected_ms.unwrap();
        assert!((20_000..20_100).contains(&expected), "{expected}");
    }

    #[test]
    fn shared_tasks_share_one_pool_until_it_goes_idle_or_is_retired() {
        let me = Proc::of(std::process::id());
        let mut state = State {
            shared_slots: 3,
            jobserver_tokens: 8,
            ..State::default()
        };
        let a = state.enqueue(spec(Kind::Shared), me, None);
        let b = state.enqueue(spec(Kind::Shared), me, None);
        state.schedule();
        assert_eq!(state.assign_pool(a), Some(PoolAssignment::Create { epoch: 1, tokens: 8 }));
        assert_eq!(state.assign_pool(b), Some(PoolAssignment::Join { epoch: 1 }));
        assert_eq!(state.active_pool().map(|(p, users)| (p.epoch, users)), Some((1, 2)));

        // Once nobody uses it, the next task gets a fresh, full pool.
        state.finish(a, Outcome::Completed { exit_code: Some(0) });
        state.finish(b, Outcome::Completed { exit_code: Some(1) });
        let c = state.enqueue(spec(Kind::Shared), me, None);
        let d = state.enqueue(spec(Kind::Shared), me, None);
        state.schedule();
        assert_eq!(state.assign_pool(c), Some(PoolAssignment::Create { epoch: 2, tokens: 8 }));
        assert_eq!(state.assign_pool(d), Some(PoolAssignment::Join { epoch: 2 }));

        // A killed command may have lost tokens: newcomers start over rather than join.
        state.finish(c, Outcome::TimedOut);
        let e = state.enqueue(spec(Kind::Shared), me, None);
        state.schedule();
        assert_eq!(state.assign_pool(e), Some(PoolAssignment::Create { epoch: 3, tokens: 8 }));
    }

    #[test]
    fn only_top_level_shared_tasks_get_the_pool() {
        let me = Proc::of(std::process::id());
        let mut state = State::default();
        let shared = state.enqueue(spec(Kind::Shared), me, None);
        let lit = state.enqueue(light(), me, None);
        state.schedule();
        let nested = state.enqueue(spec(Kind::Shared), me, Some(shared));
        state.schedule();
        assert!(state.assign_pool(shared).is_some());
        assert_eq!(state.assign_pool(lit), None);
        assert_eq!(state.assign_pool(nested), None);
        state.finish(shared, Outcome::Completed { exit_code: Some(0) });
        state.finish(nested, Outcome::Completed { exit_code: Some(0) });
        state.finish(lit, Outcome::Completed { exit_code: Some(0) });
        let exclusive = state.enqueue(spec(Kind::Exclusive), me, None);
        state.schedule();
        assert_eq!(state.assign_pool(exclusive), None);

        state.jobserver_tokens = 0;
        state.finish(exclusive, Outcome::Completed { exit_code: Some(0) });
        let off = state.enqueue(spec(Kind::Shared), me, None);
        state.schedule();
        assert_eq!(state.assign_pool(off), None);
    }

    #[test]
    fn makeflags() {
        let path = Path::new("/tmp/pool.fifo");
        assert_eq!(
            makeflags_with_jobserver(None, path).as_deref(),
            Some("--jobserver-auth=fifo:/tmp/pool.fifo")
        );
        assert_eq!(
            makeflags_with_jobserver(Some(" -k"), path).as_deref(),
            Some("-k --jobserver-auth=fifo:/tmp/pool.fifo")
        );
        assert_eq!(makeflags_with_jobserver(Some("-j8 --jobserver-auth=fifo:/x"), path), None);
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
        assert!(parse_duration("soon").is_err());
        assert_eq!(fmt_duration_ms(185_000), "3m05s");
        assert_eq!(fmt_duration_ms(3_720_000), "1h02m");
        assert_eq!(fmt_ago(59_000), "just now");
        assert_eq!(fmt_ago(60_000), "1 minute ago");
        assert_eq!(fmt_ago(185_000), "3 minutes ago");
        assert_eq!(fmt_ago(7_300_000), "2 hours ago");
        assert_eq!(fmt_ago(86_400_000), "1 day ago");
    }
}
