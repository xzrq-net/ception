# ception — implementation plan

Build `ception`, a CLI that lets Claude Code drive OpenAI Codex as a named,
long-lived subagent. Each subagent is a per-label daemon that owns a
`codex app-server` child process and one Codex thread. Thin CLI clients talk to
the daemon over a unix socket: they start turns, steer in-flight turns, and
block until the turn completes so that Claude Code's background-Bash mechanism
provides wake-on-completion.

## Environment and constraints

- Language: JavaScript, ESM (`.mjs`), running on ambient Node (v24). No build
  step, no TypeScript.
- Dependencies: none. Use `node:util` `parseArgs`, `node:net`, `node:test`.
- Platform: Linux only (NixOS). Shebang must be `#!/usr/bin/env node`.
- Codex is invoked as `npx -y @openai/codex app-server`. Inherit the user's
  `~/.codex/config.toml` (model, reasoning effort, custom instructions) — do
  not override model settings unless a CLI flag asks for it.
- The repo is currently empty except for `CLAUDE.md`. Create `package.json`
  (`"type": "module"`, `"bin": { "ception": "bin/ception.mjs" }`,
  `"scripts": { "test": "node --test test/" }`).

## Reference material

- App-server protocol docs: `~/src/codex/codex-rs/app-server/README.md`.
- Authoritative message schema: run
  `npx -y @openai/codex app-server generate-ts --out <scratch-dir>` and consult
  the generated types. Do not guess field names; check the schema for the exact
  shape of `thread/start`, `turn/start`, `turn/steer`, `turn/interrupt`,
  item notifications, and `turn/completed`.
- A working JSONL JSON-RPC client to crib from:
  `~/src/codex-plugin-cc/plugins/codex/scripts/lib/app-server.mjs`
  (class `SpawnedCodexAppServerClient`: spawn, readline loop, request/notify,
  initialize handshake). Ignore that repo's broker code; our lifecycle model is
  different.
- A fake-server test fixture pattern to crib from:
  `~/src/codex-plugin-cc/plugins/codex/tests/fake-codex-fixture.mjs`.

## Protocol essentials (verify against generated schema)

- Transport: newline-delimited JSON-RPC 2.0 over the child's stdio; the
  `"jsonrpc"` header field is omitted on the wire.
- Handshake: send `initialize` request with
  `clientInfo: { name: "ception", title: "ception", version: <pkg version> }`,
  then the `initialized` notification. Do NOT opt out of delta notifications —
  reasoning deltas feed the log file.
- Conversation: `thread/start` (params include `cwd`, `approvalPolicy`,
  sandbox/permissions, optional `model`) → `turn/start` with user input →
  stream of `turn/started`, `item/started`, `item/*/delta`, `item/completed` →
  `turn/completed` with final status and token usage.
- Steering: `turn/steer` injects user input into an in-flight turn. It is
  rejected for review/compaction turns — surface the RPC error verbatim.
- `turn/interrupt` cancels by `(threadId, turnId)`; the turn finishes with
  `status: "interrupted"`.
- Resume: `thread/resume` with a stored `threadId` reloads a persisted thread;
  Codex persists rollouts itself, so resume needs no state on our side beyond
  the thread id.
- Server-initiated requests (e.g. approval requests; should not occur since we
  set `approvalPolicy: "never"`): respond with a JSON-RPC error (`-32601`) and
  write a prominent warning to the log and to the active client.
- Unknown notification methods must never crash anything; log them at debug
  level as raw JSON.

## Architecture

```
claude ─ bash ─ ception send <label> ── unix socket ── ception daemon ── stdio ── npx @openai/codex app-server
                (thin client, blocks             (per label, detached,
                 until turn ends, exits)          owns thread + log file)
```

- One daemon per (project cwd, label). The daemon is spawned detached
  (`setsid`-equivalent: `spawn(..., { detached: true, stdio: ["ignore", logfd, logfd] })`,
  `unref()`), survives its spawning client, and dies per the lifecycle rules
  below.
- Clients connect to the daemon socket, send one JSON command, read a JSONL
  response stream, render to stdout, exit. The client process exiting is what
  wakes Claude Code — the daemon must never be the thing a background Bash
  waits on.

### Filesystem layout

Let `hash = first 12 hex chars of sha256(realpath(project cwd))`. Project cwd
defaults to `process.cwd()` of the client, overridable with `--cwd`.

- Sockets + lockfiles: `$XDG_RUNTIME_DIR/ception/<hash>-<label>.sock` /
  `.lock`. Fall back to `~/.local/state/ception/run/` if `XDG_RUNTIME_DIR` is
  unset.
- State: `~/.local/state/ception/<hash>.json` —
  `{ cwd, labels: { <label>: { threadId, lastUsed, logPath } } }`. Written
  atomically (write temp + rename).
- Logs: `~/.local/state/ception/logs/<hash>-<label>.log`, appended across
  turns and daemon restarts.

### Repo layout

```
bin/ception.mjs      entry point: parse argv, dispatch to client commands
lib/appserver.mjs    JSON-RPC JSONL client for the codex app-server child
lib/daemon.mjs       daemon main: socket server, turn management, lifecycle
lib/client.mjs       connect-or-spawn logic, command send, stream rendering
lib/render.mjs       event -> log line and event -> report formatting
lib/state.mjs        state file read/write, label registry
lib/claudepid.mjs    Claude Code ancestor detection + liveness polling
test/                node:test suites + fake app-server fixture
README.md            short usage doc
```

## CLI contract

```
ception spawn --label L [--cwd D] [--model M] [--effort E]
              [--read-only | --full-access] [--report brief|items|full]
              [PROMPT | -]
ception send  L [--report brief|items|full] [PROMPT | -]
ception interrupt L
ception kill  L | --all
ception list  [--all] [--json]
ception watch L
```

- Prompt comes from the positional argument, or from stdin when the argument
  is `-` or absent and stdin is not a TTY. Long prompts arrive via stdin.
- `spawn`: error if the label already has a live daemon. Creates daemon,
  `thread/start`, first `turn/start`, blocks until `turn/completed`, prints
  the report, exits. First line of stdout must include the log path so the
  user can `tail -f` it.
- `send`: if no live daemon but state has a `threadId` for the label,
  transparently respawn the daemon with `thread/resume`, then proceed. If the
  thread is idle: `turn/start`, block until the turn ends, print report. If a
  turn is in flight: `turn/steer`, print a one-line acknowledgement, exit 0
  immediately (the client blocked on the original send provides
  wake-on-completion).
- `interrupt`: `turn/interrupt` the active turn; no-op with a notice if idle.
- `kill`: graceful daemon shutdown (interrupt active turn, close app-server,
  remove socket). `--all` kills every daemon in the current project namespace.
- `list`: labels for the current project with liveness (live/idle/dead),
  threadId, last-used time; `--all` spans all projects; `--json` for machine
  output.
- `watch`: convenience `tail -f` of the label's log file (spawn `tail -F`).
- Sandbox flags map to app-server permissions: default `workspace-write`,
  `--read-only` and `--full-access` as the two alternatives. Always
  `approvalPolicy: "never"`.
- `--model` / `--effort` pass through to thread/turn params per the schema;
  when absent, send nothing so `~/.codex/config.toml` governs.

Exit codes: `0` turn completed (or steer/interrupt/kill accepted); `2` turn
failed; `3` turn interrupted; `4` usage or infrastructure error (bad label, no
daemon and no stored thread, app-server spawn failure, ...).

## Daemon specification

### Control socket protocol

JSONL both ways. Client sends exactly one command object, e.g.
`{ "cmd": "turn", "prompt": "...", "report": "brief" }`, then reads events
until a terminal object, then closes. Commands: `turn`, `steer`, `interrupt`,
`status`, `shutdown`. Daemon responses are objects like
`{ "type": "item", ... }` (only at report levels that need them),
`{ "type": "warning", ... }`, and a terminal
`{ "type": "result", status, report }` or `{ "type": "error", message }`.
Multiple concurrent client connections must be safe: one turn at a time per
daemon; a `turn` command while a turn is active is rejected with a clear error
telling the caller to use steer or wait.

### Spawn and locking

- Client attempts to connect to the socket. `ECONNREFUSED`/`ENOENT` → treat
  daemon as dead: unlink stale socket, acquire the per-label lockfile
  (`O_CREAT|O_EXCL` containing the pid; on `EEXIST`, check pid liveness and
  steal if dead), spawn the daemon, wait for the socket to accept (poll with
  timeout ~15s), release lock. Losing a spawn race must degrade to connecting
  to the winner's socket.
- The daemon spawns `npx -y @openai/codex app-server` with `cwd` = project
  cwd. If the child exits unexpectedly, fail any active turn with `type:
  "error"`, write to the log, and shut down (clients recover via
  respawn+resume).

### Lifecycle: when the daemon exits

1. **Claude ancestor watch (primary).** The *client* detects the Claude Code
   process at spawn time and passes `(pid, starttime)` to the daemon via the
   daemon's argv/env. Detection: walk the `/proc/<pid>/stat` ppid chain
   upward from the client's own pid; the Claude process is the nearest
   ancestor whose `/proc/<pid>/cmdline` matches `claude` (basename of argv[0]
   or an argv entry ending in `/claude`). Fallback heuristic: the parent of
   the topmost ancestor whose `/proc/<pid>/environ` contains `CLAUDECODE=1`.
   Record `starttime` (field 22 of `/proc/<pid>/stat`) to guard against pid
   reuse. The daemon polls every 30s: if the pid is gone or starttime
   changed, interrupt any active turn, flush, exit.
2. **No ancestor found** (spawned from a bare terminal): skip the watch; the
   daemon lives on idle timeout alone.
3. **Idle timeout (backstop):** exit after 4h with no turn activity and no
   connected client. Configurable via `CEPTION_IDLE_TIMEOUT_SECS`.
4. **Explicit:** `ception kill`, or `shutdown` on the socket.

On any exit path: remove the socket file, update state (`lastUsed`), leave the
log file in place.

## Output and rendering

Two independent sinks:

**Log file (always full).** Append-only plain text for humans running
`tail -f`. Per turn: a header line (timestamp, label, threadId, turn id,
prompt first line), then rendered events as they stream — raw reasoning and
reasoning-summary deltas as flowing text, `[cmd] ...` with exit code and
truncated output, `[edit] <file>` per file change, `[mcp]` tool calls,
`[msg]` agent message, then a footer (status, token usage, duration).
Reasoning deltas should be accumulated and written as coherent text, not one
line per delta.

**Client stdout (budgeted).** This is what lands in the Claude context — keep
it small by default.
- `brief` (default): final agent message verbatim, then a compact footer:
  status, files touched (names only), token usage, threadId, log path.
- `items`: brief plus one line per completed item (`[cmd]`/`[edit]`/`[mcp]`
  one-liners, no reasoning).
- `full`: everything the log gets. For interactive human use.

## Testing

Use `node:test`. Build `test/fake-appserver.mjs`: a Node script speaking the
app-server wire protocol on stdio, driven by a scenario file (JSON list of
scripted responses/notifications), substituted for the real codex via an env
var (e.g. `CEPTION_CODEX_CMD`) that overrides the `npx` command line — the
daemon must honor that override. Cover at least:

1. Happy path: spawn → turn events → `turn/completed` → report rendered, exit
   0, state file has the threadId, log file has reasoning text.
2. `send` to an idle live daemon starts a second turn on the same thread (no
   new app-server spawn — assert the fixture saw one process lifetime).
3. `send` during an active turn issues `turn/steer` and exits immediately;
   the original blocked client still completes.
4. Daemon dead + stored threadId: `send` respawns and issues `thread/resume`.
5. Unexpected server-initiated request is rejected with `-32601` and a
   warning reaches the client and the log.
6. Spawn race: two concurrent spawns for one label yield one daemon.
7. Claude-pid watch: daemon given a watched pid exits shortly after that
   process dies (use a scratch child process as the fake Claude).
8. Turn failure and interrupt map to exit codes 2 and 3.

Also add `scripts/smoke.sh`: a manual end-to-end run against real codex in a
scratch directory (spawn with a trivial prompt, send a follow-up, kill). It is
run by the user, not by CI or tests.

## Non-goals

- No Claude-side skill/prompt authoring (`skills/` is written separately).
- No MCP server mode, no broker shared across labels, no TUI.
- No macOS/Windows support.
- No handling of Codex approval flows beyond auto-reject-and-warn.
- No log rotation.

## Acceptance criteria

- `npm test` passes; tests use only the fake app-server (no network, no real
  codex, no ChatGPT auth).
- `ception` runs with zero npm dependencies on stock Node 24.
- `ception spawn --label t "do X"` against the fake server produces: brief
  stdout report, full log file, persisted state, exit 0 — and a second
  `ception send t "..."` reuses the same daemon.
- Killing the fake Claude ancestor process causes daemon exit within 60s.
- README.md documents the CLI, the lifecycle rules, and the env vars
  (`CEPTION_CODEX_CMD`, `CEPTION_IDLE_TIMEOUT_SECS`).

## Suggested order

1. `lib/appserver.mjs` + fake fixture + handshake test.
2. Daemon with `turn` command + client `spawn`/`send` happy path.
3. Rendering (log + brief report).
4. Steer, interrupt, exit codes.
5. State, respawn+resume, locking.
6. Claude-pid watch + idle timeout.
7. `list`, `kill`, `watch`, README, smoke script.
