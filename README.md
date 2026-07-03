# ception

`ception` lets Claude Code run OpenAI Codex as a named, long-lived subagent.
Each label maps to one per-project daemon that owns a `codex app-server` child
and one Codex thread. Thin CLI calls talk to that daemon over a Unix socket and
exit when the turn completes, so Claude Code's background-Bash wakeup remains
the synchronization mechanism.

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

`spawn` starts a fresh Codex thread and prints the log path on its first line.
`send` reuses the live daemon, or transparently respawns it (resuming the
stored thread and the original `--model`/`--effort`/sandbox options) if the
daemon died. If a turn is already running, `send` steers that in-flight turn
and exits immediately; the client that started the turn still blocks until
completion and delivers the report.

Flags on `spawn`: `--cwd`, `--model`, `--effort`, `--read-only` /
`--full-access` (default is a workspace-write sandbox; approvals are always
disabled), `--report brief|items|full`. Model settings default to
`~/.codex/config.toml`.

Report levels: `brief` (final message + status/files/tokens footer, default),
`items` (adds one line per command/edit/tool call), `full` (everything the log
gets, including reasoning). The log file always receives the full stream.

Exit codes: `0` turn completed (or steer/interrupt accepted), `2` turn failed,
`3` turn interrupted, `4` usage or infrastructure error.

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

## Files

- Logs: `~/.local/state/ception/logs/<cwdhash>-<label>.log` (tail with
  `ception watch LABEL`)
- State (label → thread id and options): `~/.local/state/ception/<cwdhash>.json`
- Sockets/locks: `$XDG_RUNTIME_DIR/ception/`, falling back to
  `~/.local/state/ception/run/`

## Environment variables

- `CEPTION_CODEX_CMD` — override the app-server command line (default
  `npx -y @openai/codex app-server`); used by tests to substitute a fake.
- `CEPTION_IDLE_TIMEOUT_SECS` — daemon idle timeout, default 14400.
- `CEPTION_CLAUDE_POLL_SECS` — ancestor liveness poll interval, default 30.
- `CEPTION_WATCH_PID` / `CEPTION_WATCH_STARTTIME` — bypass ancestor detection
  and watch this process instead (used by tests).

## Development

`npm test` runs the suite against `test/fake-appserver.mjs`; no network or
codex auth needed. `scripts/smoke.sh` is a manual end-to-end check against
real codex.
