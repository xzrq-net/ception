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
symlink). One symlink activates it:

```sh
ln -s ~/projects/ception ~/.claude/skills/ception
```

It loads on the next session as `ception@skills-dir`, discovered in place —
edits to the checkout take effect without reinstalling (`/reload-plugins`
picks up changes to non-skill components). For shells outside Claude Code,
also symlink `bin/ception` somewhere on your PATH.

## Usage

```sh
ception spawn --label worker "inspect this repo"
ception send worker "continue with the fix"
ception send worker - < long-prompt.md
ception interrupt worker
ception list
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

Report levels: `brief` (final message + status/files/tokens footer, default),
`items` (adds one line per command/edit/tool call), `full` (everything the log
gets, including reasoning). The log file always receives the full stream.

Exit codes: `0` turn completed (or steer/interrupt accepted), `2` turn failed,
`3` turn interrupted, `4` usage or infrastructure error.

## Scoping: project × session

Labels are namespaced by **project root** and **session**:

- The project root is found by walking up from the invocation directory to the
  nearest `.jj`/`.git`/`.hg`; without one, the directory itself is the root.
  Running `ception` from a subdirectory therefore hits the same labels as
  running it at the root. `--cwd` overrides the starting point.
- The session is the Claude Code process the client runs under (pid +
  starttime). Two concurrent Claude Code sessions in the same project can both
  use the label `impl` and get independent daemons and independent Codex
  threads — no shared rollout, ever. Outside Claude Code, all invocations
  share the `default` session.

**Adoption.** When `send` doesn't find the label in its own session, it looks
at other sessions' entries for the project. If the owning session is dead
(typical after exiting and resuming Claude Code), the label is moved into the
current session and its thread resumed — options, thread history, and log file
carry over. If the owner is still alive, `send` refuses with exit code 4
rather than share the rollout.

`interrupt` and `kill` act only on the calling session's daemons; `kill --all`
kills the calling session's daemons for the current project. `list` shows all
sessions' labels for the project (`list --all` for every project), with a
`session` column of `mine`, a live session key, or `adoptable`. `watch` may
tail any session's log.

## Daemon lifecycle

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
  with `ception watch LABEL`)
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
- `CEPTION_WATCH_PID` / `CEPTION_WATCH_STARTTIME` — bypass ancestor detection
  and watch this process instead (used by tests; also pins the session key).

## Development

`npm test` runs the suite against `test/fake-appserver.mjs`; no network or
codex auth needed. `scripts/smoke.sh` is a manual end-to-end check against
real codex.
