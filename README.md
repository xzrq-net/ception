(project is 100% vibe coded btw)

# ception

`ception` lets Claude Code run OpenAI Codex as a named, long-lived subagent.
Each label maps to one daemon that owns a `codex app-server` child and one
Codex thread. Thin CLI calls talk to that daemon over a Unix socket and exit
when the turn completes, so Claude Code's background-Bash wakeup remains the
synchronization mechanism.

## Install

The repository is a Claude Code plugin: `.claude-plugin/plugin.json`, the
skill in `skills/ception/`, and `bin/`, which Claude Code adds to the Bash
tool's PATH while the plugin is enabled (so `ception` resolves with no extra
symlink). Clone it into the user-level skills directory:

```sh
git clone git@github.com:xzrq-net/ception.git ~/.claude/skills/ception
```

It loads on the next session as `ception@skills-dir` (`/reload-plugins`
picks up changes to non-skill components without restarting); update with
`git pull`. For shells outside Claude Code, run `bin/ception` directly or
put the clone's `bin/` on your PATH.

## Usage

```sh
ception spawn --label worker "inspect this repo"
ception send worker "continue with the fix"
ception send worker - < long-prompt.md
ception goal worker - < arc.md
ception goal worker --resume
ception interrupt worker
ception list
ception quota
ception watch worker
ception kill worker
```

`spawn` starts a fresh Codex thread. Both `spawn` and `send` print the log
path on their first stdout line. `send` reuses the live daemon, or
transparently respawns it (resuming the stored thread and the original
`--model`/`--effort` options) if the daemon died. The daemon decides atomically what a `send` means: if a turn is running
it steers that turn and the sender exits immediately (the client that started
the turn still blocks until completion and delivers the report); if idle it
starts a new turn and blocks.

Flags on `spawn`: `--cwd`, `--model`, `--effort`, `--report brief|items|full`.
Model settings default to `~/.codex/config.toml`. Every other command also
accepts `--cwd`; since labels are scoped by the project root resolved from the
invocation cwd, a label spawned with `--cwd` must be addressed with the same
`--cwd` (or from inside that project).

Codex always runs with full access and approvals disabled; `ception` is meant
for environments (dev containers) where Claude Code itself runs unsandboxed.
There is no sandbox knob. If Codex ever sends an approval request anyway, the
daemon rejects it and fails the turn with exit code 4.

Report levels: `brief` (final message + status/files/tokens/duration footer, default),
`items` (adds one line per command/edit/tool call), `full` (everything the log
gets, including reasoning). The final message is never truncated at any level.
The log file receives every item, but caps the bulky ones — reasoning at 4000
characters, command output at 1600 — so it is a full trace, not a full
transcript.

`watch` attaches to the daemon and blocks until the current turn completes,
delivering the turn's report and exit code just as a spawn/send client would —
the reattach tool when that client was killed mid-turn. The report level is
the watcher's own `--report` (default `brief`), not the original client's. On an idle
daemon it prints `no active turn` and exits 0 immediately; with no live daemon
it fails with exit 4 (recovery is `send`, which respawns and resumes).
`watch --follow` instead tails the raw log indefinitely, for humans.

Exit codes: `0` turn completed (or steer/interrupt accepted), `2` turn failed,
`3` turn interrupted, `4` usage or infrastructure error.

## Goals: runs codex drives itself

A **thread goal** makes codex start turn after turn by itself until the
objective is met — no prompt per turn:

```sh
ception goal audit "<the arc: what done looks like>"   # set, and block on the run
ception goal audit -  < arc.md                          # objective from stdin
ception goal audit --resume                             # restart a stopped goal
ception goal audit --pause                              # stop starting new turns
ception goal audit --show                               # objective + status
ception goal audit --clear
```

Setting an objective, and `--resume`, behave like `spawn`: print the log path,
block until the run settles, deliver one report covering the whole run. While
a goal is `active` the daemon holds the report across turn boundaries
(`CEPTION_GOAL_GRACE_MS` is the stall safety net). Set during a running turn,
the objective is steered into it. The other forms answer immediately. A goal
alone can start a label that has no daemon yet, on the default model and
effort; `spawn` first to choose them.

Every report ends with a goal line, and `ception list` has a `goal=` column:

- `active` — codex will start another turn.
- `complete` — the objective is met. The only status that means done.
- `paused`, `blocked`, `usageLimited`, `budgetLimited` — stopped short. A turn
  error blocks the goal (`error code:` in the footer names it), and the
  report prints the `--resume` that restarts the run.

Resuming keeps the same daemon, app-server and thread — codex's background
shells and subagents survive the stop — but a stopped goal leaves the daemon
idle, so `CEPTION_IDLE_TIMEOUT_SECS` (4h default) is the real resume deadline.

`ception interrupt` pauses an active goal before interrupting the turn;
otherwise freeing the thread would just start the goal's next turn.

## Quota

`ception quota` reports the account's rate-limit windows — the quota part of
what codex's `/status` shows interactively:

```
primary              7d   47% used, resets in 5d 22h (2026-08-15 20:34Z)
secondary            not reported
GPT-5.3-Codex-Spark  7d   0% used, resets in 7d 0h (2026-08-16 22:27Z)
credits              none
```

`primary`/`secondary` are the server's own slots (OpenAI reshuffles which
real window sits in each, hence per-line lengths). Per-model limits, credits,
and any limit reached get rows when reported; `--json` prints the raw
response. Answered by a throwaway app-server: no label, no daemon, no tokens.

## Scoping: project × session

Labels are namespaced by **project root** and **session**:

- The project root is found by walking up from the invocation directory to the
  nearest `.jj`/`.git`/`.hg`; without one, the directory itself is the root.
  Running `ception` from a subdirectory therefore hits the same labels as
  running it at the root. `--cwd` overrides the starting point.
- The session is the **outermost** Claude Code process the client runs under
  (pid + starttime). Two separately launched Claude Code sessions in the same
  project can both use the label `impl` and get independent daemons and
  independent Codex threads. Outside Claude Code, all invocations share the
  `default` session.

  Outermost, because Claude Code nests short-lived claude-looking helpers
  under the real session. The cost: a `claude` launched inside another session
  shares its parent's labels and dies with it; pin `CEPTION_WATCH_PID` /
  `CEPTION_WATCH_STARTTIME` to override.

**Adoption.** When `send` doesn't find the label in its own session, it looks
at other sessions' entries for the project. If the owning session is dead
(typical after exiting and resuming Claude Code), the label is moved into the
current session and its thread resumed — options, thread history, and log file
carry over. If the owner is still alive, `send` refuses with exit code 4
rather than share the rollout.

`interrupt` and `kill` act only on the calling session's daemons; `kill --all`
kills the calling session's daemons for the current project. `list` shows all
sessions' labels for the project (`list --all` for every project), with a
`session` column of `mine`, a live session key, or `adoptable`. `watch`
attaches only to the calling session's daemon; `watch --follow` may tail any
session's log.

## Continuations

A turn that arrives after everything has settled (a goal turn whose waiter
timed out, say) is adopted anyway, without its original clients — `list`
shows the label active and `watch` can attach — so an in-flight turn is never
invisible.

Compacted turns get the goal treatment on a shorter
`CEPTION_CONTINUATION_GRACE_MS` window. Current codex compacts within a single
turn, so this is a fallback for the cross-turn shape seen on older
app-servers. If a compacted turn ends with nothing but codex's
`Instructions loaded for <path>.` acknowledgement, the report is marked failed
(exit 2) with a warning: work done before the compaction is on disk but
unreported, and `send` resumes it. That signature came from
`experimental token_budget` clearing context on autocompaction instead of
summarising it, and should not recur.

## Daemon lifecycle

The daemon is spawned through an intermediate process that exits immediately,
so the daemon reparents to init before the spawning client blocks on the
turn. Killing that client — including a kill of its whole process tree, which
is what Claude Code does when it stops a background shell — costs only the
report; the daemon and its turn keep running.

A daemon exits when any of these fires:

- **Claude ancestor watch**: at spawn, the client locates the Claude Code
  process among its ancestors and the daemon polls it (pid + starttime, so pid
  reuse doesn't fool it). When that process dies, the daemon interrupts any
  active turn and exits. Spawned outside Claude Code, this watch is skipped.
- **Idle timeout**: no turns and no connected clients for
  `CEPTION_IDLE_TIMEOUT_SECS` (default 14400).
- **Explicit**: `ception kill LABEL` or `ception kill --all`.

Daemon death is cheap: thread history persists in Codex's own rollout store,
and the next `send` respawns and resumes.

State entries whose owning session is dead and that have been idle for
`CEPTION_GC_DAYS` (default 7) are garbage-collected on the next invocation,
along with their log files.

## Files

- Logs: `~/.local/state/ception/logs/<cwdhash>-<session>-<label>.log` (tail
  with `ception watch --follow LABEL`)
- State (session → label → thread id and options):
  `~/.local/state/ception/<cwdhash>.json`, guarded by a `.lock` file for
  cross-process read-modify-write
- Sockets/locks: `$XDG_RUNTIME_DIR/ception/`, falling back to
  `~/.local/state/ception/run/`

## Environment variables

- `CEPTION_CODEX_CMD` — override the app-server command line (default
  `npx -y @openai/codex app-server`); used by tests to substitute a fake.
- `CEPTION_IDLE_TIMEOUT_SECS` — daemon idle timeout, default 14400.
- `CEPTION_CLAUDE_POLL_SECS` — ancestor liveness poll interval, default 30.
- `CEPTION_SPAWN_TIMEOUT_SECS` — how long the client waits for a spawned
  daemon's socket, default 120 (generous because the first spawn may sit
  through an npx download).
- `CEPTION_GC_DAYS` — age before dead-session state entries and logs are
  collected, default 7.
- `CEPTION_GOAL_GRACE_MS` — with an active thread goal, how long a completed
  turn waits for codex's follow-on turn before settling anyway, default 30000.
  A goal status change settles it sooner; turns with no goal never wait.
- `CEPTION_GOAL_START_MS` — how long `ception goal`/`--resume` waits for codex
  to start the goal's turn before returning the goal state instead, default
  30000.
- `CEPTION_CONTINUATION_GRACE_MS` — same, for the compacted-turn fallback,
  default 2000.
- `CEPTION_WATCH_PID` / `CEPTION_WATCH_STARTTIME` — bypass ancestor detection
  and watch this process instead (used by tests; also pins the session key).

## Development

`npm test` runs the suite against `test/fake-appserver.mjs`; no network or
codex auth needed. `scripts/smoke.sh` is a manual end-to-end check against
real codex.

### Deploying local work

The live install is the clone at `~/.claude/skills/ception`, whose `origin`
points at this working repo. To ship commits:

```sh
jj bookmark set master -r <rev>   # point master at what should ship
git -C ~/.claude/skills/ception pull --ff-only
```

New invocations pick up `bin/` and skill changes immediately; daemons that
are already running keep their old code until they exit. `/reload-plugins`
refreshes other plugin components without restarting the session.
