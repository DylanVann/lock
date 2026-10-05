
## CPU-heavy commands

This machine is shared by several agents. Prefix CPU-heavy commands with `lock`:

- Builds, test suites, type checks of big projects: `lock -t <estimate> --name "<what>" <cmd>`
- Benchmarks, profiling, anything timing-sensitive: `lock -x -t <estimate> --name "<what>" <cmd>`
- Dev servers, watchers, debugging hosts: `lock -l -t <estimate> --name "<what>" <cmd>`

`-t` is your estimate of the longest the command should run (e.g. `-t 10m` for a build
that usually takes 3). It's killed after that and `lock` exits with 124. You can leave
`-t` off for quick commands (under 5 seconds). Don't wrap quick commands at all.

`lock` waits for its turn, then runs the command and passes through its exit code.
Waiting counts against your shell tool's timeout, so allow for the queue as well as the
run: raise the tool's timeout, or run the command in the background and check back.
`lock status` shows what's running and queued.

`-x` waits until nothing else is running and holds everything else back, so benchmarks
get the machine to themselves. `-l` is for long-lived, mostly idle commands: they take no
slot and are paused while a `-x` task runs.

Builds share a jobserver through `lock`, so don't pass `-j`/`--jobs` yourself.
