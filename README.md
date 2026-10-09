(project is 100% vibe coded btw)

# ception

`ception` lets an agent harness (built for Claude Code, usable from others)
run OpenAI Codex as a named, long-lived subagent. Each label maps to one
daemon that owns a `codex app-server` child and one Codex thread. Thin CLI
calls talk to that daemon over a unix socket and exit when the turn
completes, so a harness's background-shell wakeup is the synchronization
mechanism.

## Install

`flake.nix` packages the CLI (Linux only: process tracking uses `/proc` and
pidfds). `nix build` runs the test suite as its check phase. With
home-manager:

```nix
inputs.ception.url = "github:xzrq-net/ception";
# ...
home.packages = [ inputs.ception.packages.${pkgs.stdenv.hostPlatform.system}.default ];
```

The default codex command is `npx -y @openai/codex app-server`; the package
appends its own node to PATH so `npx` resolves.

Nothing loads the agent-facing guide automatically. `ception skill` prints it
([SKILL.md](SKILL.md)); point the agent at it from its own instructions, e.g.
in `~/.claude/CLAUDE.md`: "To delegate work to Codex, use `ception`; run
`ception skill` before first use and again after compaction."

## Usage

```sh
ception spawn worker "inspect this repo"
ception send worker "continue with the fix"
ception send worker - < long-prompt.md
ception goal worker - < arc.md
ception goal worker --resume
ception interrupt worker
ception list
ception quota
ception watch worker
ception kill worker
ception skill
```

`spawn` starts a fresh Codex thread under the label; it refuses a label whose
daemon is live. `send` reuses the live daemon, or respawns it if it died: the
daemon resumes the label's recorded thread with its original `--model` and
`--effort`. The daemon decides atomically what a `send` means: with a turn
running it steers that turn and the sender exits at once (the client that
started the turn still gets the report); idle, it starts a new turn and
blocks. Prompts are the remaining words, or stdin for a lone `-`.

Both `spawn` and `send` print the log path on their first stdout line.

Flags on `spawn`: `--model`, `--effort`; model settings default to
`~/.codex/config.toml`. `spawn`, `send`, `goal` and `watch` take
`--report brief|items|full` and `--timeout SECS`; every command takes `--cwd`.
Labels are scoped by the project root resolved from the invocation directory,
so a label spawned with `--cwd` must be addressed with the same `--cwd` (or
from inside that project).

Codex always runs with full access and approvals disabled; ception is meant
for environments where the harness itself runs unsandboxed. If Codex sends an
approval request anyway, the daemon rejects it and fails the turn (exit 4).

Report levels: `brief` (final message and a status/files/tokens/duration
footer, the default), `items` (adds one line per command, edit and tool
call), `full` (everything the log gets, reasoning included). The final
message is never truncated. The log receives every item but caps the bulky
ones (reasoning at 4000 characters, command output at 1600): a full trace,
not a full transcript.

`watch` attaches to the daemon and blocks until the current turn completes,
delivering that turn's report and exit code as a spawn/send client would; it
is how to reattach when that client was killed. Its report level is its own
`--report`. Idle daemon: prints `no active turn`, exits 0. No daemon: exit 4
(`send` respawns and resumes). `watch --follow` tails the raw log instead.

Exit codes: `0` turn completed (or steer/interrupt accepted), `2` turn
failed, `3` turn interrupted, `4` usage or infrastructure error, `5` still
running after `--timeout`.

### Harnesses without background shells

A blocking call that outlives the harness's tool timeout gets killed; the
daemon and its turn carry on. `--timeout SECS` makes that explicit: the client
waits at most SECS (or until the turn has started, if that takes longer), then
prints `still running: run N; reattach with ception watch LABEL --run N` and
exits 5. `--timeout 0` returns as soon as the turn is running. `watch --run N`
blocks on that run if it is still going and otherwise delivers its retained
report (the daemon keeps the last 16). Failures before the turn starts are
reported directly, as without the flag.

## Goals: runs codex drives itself

A thread goal makes codex start turn after turn by itself until the objective
is met:

```sh
ception goal audit "<the arc: what done looks like>"   # set, and block on the run
ception goal audit - < arc.md                           # objective from stdin
ception goal audit --resume                             # restart a stopped goal
ception goal audit --pause                              # stop starting new turns
ception goal audit --show                               # objective and status
ception goal audit --clear
```

Setting an objective, and `--resume`, behave like `spawn`: print the log path,
block until the run settles, deliver one report covering the whole run. While
a goal is `active` the daemon holds the report across turn boundaries
(`CEPTION_GOAL_GRACE_MS` is the stall safety net). Set during a running turn,
the objective is folded into that turn. The other forms answer at once. A
goal alone can start a label that has no thread yet, on the default model;
`spawn` first to choose one.

Every turn report ends with a goal line, and `ception list` has a `goal=`
column:

- `active`: codex will start another turn.
- `complete`: the objective is met. The only status that means done.
- `paused`, `blocked`, `usageLimited`, `budgetLimited`: stopped short. A turn
  error blocks the goal (`error code:` in the footer names it), and the
  report prints the `--resume` command that restarts the run.

Resuming keeps the same daemon, app-server and thread, so codex's background
shells and subagents survive the stop; a stopped goal leaves the daemon idle,
so `CEPTION_IDLE_TIMEOUT_SECS` (4h) is the real resume deadline.

`ception interrupt` pauses an active goal before interrupting, even with no
turn running; otherwise freeing the thread would just start the goal's next
turn.

## Quota

`ception quota` reports the account's rate-limit windows, the quota part of
codex's interactive `/status`:

```
primary              7d   47% used, resets in 5d 22h (2026-08-15 20:34Z)
secondary            not reported
GPT-5.3-Codex-Spark  7d   0% used, resets in 7d 0h (2026-08-16 22:27Z)
credits              none
```

`primary`/`secondary` are the server's own slots (OpenAI reshuffles which real
window sits in each, hence per-line lengths). Per-model limits, credits, and
any limit reached get rows when reported; `--json` prints the raw response.
Answered by a throwaway app-server: no label, no daemon, no tokens.

## Scoping: project × session

Labels are namespaced by project root and session.

- The project root is the nearest ancestor of the invocation directory with a
  `.jj`, `.git` or `.hg`; without one, the directory itself. `--cwd` changes
  the starting point.
- The session is `CEPTION_SESSION` if set, else Claude Code's
  `CLAUDE_CODE_SESSION_ID`, else `default`. Claude Code keeps the session id
  across `--resume` (with a new process), so a resumed session finds its
  labels and `send` revives their threads. A nested `claude` gets its own.
  Other harnesses share one `default` session per project unless they set
  `CEPTION_SESSION`.
- A daemon watches the process in `CEPTION_WATCH_PID`, else `CLAUDE_PID`, and
  exits when it dies. Without either it lives until its idle timeout.

Another session's labels are invisible to `send`, `interrupt`, `kill` and
`watch`; `send` to a label only another session has fails naming that
session. Taking it over is deliberate: run with `CEPTION_SESSION=<that id>`.
`list` shows every session's labels for the project (`list --all` for every
project), with a `session` column of `mine` or the session id. `kill --all`
stops the calling session's daemons in the project.

## Continuations

Codex can start a turn on its own: an active goal's next turn, or the
continuation of a compacted turn on older app-servers. While a client waits,
the daemon folds such turns into one report (compacted turns get the shorter
`CEPTION_CONTINUATION_GRACE_MS` hold). A turn that arrives after everything
settled is still tracked, without clients: `list` shows the label active and
`watch` can attach, so an in-flight turn is never invisible.

If a compacted turn ends with nothing but codex's `Instructions loaded for
<path>.` acknowledgement, the report is marked failed (exit 2) with a warning:
work done before the compaction is on disk but unreported, and `send` resumes
it.

## Daemon lifecycle

The client starts a daemon by re-executing itself with a double fork, so the
daemon is reparented before the client blocks on the turn. Killing that
client, including a kill of its whole process tree (what Claude Code does to
a background shell), costs only the report. The daemon answers the client on
a readiness pipe: ready, busy (another daemon holds the label), or the
startup error.

A daemon holds its label's lock (`flock`) for its whole life; that is the
one-daemon-per-label guarantee, and the kernel releases it however the daemon
dies. It owns the app-server's process group and takes it down on exit,
codex's shells and subagents included, before releasing the lock.

A daemon exits when:

- its watched process dies (a pidfd, so immediately); it interrupts any
  running turn first;
- it has had no turn, client or pending work for `CEPTION_IDLE_TIMEOUT_SECS`
  (default 14400); `list`/`watch` probes don't count as use;
- `ception kill LABEL` or `kill --all` asks it to.

Daemon death is cheap: thread history persists in Codex's own rollout store,
and the next `send` respawns and resumes.

## Files

Under `${XDG_STATE_HOME:-~/.local/state}/ception-rs/`:

- `projects/<projhash>/<session>/<label>.json`: the label's record (project
  path, thread id, model, effort); its mtime is "last used". Written only by
  the label's daemon while it holds the lock.
- `projects/<projhash>/<session>/<label>.log`: the turn log (tail with
  `ception watch --follow LABEL`).
- `run/<key>.lock`, `run/<key>.sock`: label locks and sockets, `key` a hash of
  project, session and label. Kept under the state root, not
  `$XDG_RUNTIME_DIR`, so containers on one kernel sharing the state root share
  the locks and can reach each other's daemons.

Labels of other sessions idle longer than `CEPTION_GC_DAYS` (7) with no live
daemon are deleted (record and log) on the next spawn/send/goal/list in that
project, under the label lock.

## Environment variables

- `CEPTION_SESSION`, `CEPTION_WATCH_PID`: session and watched process; see
  Scoping.
- `CEPTION_CODEX_CMD`: the app-server command line (default
  `npx -y @openai/codex app-server`); tests substitute a fake.
- `CEPTION_IDLE_TIMEOUT_SECS`: daemon idle timeout, default 14400.
- `CEPTION_SPAWN_TIMEOUT_SECS`: daemon startup bound, default 120 (the first
  spawn may sit through an npx download).
- `CEPTION_GC_DAYS`: age before other sessions' idle labels are collected,
  default 7.
- `CEPTION_GOAL_GRACE_MS`: with an active goal, how long a completed turn
  waits for codex's follow-on turn before settling anyway, default 30000. A
  goal status change settles it sooner.
- `CEPTION_GOAL_START_MS`: how long `goal`/`--resume` waits for codex to start
  the goal's turn before answering with the goal state, default 30000.
- `CEPTION_CONTINUATION_GRACE_MS`: the same for compacted turns, default 2000.

## Development

`nix develop` provides the toolchain. `cargo test` runs unit tests and the
integration suite (`tests/cli.rs`), which drives the real binary against
`src/bin/ception-fake-appserver.rs`, a scriptable stand-in for the app-server
(behaviors chosen by `CEPTION_FAKE_BEHAVIOR`); no network or codex auth
needed. `scripts/smoke.sh` is a manual end-to-end check against real codex.

Daemons that are already running keep the code they started with until they
exit; new invocations get the new build.
