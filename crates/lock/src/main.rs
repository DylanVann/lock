//! `lock`: put it in front of a CPU-heavy command to queue it machine-wide.
//!
//!     lock -t 5m --name Build bun run build
//!     lock -x -t 10m --name "Bench parser" cargo bench
//!     lock status

use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{ExitCode, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use lock::{
    Kind, Outcome, Proc, State, Task, TaskId, TaskSpec, TaskState, fmt_duration_ms, now_ms,
    parse_duration, read_state, with_state,
};
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::{Instant, sleep, sleep_until};

const POLL: Duration = Duration::from_millis(250);
/// After asking the command to stop, how long to wait before SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(5);
/// How often the guard checks that its `lock` process is still there.
const GUARD_POLL: Duration = Duration::from_millis(200);
/// Set in the command's environment to its task ID, so a `lock` inside it can find the
/// lease it runs under.
const TASK_ENV: &str = "LOCK_TASK";

/// Exit codes, following `timeout(1)` and sysexits where there's a convention.
const EXIT_USAGE: u8 = 2;
const EXIT_TIMEOUT: u8 = 124;
const EXIT_CANCELLED: u8 = 125;
const EXIT_NOT_FOUND: u8 = 127;
const EXIT_WAIT_TIMEOUT: u8 = 75;

#[derive(Parser)]
#[command(
    name = "lock",
    version,
    about = "Queue CPU-heavy commands machine-wide so agents don't trample each other.",
    long_about = "Queue CPU-heavy commands machine-wide so agents don't trample each other.\n\n\
        Put `lock` in front of a command. Shared tasks (the default, e.g. builds) run \
        alongside a limited number of other shared tasks. Exclusive tasks (-x, e.g. \
        benchmarks) wait until nothing else is running, and nothing else starts until \
        they finish. The queue is first-come first-served.\n\n\
        Pass -t with your estimate of how long the command may run; it's stopped after \
        that. Quick commands can leave it off and get a short default (5s).\n\n\
        A lock inside a locked command runs under the outer command's lease instead of \
        queueing again. Light tasks (-l, e.g. a dev server or debugging host that mostly \
        idles) take no shared slot and are paused while an exclusive task runs.\n\n\
        Examples:\n  \
        lock -t 5m --name Build bun run build\n  \
        lock -x -t 10m --name 'Bench parser' cargo bench\n  \
        lock -l -t 1h --name 'Dev server' bun run dev\n  \
        lock status\n  \
        lock watch",
    subcommand_value_name = "COMMAND",
    subcommand_help_heading = "Commands"
)]
struct Cli {
    #[command(flatten)]
    run: RunArgs,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Args)]
struct RunArgs {
    /// Human-readable name shown in the queue, e.g. "Build" or "Bench parser".
    #[arg(short, long)]
    name: Option<String>,
    /// Need the machine to yourself (benchmarks). Default is shared (builds).
    #[arg(short = 'x', long)]
    exclusive: bool,
    /// A long-lived, mostly idle command (dev server, debugging host). It takes no shared
    /// slot, and it is paused (SIGSTOP) while an exclusive task runs instead of holding it up.
    #[arg(short, long, conflicts_with = "exclusive")]
    light: bool,
    /// Your estimate of the longest this command should run once started (e.g. 90s, 10m, 1h);
    /// it's stopped after that. Only omit it for quick commands: the default is 5s
    /// (see `lock default-timeout`).
    #[arg(short, long, value_parser = parse_duration)]
    timeout: Option<Duration>,
    /// How long this command usually runs, for the progress bar (e.g. 3m). Defaults to the
    /// median of its recent successful runs.
    #[arg(short, long, value_parser = parse_duration)]
    estimate: Option<Duration>,
    /// Give up if the lock isn't acquired within this long. Exits with 75.
    #[arg(short, long, value_parser = parse_duration)]
    wait_timeout: Option<Duration>,
    /// Who is running this. Defaults to $LOCK_AGENT or a detected agent.
    #[arg(long)]
    agent: Option<String>,
    /// Don't print queue progress to stderr.
    #[arg(short, long)]
    quiet: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Show running, waiting and recently finished tasks.
    Status {
        /// Print the raw state as JSON.
        #[arg(long)]
        json: bool,
        /// How many recently finished tasks to show.
        #[arg(long, default_value_t = 5)]
        history: usize,
    },
    /// Live view of the queue. Ctrl-C to exit.
    Watch {
        #[arg(long, default_value_t = 10)]
        history: usize,
    },
    /// Stop a running task or remove a waiting one.
    Cancel { ids: Vec<TaskId> },
    /// Show or set how many shared tasks may run at once.
    Slots { count: Option<u32> },
    /// Show or set the size of the jobserver shared by running shared tasks ("off" to
    /// disable). Jobserver-aware tools (ninja, make 4.4+, cargo) take a token per job.
    Jobs { count: Option<String> },
    /// Show or set the run timeout for commands that don't pass --timeout.
    DefaultTimeout {
        #[arg(value_parser = parse_duration)]
        duration: Option<Duration>,
    },
    /// Internal: stops a command whose `lock` process died.
    #[command(name = "__guard", hide = true)]
    Guard { owner: u32, pgid: u32 },
    /// Run a command under the lock (same as `lock [OPTIONS] CMD...`).
    #[command(external_subcommand)]
    Run(Vec<OsString>),
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        None => status(false, 5),
        Some(Command::Status { json, history }) => status(json, history),
        Some(Command::Watch { history }) => watch(history),
        Some(Command::Cancel { ids }) => cancel(ids),
        Some(Command::Slots { count }) => slots(count),
        Some(Command::Jobs { count }) => jobs(count),
        Some(Command::DefaultTimeout { duration }) => default_timeout(duration),
        Some(Command::Guard { owner, pgid }) => guard(owner, pgid),
        Some(Command::Run(command)) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            match runtime.block_on(run(cli.run, command)) {
                Ok(code) => return ExitCode::from(code),
                Err(e) => Err(e),
            }
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lock: {e:#}");
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------
// Running a command

struct Reporter {
    quiet: bool,
}

impl Reporter {
    fn say(&self, message: impl std::fmt::Display) {
        if !self.quiet {
            eprintln!("lock: {message}");
        }
    }
}

async fn run(args: RunArgs, command: Vec<OsString>) -> Result<u8> {
    let out = Reporter { quiet: args.quiet };
    // A state file that can't be read is replaced by the next change to the queue, so it
    // mustn't stop us here: it only means the defaults and no outer task.
    let current = read_state().unwrap_or_default();
    // Every task has a timeout, so a hung command can't hold the machine indefinitely.
    let timeout = match args.timeout {
        Some(t) if t.is_zero() => {
            eprintln!("lock: --timeout must be greater than zero");
            return Ok(EXIT_USAGE);
        }
        Some(t) => t,
        None => Duration::from_millis(current.default_timeout_ms),
    };
    let defaulted = args.timeout.is_none();
    let parent = nested_in(&current);
    let cwd = std::env::current_dir().context("reading current directory")?;
    let (repo, branch) = lock::git_info(&cwd);
    let kind = if args.exclusive {
        Kind::Exclusive
    } else {
        Kind::Shared
    };
    let spec = TaskSpec {
        kind,
        name: args.name,
        agent: args.agent.or_else(lock::detect_agent),
        cwd: cwd.to_string_lossy().into_owned(),
        repo: repo.map(|r| r.to_string_lossy().into_owned()),
        branch,
        command: command
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect(),
        timeout_ms: Some(timeout.as_millis() as u64),
        light: args.light,
    };

    if kind == Kind::Shared
        && !spec.light
        && parent.is_none()
        && current.jobserver_tokens > 0
        && let Some(hint) = explicit_job_count(&spec.command)
    {
        out.say(hint);
    }

    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sighup = signal(SignalKind::hangup())?;

    // Join the queue and wait our turn.
    let me = Proc::of(std::process::id());
    let enqueued = Instant::now();
    let id = with_state(|s| {
        let id = s.enqueue(spec.clone(), me, parent);
        if let (Some(estimate), Some(task)) = (args.estimate, s.get_mut(id)) {
            task.expected_ms = Some(estimate.as_millis() as u64);
        }
        id
    })?;
    let give_up_at = args.wait_timeout.map(|w| enqueued + w);
    let mut last_position = None;
    loop {
        let (task, position, summary) =
            with_state(|s| (s.get(id).cloned(), s.position(id), busy_summary(s)))?;
        let Some(task) = task else {
            // Someone removed us; only `lock cancel` does that to a live process.
            out.say("removed from the queue");
            return Ok(EXIT_CANCELLED);
        };
        if task.refused {
            with_state(|s| s.finish(id, Outcome::Abandoned))?;
            // Shown even with --quiet: the caller has to change how it runs.
            eprintln!(
                "lock: can't take the machine from inside task {}: another command, inside a different \
                 running task, is already waiting to, and each would wait for the other. Run the outer \
                 command with -x instead.",
                task.parent.map_or("?".into(), |p| p.to_string())
            );
            return Ok(EXIT_WAIT_TIMEOUT);
        }
        if task.state == TaskState::Running {
            if last_position.is_some() {
                out.say(format_args!(
                    "acquired {} lock after {}",
                    kind.label(),
                    fmt_duration_ms(enqueued.elapsed().as_millis() as u64)
                ));
            }
            break;
        }
        if position != last_position {
            out.say(format_args!(
                "waiting for {} lock (#{} in queue; {summary})",
                kind.label(),
                position.unwrap_or(0),
            ));
            last_position = position;
        }
        let interrupted = tokio::select! {
            _ = sleep(POLL) => None,
            _ = sleep_until(give_up_at.unwrap_or_else(far_future)), if give_up_at.is_some() => {
                out.say("gave up waiting for the lock");
                Some(EXIT_WAIT_TIMEOUT)
            }
            _ = sigint.recv() => Some(128 + libc::SIGINT as u8),
            _ = sigterm.recv() => Some(128 + libc::SIGTERM as u8),
            _ = sighup.recv() => Some(128 + libc::SIGHUP as u8),
        };
        if let Some(code) = interrupted {
            let finished = with_state(|s| {
                let outcome = match s.get(id) {
                    Some(t) if t.cancelled => Outcome::Cancelled,
                    _ => Outcome::Abandoned,
                };
                s.finish(id, outcome)
            })?;
            return Ok(match finished.map(|f| f.outcome) {
                Some(Outcome::Cancelled) => {
                    out.say("cancelled while waiting");
                    EXIT_CANCELLED
                }
                _ => code,
            });
        }
    }

    // We hold the lease. From here on, every exit path must release it.
    let pool = join_jobserver(&out, id);
    let jobserver = pool.as_ref().map(|p| p.path.as_path());
    let result = run_command(&out, id, &spec, defaulted, jobserver, &mut sigint, &mut sigterm, &mut sighup).await;
    let (outcome, code) = match &result {
        Ok(v) => *v,
        Err(_) => (Outcome::Completed { exit_code: None }, 1),
    };
    let cancelled = with_state(|s| {
        let outcome = match s.get(id) {
            Some(t) if t.cancelled && outcome != Outcome::TimedOut => Outcome::Cancelled,
            _ => outcome,
        };
        s.finish(id, outcome);
        outcome == Outcome::Cancelled
    })?;
    // Only now, once we no longer count as using the pool: closing the FIFO while still
    // counted could let a newcomer join a pool that nobody holds open, and so is empty.
    drop(pool);
    result?;
    Ok(if cancelled { EXIT_CANCELLED } else { code })
}

/// Give a shared task the machine-wide jobserver (see [`lock::Pool`]), unless one is
/// already set up around us (say, `lock` in a make recipe). Problems only cost the command
/// its jobserver, never the run.
fn join_jobserver(out: &Reporter, id: TaskId) -> Option<lock::PoolHandle> {
    let existing = std::env::var("MAKEFLAGS").ok();
    if existing.as_deref().is_some_and(|f| f.contains("--jobserver-")) {
        return None;
    }
    match with_state(|s| lock::join_pool(s, id)) {
        Ok(Some(Ok(pool))) => Some(pool),
        Ok(Some(Err(e))) => {
            out.say(format_args!("running without the jobserver: {e}"));
            None
        }
        Ok(None) => None,
        Err(e) => {
            out.say(format_args!("running without the jobserver: {e}"));
            None
        }
    }
}

/// Ninja, make and `cmake --build` ignore a jobserver when given an explicit job count
/// and run that many jobs regardless of other builds. Returns a note saying so. It's only
/// a note: a fixed count is sometimes deliberate, e.g. for jobs that are themselves
/// multithreaded (test suites), which would take one token each but use several cores.
fn explicit_job_count(command: &[String]) -> Option<String> {
    let (program, args) = command.split_first()?;
    let program = Path::new(program).file_name()?.to_string_lossy();
    let has_j = |args: &[String]| {
        args.iter().any(|a| {
            (a.starts_with("-j") && !a.starts_with("-j-")) || a == "--jobs" || a.starts_with("--jobs=")
        })
    };
    match program.as_ref() {
        "ninja" | "make" | "gmake" if has_j(args) => Some(format!(
            "note: with -j, {program} runs that many jobs itself instead of sharing the machine-wide \
             jobserver with other builds"
        )),
        "cmake" if args.iter().any(|a| a == "--build") => {
            let parallel = has_j(args) || args.iter().any(|a| a.starts_with("--parallel"));
            let from_env = std::env::var_os("CMAKE_BUILD_PARALLEL_LEVEL").is_some();
            (parallel || from_env).then(|| {
                let how = if parallel { "--parallel/-j" } else { "CMAKE_BUILD_PARALLEL_LEVEL" };
                format!(
                    "note: cmake --build passes {how} on as -j, so the build runs that many jobs itself \
                     instead of sharing the machine-wide jobserver with other builds"
                )
            })
        }
        _ => None,
    }
}

/// The task this `lock` runs inside, if it was started by a locked command: `$LOCK_TASK`
/// names a running task whose command is one of our ancestors. Checking the ancestry
/// keeps a variable leaked into an unrelated process (say, a daemon started by a build)
/// from borrowing someone else's lease.
fn nested_in(state: &State) -> Option<TaskId> {
    let id: TaskId = std::env::var(TASK_ENV).ok()?.trim().parse().ok()?;
    let task = state.get(id)?;
    let child = task.child?;
    (task.state == TaskState::Running && lock::is_ancestor(child.pid, std::process::id())).then_some(id)
}

fn far_future() -> Instant {
    Instant::now() + Duration::from_secs(86400 * 365)
}

/// e.g. "running: 2 shared" or "running: 1 exclusive; 3 ahead".
fn busy_summary(state: &State) -> String {
    let exclusive = state
        .running()
        .filter(|t| t.spec.kind == Kind::Exclusive)
        .count();
    let shared = state.slot_holders().count();
    let light = state.running().filter(|t| t.spec.light).count();
    let mut parts = Vec::new();
    if exclusive > 0 {
        parts.push(format!("{exclusive} exclusive"));
    }
    if shared > 0 {
        parts.push(format!("{shared}/{} shared", state.shared_slots));
    }
    if light > 0 {
        parts.push(format!("{light} light"));
    }
    if parts.is_empty() {
        return "nothing running".into();
    }
    format!("running: {}", parts.join(", "))
}

async fn run_command(
    out: &Reporter,
    id: TaskId,
    spec: &TaskSpec,
    defaulted: bool,
    jobserver: Option<&Path>,
    sigint: &mut tokio::signal::unix::Signal,
    sigterm: &mut tokio::signal::unix::Signal,
    sighup: &mut tokio::signal::unix::Signal,
) -> Result<(Outcome, u8)> {
    let Some((program, args)) = spec.command.split_first() else {
        bail!("no command given");
    };
    let foreground = TerminalHandoff::new();
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args);
    // A `lock` inside the command runs under this task's lease.
    cmd.env(TASK_ENV, id.to_string());
    if let Some(path) = jobserver {
        let existing = std::env::var("MAKEFLAGS").ok();
        if let Some(flags) = lock::makeflags_with_jobserver(existing.as_deref(), path) {
            cmd.env("MAKEFLAGS", flags);
        }
    }
    // Own process group, so a timeout can stop the whole tree (cargo -> rustc etc.).
    cmd.process_group(0);
    if foreground.is_some() {
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                // Take the terminal so the command can read it and gets Ctrl-C directly.
                libc::signal(libc::SIGTTOU, libc::SIG_IGN);
                libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpid());
                libc::signal(libc::SIGTTOU, libc::SIG_DFL);
                Ok(())
            });
        }
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            out.say(format_args!("{program}: {e}"));
            let code = if e.kind() == std::io::ErrorKind::NotFound {
                EXIT_NOT_FOUND
            } else {
                126
            };
            return Ok((Outcome::Completed { exit_code: Some(code as i32) }, code));
        }
    };
    let pgid = child.id().expect("child pid");
    let mut guard = spawn_guard(pgid);
    with_state(|s| {
        if let Some(task) = s.get_mut(id) {
            task.child = Some(Proc::of(pgid));
        }
    })?;

    let mut deadline = Instant::now() + Duration::from_millis(spec.timeout_ms.unwrap_or(0));
    let mut timed_out = false;
    let mut kill_at: Option<Instant> = None;
    let light = spec.light;
    let stop = |sig: libc::c_int, kill_at: &mut Option<Instant>| {
        lock::signal_group(pgid, sig);
        if light {
            // A light task may be paused for an exclusive one, and can't act on a signal until resumed.
            lock::signal_group(pgid, libc::SIGCONT);
        }
        // A second request, or a command that ignores the first, gets SIGKILL.
        *kill_at = Some(match kill_at {
            Some(_) => Instant::now(),
            None => Instant::now() + KILL_GRACE,
        });
    };

    let status = loop {
        tokio::select! {
            status = child.wait() => break status?,
            _ = sleep_until(deadline), if !timed_out => {
                // A light task's timeout is on hold while it's paused; the scheduler moves its deadline.
                if let Some(later) = current_deadline(id) {
                    deadline = later;
                    continue;
                }
                timed_out = true;
                let limit = fmt_duration_ms(spec.timeout_ms.unwrap_or(0));
                if defaulted {
                    // Shown even with --quiet: the caller needs to know to pass --timeout.
                    eprintln!(
                        "lock: stopped after the default timeout of {limit}, which is only meant for \
                         quick commands. Pass your estimate of how long it needs, e.g. `lock -t 10m ...`"
                    );
                } else {
                    out.say(format_args!("timeout of {limit} reached, stopping command"));
                }
                stop(libc::SIGTERM, &mut kill_at);
            }
            _ = sleep_until(kill_at.unwrap_or_else(far_future)), if kill_at.is_some() => {
                lock::signal_group(pgid, libc::SIGKILL);
                kill_at = None;
            }
            // Only reaches us when the command doesn't own the terminal.
            _ = sigint.recv() => stop(libc::SIGINT, &mut kill_at),
            _ = sigterm.recv() => stop(libc::SIGTERM, &mut kill_at),
            _ = sighup.recv() => stop(libc::SIGHUP, &mut kill_at),
        }
    };
    drop(foreground);
    // Don't leave stragglers from the group (background jobs the command started and
    // didn't wait for) running without the lock. Processes that left the group with
    // setsid, such as build daemons, are out of reach and meant to outlive the command.
    if timed_out || kill_at.is_some() {
        lock::signal_group(pgid, libc::SIGKILL);
    } else if lock::group_exists(pgid) {
        stop_stragglers(pgid, spec.light).await;
    }
    if let Some(guard) = guard.as_mut() {
        let _ = guard.kill();
        let _ = guard.wait();
    }

    use std::os::unix::process::ExitStatusExt;
    let code = match (status.code(), status.signal()) {
        (Some(c), _) => c.clamp(0, 255) as u8,
        (None, Some(sig)) => 128 + sig as u8,
        _ => 1,
    };
    if timed_out {
        return Ok((Outcome::TimedOut, EXIT_TIMEOUT));
    }
    Ok((Outcome::Completed { exit_code: status.code() }, code))
}

/// Ask what's left of the command's process group to stop, as a timeout would: SIGTERM,
/// then SIGKILL if it's still there after the grace period.
async fn stop_stragglers(pgid: u32, light: bool) {
    lock::signal_group(pgid, libc::SIGTERM);
    if light {
        lock::signal_group(pgid, libc::SIGCONT);
    }
    let give_up = Instant::now() + KILL_GRACE;
    while lock::group_exists(pgid) && Instant::now() < give_up {
        sleep(Duration::from_millis(50)).await;
    }
    lock::signal_group(pgid, libc::SIGKILL);
}

/// Where the task's deadline stands now: `None` once it has passed, a moment from now
/// while the task is paused, otherwise the (possibly moved) deadline.
fn current_deadline(id: TaskId) -> Option<Instant> {
    let state = read_state().ok()?;
    let task = state.get(id)?;
    if task.paused_at_ms.is_some() {
        return Some(Instant::now() + Duration::from_secs(1));
    }
    let left = task.deadline_ms?.checked_sub(now_ms())?;
    (left > 50).then(|| Instant::now() + Duration::from_millis(left))
}

/// Start `lock __guard`, which stops the command if this process dies without doing so
/// (kill -9, a crash). Otherwise the command would keep running without the lock until
/// the next `lock` invocation noticed. It runs in its own session, so whatever takes
/// down this process (a terminal hangup, a killed process group) doesn't take it too.
fn spawn_guard(pgid: u32) -> Option<std::process::Child> {
    let exe = std::env::current_exe().ok()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["__guard", &std::process::id().to_string(), &pgid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().ok()
}

/// The guard's loop: wait until the command's group is gone (done), or until our parent
/// `lock` process is (we were reparented), and then stop the group.
fn guard(owner: u32, pgid: u32) -> Result<()> {
    loop {
        std::thread::sleep(GUARD_POLL);
        if !lock::group_exists(pgid) {
            return Ok(());
        }
        // SAFETY: plain getppid(2).
        if unsafe { libc::getppid() } as u32 != owner {
            break;
        }
    }
    lock::signal_group(pgid, libc::SIGTERM);
    lock::signal_group(pgid, libc::SIGCONT);
    let give_up = std::time::Instant::now() + KILL_GRACE;
    while lock::group_exists(pgid) && std::time::Instant::now() < give_up {
        std::thread::sleep(GUARD_POLL);
    }
    lock::signal_group(pgid, libc::SIGKILL);
    // Drop the dead task and hand its slot on now, not whenever someone next uses the queue.
    with_state(|_| ())?;
    Ok(())
}

/// When we're the foreground job on a terminal, the command gets the terminal
/// while it runs. Dropping this takes it back.
struct TerminalHandoff {
    our_pgrp: libc::pid_t,
}

impl TerminalHandoff {
    fn new() -> Option<TerminalHandoff> {
        // SAFETY: plain libc queries on stdin.
        unsafe {
            if libc::isatty(libc::STDIN_FILENO) != 1 {
                return None;
            }
            let our_pgrp = libc::getpgrp();
            (libc::tcgetpgrp(libc::STDIN_FILENO) == our_pgrp).then_some(TerminalHandoff { our_pgrp })
        }
    }
}

impl Drop for TerminalHandoff {
    fn drop(&mut self) {
        // SAFETY: we're a background group at this point, so ignore SIGTTOU while
        // taking the terminal back.
        unsafe {
            let old = libc::signal(libc::SIGTTOU, libc::SIG_IGN);
            libc::tcsetpgrp(libc::STDIN_FILENO, self.our_pgrp);
            libc::signal(libc::SIGTTOU, old);
        }
    }
}

// ---------------------------------------------------------------------------
// Viewing and managing the queue

fn status(json: bool, history: usize) -> Result<()> {
    let state = read_state()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&state)?);
        return Ok(());
    }
    let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    print!("{}", render(&state, history, color));
    Ok(())
}

fn watch(history: usize) -> Result<()> {
    let mut stdout = std::io::stdout();
    let color = stdout.is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let stop = stop.clone();
        // SIGINT just sets the flag; the loop restores the terminal before exiting.
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(async {
                let _ = tokio::signal::ctrl_c().await;
            });
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
        });
    }
    // Alternate screen, hidden cursor.
    write!(stdout, "\x1b[?1049h\x1b[?25l")?;
    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
        let frame = match read_state() {
            Ok(state) => render(&state, history, color),
            Err(e) => format!("error reading state: {e}\n"),
        };
        let clock = humantime::format_rfc3339_seconds(std::time::SystemTime::now());
        write!(
            stdout,
            "\x1b[H\x1b[2J{}\n{frame}",
            dim(&format!("lock watch · {clock} · Ctrl-C to exit"), color)
        )?;
        stdout.flush()?;
        for _ in 0..5 {
            if stop.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    write!(stdout, "\x1b[?25h\x1b[?1049l")?;
    stdout.flush()?;
    Ok(())
}

fn cancel(ids: Vec<TaskId>) -> Result<()> {
    if ids.is_empty() {
        bail!("give the ID of the task to cancel (see `lock status`)");
    }
    let mut missing = Vec::new();
    for id in ids {
        if lock::cancel(id)? {
            println!("cancelled {id}");
        } else {
            missing.push(id.to_string());
        }
    }
    if !missing.is_empty() {
        bail!("no such task: {}", missing.join(", "));
    }
    Ok(())
}

fn default_timeout(duration: Option<Duration>) -> Result<()> {
    match duration {
        Some(d) if d.is_zero() => bail!("the default timeout must be greater than zero"),
        Some(d) => {
            with_state(|s| s.default_timeout_ms = d.as_millis() as u64)?;
            println!("default timeout: {}", fmt_duration_ms(d.as_millis() as u64));
        }
        None => println!(
            "default timeout: {}",
            fmt_duration_ms(read_state()?.default_timeout_ms)
        ),
    }
    Ok(())
}

fn jobs(count: Option<String>) -> Result<()> {
    let tokens = match count.as_deref().map(str::trim) {
        None => None,
        Some("off" | "0") => Some(0),
        Some(n) => match n.parse::<u32>() {
            Ok(n) if n <= 4096 => Some(n),
            _ => bail!("expected a number of jobs (up to 4096) or \"off\", got {n:?}"),
        },
    };
    if let Some(tokens) = tokens {
        // Tasks already running keep the pool they have; the next one starts a new pool.
        with_state(|s| {
            s.jobserver_tokens = tokens;
            if let Some(pool) = s.pool.as_mut() {
                pool.retired = true;
            }
        })?;
    }
    let state = read_state()?;
    if state.jobserver_tokens == 0 {
        println!("jobserver: off");
        return Ok(());
    }
    println!("jobserver: {} jobs shared by running shared tasks", state.jobserver_tokens);
    if let Some((pool, users)) = state.active_pool() {
        let free = lock::pool_free_tokens(pool.epoch)
            .map_or(String::new(), |f| format!(", {f} of {} tokens free", pool.tokens));
        let tasks = if users == 1 { "task" } else { "tasks" };
        println!("in use by {users} {tasks}{free}: {}", lock::pool_path(pool.epoch).display());
    }
    Ok(())
}

fn slots(count: Option<u32>) -> Result<()> {
    match count {
        Some(0) => bail!("need at least 1 slot"),
        Some(n) => {
            with_state(|s| s.shared_slots = n)?;
            println!("shared slots: {n}");
        }
        None => println!("shared slots: {}", read_state()?.shared_slots),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Rendering

fn render(state: &State, history: usize, color: bool) -> String {
    let now = now_ms();
    let mut out = String::new();
    let running: Vec<&Task> = state.running().collect();
    let waiting: Vec<&Task> = state.waiting().collect();
    let shared = state.slot_holders().count();
    let exclusive = running
        .iter()
        .filter(|t| t.spec.kind == Kind::Exclusive)
        .count();
    let light = running.iter().filter(|t| t.spec.light).count();

    let mut header = if exclusive > 0 {
        "exclusive lock held".to_string()
    } else {
        format!("{shared}/{} shared slots in use", state.shared_slots)
    };
    if light > 0 {
        header += &format!(", {light} light");
    }
    if let Some((pool, _)) = state.active_pool()
        && let Some(free) = lock::pool_free_tokens(pool.epoch)
    {
        header += &format!(" · {free}/{} jobs free", pool.tokens);
    }
    out += &format!("{} {}\n", bold("RUNNING", color), dim(&format!("· {header}"), color));
    if running.is_empty() {
        out += &dim("  nothing running\n", color);
    } else {
        let mut table = Table::new(&["ID", "KIND", "RUNNING", "USUAL", "LEFT", "WAITED", "AGENT", "WHERE", "PID", "NAME"]);
        for t in &running {
            let started = t.started_at_ms.unwrap_or(now);
            let left = match t.deadline_ms {
                _ if t.paused_at_ms.is_some() => "paused".into(),
                Some(d) if d > now => fmt_duration_ms(d - now),
                Some(_) => "overdue".into(),
                None => "-".into(),
            };
            let elapsed = fmt_duration_ms(t.elapsed_ms(now));
            // Yellow once it's taking longer than it usually does.
            let elapsed = match t.progress(now) {
                lock::Progress::Overdue => paint(&elapsed, "33", color),
                _ => elapsed,
            };
            table.row(vec![
                t.id.to_string(),
                kind_cell(state, t, color),
                elapsed,
                usual_cell(t),
                left,
                fmt_duration_ms(started.saturating_sub(t.enqueued_at_ms)),
                t.spec.agent.clone().unwrap_or_else(|| "-".into()),
                t.spec.location(),
                t.child.map_or(t.owner.pid, |c| c.pid).to_string(),
                t.spec.title(),
            ]);
        }
        out += &table.render(color);
    }

    out += &format!(
        "\n{} {}\n",
        bold("QUEUE", color),
        dim(&format!("· {} waiting", waiting.len()), color)
    );
    if waiting.is_empty() {
        out += &dim("  empty\n", color);
    } else {
        let mut table = Table::new(&["#", "ID", "KIND", "WAITING", "USUAL", "TIMEOUT", "AGENT", "WHERE", "PID", "NAME"]);
        for (i, t) in waiting.iter().enumerate() {
            table.row(vec![
                (i + 1).to_string(),
                t.id.to_string(),
                kind_cell(state, t, color),
                fmt_duration_ms(now.saturating_sub(t.enqueued_at_ms)),
                usual_cell(t),
                t.spec.timeout_ms.map_or("-".into(), fmt_duration_ms),
                t.spec.agent.clone().unwrap_or_else(|| "-".into()),
                t.spec.location(),
                t.owner.pid.to_string(),
                t.spec.title(),
            ]);
        }
        out += &table.render(color);
    }

    if history > 0 && !state.history.is_empty() {
        out += &format!("\n{}\n", bold("RECENT", color));
        let mut table = Table::new(&["ID", "KIND", "RESULT", "RAN", "WAITED", "ENDED", "AGENT", "WHERE", "NAME"]);
        for f in state.history.iter().take(history) {
            let t = &f.task;
            let (ran, waited) = match t.started_at_ms {
                Some(s) => (
                    fmt_duration_ms(f.ended_at_ms.saturating_sub(s)),
                    fmt_duration_ms(s.saturating_sub(t.enqueued_at_ms)),
                ),
                None => (
                    "-".into(),
                    fmt_duration_ms(f.ended_at_ms.saturating_sub(t.enqueued_at_ms)),
                ),
            };
            let result = f.outcome.label();
            let result = if f.outcome.is_success() {
                paint(&result, "32", color)
            } else {
                paint(&result, "31", color)
            };
            table.row(vec![
                t.id.to_string(),
                kind_cell(state, t, color),
                result,
                ran,
                waited,
                lock::fmt_ago(now.saturating_sub(f.ended_at_ms)),
                t.spec.agent.clone().unwrap_or_else(|| "-".into()),
                t.spec.location(),
                t.spec.title(),
            ]);
        }
        out += &table.render(color);
    }
    out
}

/// The task's typical run time from previous runs, e.g. "~3m05s".
fn usual_cell(task: &Task) -> String {
    task.expected_ms
        .map_or("-".into(), |ms| format!("~{}", fmt_duration_ms(ms)))
}

/// "shared", "exclusive" or "light", plus "in 12" for a task running under task 12's lease.
fn kind_cell(state: &State, task: &Task, color: bool) -> String {
    let label = match task.spec.kind {
        _ if task.spec.light => paint("light", "2", color),
        Kind::Shared => paint("shared", "36", color),
        Kind::Exclusive => paint("exclusive", "35", color),
    };
    match state.lease_chain(task).first() {
        Some(p) => format!("{label} in {p}"),
        None => label,
    }
}

fn paint(s: &str, code: &str, color: bool) -> String {
    if color {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

fn bold(s: &str, color: bool) -> String {
    paint(s, "1", color)
}

fn dim(s: &str, color: bool) -> String {
    paint(s, "2", color)
}

/// Left-aligned columns, sized to fit. The last column isn't padded.
struct Table {
    header: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    fn new(header: &[&str]) -> Table {
        Table {
            header: header.iter().map(|h| h.to_string()).collect(),
            rows: Vec::new(),
        }
    }

    fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    fn render(&self, color: bool) -> String {
        let width = |s: &str| strip_ansi(s).chars().count();
        let mut widths: Vec<usize> = self.header.iter().map(|h| width(h)).collect();
        for row in &self.rows {
            for (w, cell) in widths.iter_mut().zip(row) {
                *w = (*w).max(width(cell));
            }
        }
        let line = |cells: &[String]| {
            let mut s = String::from(" ");
            for (i, cell) in cells.iter().enumerate() {
                s.push(' ');
                s += cell;
                if i + 1 < cells.len() {
                    s += &" ".repeat(widths[i] - width(cell) + 1);
                }
            }
            s.push('\n');
            s
        };
        // Fit the last column (the free-form name) to the terminal, if there is one.
        let used: usize = 2 + widths[..widths.len() - 1].iter().map(|w| w + 2).sum::<usize>();
        let fit = |cells: &[String]| -> Vec<String> {
            let mut cells = cells.to_vec();
            if let (Some(cols), Some(last)) = (terminal_width(), cells.last_mut()) {
                let room = cols.saturating_sub(used).max(12);
                if width(last) > room {
                    *last = strip_ansi(last).chars().take(room - 1).collect::<String>() + "…";
                }
            }
            cells
        };
        let mut out = dim(&line(&self.header), color);
        for row in &self.rows {
            out += &line(&fit(row));
        }
        out
    }
}

fn terminal_width() -> Option<usize> {
    if !std::io::stdout().is_terminal() {
        return None;
    }
    // SAFETY: TIOCGWINSZ fills a winsize struct.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0;
    (ok && size.ws_col > 0).then_some(size.ws_col as usize)
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}
