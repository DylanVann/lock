# Enforcing `lock` in coding agents

**Status: proposal.** Nothing in this document is implemented yet. It describes how
agents could be made to use `lock`, rather than only asked to.

## Why

Telling agents to use `lock` in `CLAUDE.md` or `AGENTS.md` works most of the time.
It fails in the places that matter: long sessions where the instruction has scrolled
out of attention, subagents that never saw it, and agents that decide this one build is
small enough. One unwrapped `cargo build --release` during someone's benchmark is
enough to spoil the numbers.

Claude Code, Codex and Cursor can all run a program before each shell command the agent
wants to execute, and veto the command with a message that goes back to the model.
That's the enforcement point: catch CPU-heavy commands that aren't under `lock`, and
tell the agent exactly what to run instead.

## How it works

```
agent wants to run:  cargo build --release
        │
        ▼
harness runs the hook:  lock hook claude   (tool call JSON on stdin)
        │
        ├─ not heavy, or already under lock ──▶ allow, command runs as usual
        │
        └─ heavy and unwrapped ──▶ deny, with a message to the agent:
              "This machine is shared; run CPU-heavy commands under `lock`.
               `cargo build --release` usually takes ~3m here. Run:
                 lock -t 10m --name "cargo build --release" cargo build --release"
        │
        ▼
agent retries with the suggested command ──▶ hook sees `lock` in front ──▶ allow
```

The cost is one extra round trip the first time an agent forgets, and the message
teaches it the pattern for the rest of the session. Because `lock` already records
how long each task usually takes (see `run_times` in the state file), the message can
suggest a timeout, so the agent doesn't have to estimate one.

## One hook for every agent: `lock hook`

Each agent sends a different JSON shape and expects a different reply, but the
decision is the same. The proposal is a subcommand that does both:

```sh
lock hook claude    # Claude Code PreToolUse protocol
lock hook codex     # Codex PreToolUse protocol (same shape as Claude Code's)
lock hook cursor    # Cursor beforeShellExecution protocol
```

It reads the event from stdin, pulls out the command string and working directory,
classifies the command (below), and prints the reply in that agent's format. The
classifier is shared, so a rule added once applies to every agent.

For testing rules without an agent:

```sh
$ lock check 'cd web && bun run build 2>&1 | tail -20'
deny: `bun run build` is heavy (shared) and not under lock
suggest: cd web && lock -t 5m --name "bun run build" bun run build 2>&1 | tail -20

$ lock check 'lock -t 5m --name Build bun run build'
allow: already under lock

$ lock check 'git status'
allow: not heavy
```

## What counts as heavy

A small built-in rule list, extended by `~/.config/lock/rules.toml` and by a
`.config/lock.toml` in the repo (for project-specific entry points like `just ci`).
Rules are regular expressions matched against each simple command (see the next
section), and say what kind of task it should be:

```toml
# Builds and test suites: shared
[[rule]]
match = '^cargo (build|b|test|t|check|c|clippy|doc|run|r|nextest)\b'
kind = "shared"

[[rule]]
match = '^(make|ninja|bazel (build|test)|xcodebuild|swift (build|test)|go (build|test))\b'
kind = "shared"

[[rule]]
match = '^(bun|npm|pnpm|yarn) (run )?(build|test|typecheck|lint)\b'
kind = "shared"

# Benchmarks and profiling: exclusive
[[rule]]
match = '^(cargo bench|hyperfine|go test .*-bench|(bun|npm|pnpm) (run )?bench)\b'
kind = "exclusive"

# Long-lived, mostly idle: light
[[rule]]
match = '^((bun|npm|pnpm|yarn) (run )?dev|cargo watch|vite)\b'
kind = "light"
```

Rules are checked in order and the first match wins, so the more specific `cargo
bench` rule must come before a general `cargo` one. Anything that matches no rule is
allowed. The goal is to catch the common expensive commands, not to be a sandbox. A
missed command costs a little CPU contention; a false positive costs the agent a
retry and some patience.

## Reading shell command strings

Agents send whole shell strings, not argument lists:

```
cd web && BUN_ENV=ci bun run build 2>&1 | tail -20
```

The classifier splits the string into simple commands at `&&`, `||`, `;`, `|` and
newlines, then for each one skips leading environment assignments (`FOO=1`) and
transparent wrappers (`env`, `time`, `nice`, `command`, `timeout 600`) to find the
program and its arguments. A heavy command is fine if it's under `lock`, meaning
`lock` is the program of that same simple command.
Both of these are allowed:

```sh
cd web && lock -t 5m --name Build bun run build 2>&1 | tail -20
lock -t 5m --name Build sh -c 'cd web && bun run build'
```

When the string is too complex to read confidently (heredocs, subshells, `$(...)`,
`eval`), the hook allows it. It should also never block because of its own error.
Failing open keeps a classifier bug from wedging every agent on the machine.

The splitter should come from a real shell parser, such as a POSIX shell grammar crate,
not from regular expressions over the raw string. Otherwise quoting
(`echo "cargo build"`) produces false positives.

## Deny or rewrite?

Claude Code and Codex let a hook replace the command (`updatedInput`), so the hook
could silently wrap it in `lock`. Cursor can only allow, deny, or ask. The recommended
default is to **deny with a suggested command**, even where rewriting is possible:

- **The agent should choose the timeout.** A rewrite has to guess `-t`, and a wrong
  guess kills a build partway through. The suggestion can include the usual run time
  from history, so the agent's estimate is cheap to make.
- **The agent should know it's queued.** A silently rewritten command might sit in
  the queue for minutes with no explanation, and the agent may assume it hung and kill
  it. Having typed `lock` itself, it knows why it's waiting (`lock` prints its queue
  position to stderr).
- **The name is better from the agent.** "Build web app" is more useful in
  `lock status` than the command line.

A `--rewrite` mode is still useful where a task has history. It would wrap the
command with a timeout of `max(3 × usual, 2m)` and the rule's kind, and add a short
note to the agent explaining what happened.

## Agent tool timeouts

Time spent waiting in the queue counts against the agent's own timeout for the
shell command. If Claude Code's Bash tool gives up after its default 2 minutes while
`lock` is still queued, the harness kills `lock`. The task leaves the queue ("gave
up") and the agent sees a timeout, not a build.

The deny message should therefore tell the agent to allow for waiting as well as
running: raise the tool's timeout (Claude Code's Bash tool accepts one, up to 10
minutes), or run long commands in the background and check back. `lock status --json`
could also expose the expected wait: the usual run time of everything ahead in the
queue. The hook could then say "the queue is about 4 minutes deep right now".

Claude Code passes the tool's timeout to the hook (`tool_input.timeout`). So even an
already-wrapped command can get a warning when the queue wait plus the `-t` estimate
clearly won't fit, suggesting `run_in_background` instead.

## Setup per agent

All three read the hook's JSON reply from stdout. Use the absolute path to `lock`,
since hooks don't necessarily run with your shell's `PATH`. The examples assume
`./install.sh` put it in `~/.local/bin`; replace `/Users/you` with your home directory.
Configure the hooks at user level so they apply in every repo.

### Claude Code

In `~/.claude/settings.json`:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          { "type": "command", "command": "/Users/you/.local/bin/lock hook claude", "timeout": 10 }
        ]
      }
    ]
  }
}
```

- **Input:** the hook gets `cwd` and `tool_input.command` on stdin. It also gets
  `tool_input.timeout` (in ms) and `tool_input.run_in_background`, which let it check
  that the agent will wait long enough (see [Agent tool timeouts](#agent-tool-timeouts)).
- **Denying:** it prints the following. The reason is fed back to Claude, which can
  retry.
  ```json
  {
    "hookSpecificOutput": {
      "hookEventName": "PreToolUse",
      "permissionDecision": "deny",
      "permissionDecisionReason": "This machine is shared; run CPU-heavy commands under `lock`. ..."
    }
  }
  ```
- **Warn mode:** instead of denying, it returns `additionalContext` with the same
  text.
- **Rewrite mode:** returns `updatedInput: { "command": "lock ... cargo build" }`. The
  rewritten command still goes through Claude Code's permission checks. If several
  PreToolUse hooks rewrite the same call, the last to finish wins, so keep this the
  only rewriting Bash hook.
- **Loading:** hooks are read when a session starts, so start a new session after
  adding it. `/hooks` lists what's loaded, and `claude --debug` logs each hook run with
  its output.
- **Other scopes:** the same block works in a project's `.claude/settings.json` or in a
  plugin's `hooks/hooks.json`. Hooks from all scopes are combined, not overridden.

### Codex

Codex uses the same event name, matcher style and reply format as Claude Code, so
`lock hook codex` differs from `lock hook claude` only in how it labels its log lines.
In `~/.codex/config.toml`:

```toml
[[hooks.PreToolUse]]
matcher = "^Bash$"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "/Users/you/.local/bin/lock hook codex"
timeout = 10
```

(`~/.codex/hooks.json` takes the same structure as JSON.)

- **Enabling:** hooks are on by default in current versions. Older versions needed
  them enabled under `[features]` in `config.toml`.
- **Input:** the command is in `tool_input.command`, with `cwd` alongside.
- **Replies:** denying uses the same `hookSpecificOutput` block as Claude Code (exit
  code 2 with a message on stderr also works). Rewriting uses `updatedInput`, which for
  Bash must contain a string `command`.
- **Ordering:** matching hooks run concurrently, so another hook can't be relied on to
  run before or after this one.

### Cursor

In `~/.cursor/hooks.json`:

```json
{
  "version": 1,
  "hooks": {
    "beforeShellExecution": [
      { "command": "/Users/you/.local/bin/lock hook cursor", "timeout": 10 }
    ]
  }
}
```

- **Input:** the hook receives `command` and `cwd` at the top level of its JSON input.
- **Replies:** it answers with `permission`, plus `agent_message` for the model and
  `user_message` for you:
  ```json
  {
    "permission": "deny",
    "agent_message": "This machine is shared; run CPU-heavy commands under `lock`. ...",
    "user_message": "Blocked `cargo build`: not under lock"
  }
  ```
- **No rewriting:** Cursor has no way for this hook to rewrite the command, so
  `--rewrite` falls back to deny. Warn mode returns `"permission": "allow"` with an
  `agent_message`.
- **Invalid output blocks:** Cursor blocks the command whenever the hook prints
  invalid JSON, even when failing open is configured. `lock hook cursor` must
  therefore catch its own errors and still print `{"permission": "allow"}`.
- **Other scopes:** project hooks live in `<repo>/.cursor/hooks.json`, and managed
  hooks in `/Library/Application Support/Cursor/hooks.json`.

### Other agents

Agents without a pre-command hook keep relying on the instructions in `AGENTS.md`,
optionally backed by PATH shims (next section).

## Without hooks: PATH shims

For agents without hooks, or for scripts that call build tools directly, the fallback
is a directory of shims that comes first on the agent's `PATH` only (set in the
agent's environment, not your shell profile). Each shim, such as `cargo`, looks up the
real binary further down `PATH` and re-runs itself under `lock` when the arguments
match a rule.

Shims work with any harness and catch nested calls, but they are blunter than hooks:

- **No one to pick the timeout.** The shim has to use history or a generous default.
- **The command runs without explanation.** The agent sees a delay but never learns
  about `lock`.
- **They're easy to get subtly wrong.** A shim must find the real binary without
  finding itself, and must pass through quick invocations like `cargo --version`
  untouched.

Hooks are the better first step; shims are for the gaps.

## Escape hatches

- **Per command:** prefix with `LOCK_SKIP=1`, e.g. `LOCK_SKIP=1 cargo build`. The hook
  allows the command and logs it. The variable itself is harmless to the command.
- **Per session or agent:** set `LOCK_ENFORCE=off` in the agent's environment.
- **Warn mode:** `lock hook claude --warn` never blocks. It allows the command and
  passes the same suggestion to the agent as context. This is useful while rolling out.

## Rolling it out

1. **Warn mode first.** Run `--warn` for a few days. The hook appends every decision
   to `~/.local/state/lock/hook.log`, so false positives and misses show up there.
2. **Tune the rules.** Adjust from the log, and add `.config/lock.toml` entries for
   project-specific entry points.
3. **Enforce.** Switch to deny, keeping the instructions in `AGENTS.md`/`CLAUDE.md`.
   They still help, because an agent that already knows the pattern rarely hits the
   hook.

## Open questions

- **Idle machine:** should the hook still block when the queue is empty? This proposal
  says yes. `lock` starts immediately then, so the only cost is the retry, and
  consistent behaviour teaches the habit.
- **Test runners:** they vary wildly. `cargo test` on a small crate takes seconds;
  `pytest` in a monorepo takes many minutes. Rules may need per-repo overrides more
  often than builds do.
- **Visibility:** should hook decisions show up in `lock status` and the viewers, for
  example as "blocked 3 unwrapped commands today"?

## References

- Claude Code: [hooks reference](https://code.claude.com/docs/en/hooks.md),
  [hooks guide](https://code.claude.com/docs/en/hooks-guide.md)
- Codex: [hooks](https://learn.chatgpt.com/docs/hooks)
- Cursor: [hooks](https://cursor.com/docs/hooks)
