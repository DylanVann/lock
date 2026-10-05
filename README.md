# lock

A machine-wide queue for CPU-heavy work, so agents running in different repos don't
slow each other down or skew each other's benchmarks.

Put `lock` in front of a command:

```sh
lock -t 5m --name Build bun run build                # shared: runs alongside other shared tasks
lock -x -t 10m --name "Bench parser" cargo bench     # exclusive: waits until nothing else runs
lock -l -t 1h --name "Dev server" bun run dev        # light: no slot, paused while an exclusive task runs
```

- **Shared** (default) is for builds and tests. Up to `N` run at once (`lock slots N`,
  default `max(2, cpus/3)`).
- **Exclusive** (`-x`) is for benchmarks and profiling. It waits until nothing is
  running, and nothing else starts until it finishes.
- The queue is strict first-come first-served. Once an exclusive task is at the front,
  shared tasks queued behind it wait, so it can't be starved.
- **Light** (`-l`) is for long-lived, mostly idle commands: dev servers, debugging hosts,
  watchers. They take no shared slot, and instead of holding up an exclusive task they
  are paused (SIGSTOP to their process group) while it runs and resumed (SIGCONT)
  afterwards. Their timeout doesn't run while they're paused.
- Every task has a timeout: its maximum run time, counted from when the command starts,
  not while it waits. Pass your estimate with `-t/--timeout` (`90s`, `10m`, `1h`). It
  can be left off for quick commands, which get a short default of 5s
  (`lock default-timeout` changes it machine-wide). When the time runs out, the
  command's whole process group gets SIGTERM, then SIGKILL 5s later, and `lock` exits
  with 124. If it was the default that ran out, `lock` says so and suggests passing `-t`.
- A `lock` inside a locked command (a script that locks each of its steps, run under
  `lock` itself) works under the outer command's lease: `lock` sets `LOCK_TASK` in
  the command's environment, and a nested `lock` whose ancestor is that task's command
  starts straight away, without queueing again or taking another slot. A nested `-x`
  inside a shared task waits only until nothing but its outer tasks runs, and holds the
  rest of the queue back meanwhile. If a second one asks from inside a different running
  task, each would wait for the other's outer task, so the second is refused (exit 75)
  and should run its outer command with `-x`. `lock status` shows nested tasks as
  `shared in 12`.
- Shared tasks share a machine-wide **jobserver** (see below), so concurrent builds split
  the machine's cores between them instead of each starting one job per core.
- Progress bars compare a running task with its usual run time: the median of its last
  5 successful runs (same repo and name). `-e/--estimate 3m` sets it instead, for a
  first run or a test.
- `-w/--wait-timeout 5m` gives up if the lock isn't acquired in time (exit 75).
- When the command exits, anything it left behind in its process group (a background
  job it didn't wait for) gets SIGTERM, then SIGKILL 5s later, so it doesn't keep using
  the CPU after the lock is released. Processes that left the group with `setsid`, such
  as build daemons, are out of reach and are meant to outlive the command anyway.
- Otherwise `lock` passes through the command's exit code. Stdin, stdout and stderr are
  passed straight through, and when run from a terminal the command gets the terminal
  (Ctrl-C works as usual).
- To run a program that shares a name with a subcommand (`status`, `watch`, `cancel`,
  `slots`), use `lock -- status`.

Each entry records the name, kind, agent, cwd, git repo and branch, command, PIDs,
timeout, and when it was queued, started and finished.

## Viewing the queue

```sh
lock status         # or plain `lock`
lock watch          # live view
lock status --json  # full state, for scripts and agents
lock-gui            # GPUI window
open macos/build/Lock.app  # native AppKit app (see below)
lock cancel 12      # stop a running task / remove a waiting one (exit 125)
lock slots 4        # change how many shared tasks may run at once
lock default-timeout 10s   # change the timeout for commands without -t (default 5s)
lock jobs 8         # size of the shared jobserver (default: one per core; "off" to disable)
```

```
RUNNING · 2/2 shared slots in use
  ID  KIND    RUNNING  LEFT   WAITED  AGENT        WHERE      PID    NAME
  1   shared  1m32s    28s    0s      claude-code  web@main   48038  Build web app
  2   shared  40s      -      12s     codex        api@fix    48044  cargo test

QUEUE · 1 waiting
  #  ID  KIND       WAITING  TIMEOUT  AGENT        WHERE      PID    NAME
  1  3   exclusive  45s      10m00s   claude-code  lock@main  48041  Bench parser
```

## Jobserver

Slots limit how many builds run at once, but a build decides for itself how many jobs to
start, and most start one per core. Three concurrent `ninja` builds on a 10-core machine
can run 36 compiler processes between them.

So shared tasks also get a machine-wide [GNU make jobserver](https://www.gnu.org/software/make/manual/html_node/Job-Slots.html):
a FIFO holding one token per core. `lock` points the command at it through `MAKEFLAGS`
(`--jobserver-auth=fifo:~/.local/state/lock/jobserver-N.fifo`). Tools that speak the protocol
take a token before starting each job and return it when the job finishes, so every
build running at once shares the same pool of jobs:

- **Supported:** ninja 1.13+, GNU make 4.4+, cargo (and rustc), and GCC/LLVM LTO. Tools
  that don't speak it (bun, tsc, go, xcodebuild, macOS's make 3.81) ignore it; slots
  still limit them.
- **An explicit `-j` opts out.** An explicit job count makes ninja and make ignore the
  jobserver and run that many jobs regardless. That includes `cmake --build --parallel N`
  and `CMAKE_BUILD_PARALLEL_LEVEL`, which cmake passes on as `-j`. For ordinary builds,
  leave it out. Keep it when each job is itself multithreaded, such as a target that runs
  test suites: a suite would take one token but use several cores, so a small fixed count
  limits it better. `lock` prints a note when it sees one.
- **Implicit job:** each tool runs one job without a token, so N concurrent builds can
  run up to N jobs more than the pool size.
- **Where it applies:** only top-level shared tasks get it. A nested `lock` inherits its
  outer task's jobserver, an exclusive task has the machine anyway, and a light task can
  be paused while holding tokens. A jobserver set up around `lock` (say, `lock` in a
  make recipe) is left in place.
- **Measured:** two 30-job ninja builds that run 24 jobs at once on their own ran at most
  6 at once under `lock jobs 4`. A cargo build limited to 2 jobs ran at most 3 `rustc`.

There's no daemon, and a FIFO only holds its tokens while some process has it open.
Every running task using the pool keeps it open, and when none is running, the next task
creates a new pool with a full set of tokens. A tool killed with SIGKILL (a timeout, a
cancel, a crash) loses the tokens it held. So when a task using the pool ends that way,
the pool is retired: tasks still using it carry on, and newcomers start a fresh pool.
`lock jobs` and the `lock status` header show how many tokens are free.

## How it works

There is no daemon. The queue is `~/.local/state/lock/state.json` (override the directory with
`LOCK_DIR`). Every change happens under an exclusive `flock` on `~/.local/state/lock/state.lock`,
and the file is replaced atomically, so viewers can read it without locking.

Whoever holds the lock also does the scheduling: it drops entries whose `lock` process
has died (checked by PID and process start time, so a reused PID doesn't count), kills
any command such a process left behind, and hands out free slots in queue order. Waiting
`lock` processes poll every 250ms. Nothing needs to be running for this to work, and a
crashed or `kill -9`'d agent can't hold the lock forever.

While a command runs, its `lock` process keeps a small guard process (`lock __guard`,
in its own session). If `lock` dies without cleaning up (`kill -9`, a crash, a killed
agent), the guard stops the command's process group within a fraction of a second and
frees its slot, instead of leaving the command running without the lock until the next
`lock` invocation notices.

As a backstop, if a task is still running 30s past its deadline (say its `lock` process
is stopped), the next process to touch the queue kills it.

The agent name comes from `--agent`, then `$LOCK_AGENT`, then detection
(`CLAUDECODE` → `claude-code`, and similar for Codex, Cursor, Gemini, OpenCode).

## Install

```sh
./install.sh   # `lock` and `lock-gui` into ~/.local/bin, and on macOS Lock.app into /Applications
```

Rerun it to update; a running Lock app is quit and reopened. Set `BIN_DIR` or `APP_DIR` to
install somewhere else.

Building the GUI needs Xcode's Metal toolchain
(`xcodebuild -downloadComponent MetalToolchain`).

## Native macOS app

`macos/` contains an alternative UI written in Objective-C with plain AppKit. The window
lists running, queued and recent tasks, with progress against each task's usual run time,
Stop and Show in Finder buttons, and a right-click menu (Copy Command, Copy PID). A menu
bar extra shows what's running and queued, live, with a spinner per running task. The
Settings window (⌘,) sets shared slots, the jobserver and the default timeout.

```sh
macos/build.sh   # builds macos/build/Lock.app; ./install.sh also installs it
```

It needs only the Xcode command line tools, not an Xcode project. The app reads
`state.json` directly, refreshing as soon as the directory changes and ticking once a
second. Stop and slot changes run the `lock` CLI, which the build copies into the app
bundle.

## Instructions for agents

Add something like this to your agent instructions (`CLAUDE.md`, `AGENTS.md`):

```md
## CPU-heavy commands

This machine is shared by several agents. Prefix CPU-heavy commands with `lock`:

- Builds, test suites, type checks of big projects: `lock -t <estimate> --name "<what>" <cmd>`
- Benchmarks, profiling, anything timing-sensitive: `lock -x -t <estimate> --name "<what>" <cmd>`

`-t` is your estimate of the longest the command should run (e.g. `-t 10m` for a build
that usually takes 3). It's killed after that and `lock` exits with 124. You can leave
`-t` off for quick commands (under 5 seconds).
`lock` waits for its turn, then runs the command and passes through its exit code.
Run `lock status` to see what's running and queued. Don't wrap quick commands.
```

Instructions are advisory. [docs/agent-enforcement.md](docs/agent-enforcement.md)
proposes enforcing them with pre-command hooks in Claude Code, Codex and Cursor.
