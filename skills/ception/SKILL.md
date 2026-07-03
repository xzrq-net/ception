---
name: ception
description: Delegate implementation work to OpenAI Codex (GPT) running as a named background subagent. GPT is a frontier-class implementor — delegate anything whose intent you can put in writing, hard or mechanical, to keep the token churn out of your own context. Keep only decisions that need the user context in your head.
---

# ception: Codex as a subagent

`ception` runs Codex (GPT, the user's configured model) as a per-label daemon.
You interact with it like a native subagent: spawn in background, get woken on
completion, steer mid-flight, send follow-ups to the same thread.

## Division of labor

GPT is a peer intellect with one structural deficit: it does not share your
conversation, so it cannot infer the user's intent or taste. Everything else —
gnarly debugging, subtle algorithms, wide refactors, migrations, test design,
performance hunts — is in scope. Do not reserve hard work for yourself out of
capability doubt; reserve it only when the intent can't be transferred.

Delegate when you can write the intent down: the goal, the constraints, what
the user would object to. Keep for yourself:

- decisions that need judgment built up in this session, or a read on the
  user you can't articulate
- tasks where writing the intent down costs as much as doing the work
- the final review — always, because the failure mode is misread intent, not
  incompetence

## How to prompt GPT

The prompt must carry everything; there is no shared context.

- Lead with the actual goal and what the work feeds into — the *why* is what
  lets it make correct micro-decisions on its own.
- Concrete anchors: file paths, function names, and a verification command to
  run ("run `npm test`; all tests must pass").
- Constraints and non-goals explicitly: "do not refactor X", "no new
  dependencies", "do not touch files outside Y/".
- Latitude is fine and often better than over-specifying: "choose the data
  structure" works. When you leave a decision open, ask it to state in its
  report which way it went and why, so you can check the choice against
  intent.
- Taste is the one thing it cannot infer. Encode it as rules: match the
  surrounding code's comment density and idiom, naming conventions, error
  message style, what counts as too clever.
- For review work, just say "review": its system prompt has a code-review
  stance built in (findings first, ordered by severity, file/line refs).

The user's config defaults to a high reasoning effort. Pass `--effort medium`
only for truly mechanical bulk; leave it alone for anything with subtlety.

## Operating procedure

Start a run in background Bash (heredoc avoids quoting issues; `-` reads the
prompt from stdin). You are woken when the turn completes:

```sh
ception spawn --label impl - <<'EOF'
<goal, constraints, anchors, verification>
EOF
```

- The first stdout line (of both `spawn` and `send`) is the log path; the
  final report arrives on completion (message + status/files/tokens footer).
  Exit codes: 0 done, 2 failed, 3 interrupted, 4 infra/usage error.
- If you spawned with `--cwd` pointing outside the current project, pass that
  same `--cwd` to `send`, `interrupt`, `kill`, and `watch` too — without it
  they look in the wrong project and fail with "no live daemon or stored
  thread".
- Labels are scoped to the project root (nearest `.jj`/`.git` walking up from
  the shell's cwd, so any subdirectory of the project reaches the same labels)
  and to this Claude Code session — another session's labels are invisible to
  `send` and can't collide with yours. After the user resumes a session,
  `send` transparently adopts the old session's label and resumes its thread;
  if it instead fails with "belongs to live session", pick a different label.
- Follow-ups and course corrections go to the same thread:
  `ception send impl "..."`. If the turn is still running this steers it and
  returns immediately; if idle it starts a new turn and blocks. Steer on
  observed divergence — steering is cheap and reliably lands — but don't
  hover.
- Peek mid-run without ingesting reasoning spam:
  `grep -E '^\[(cmd|edit|mcp|msg)\]' <logpath> | tail -20`. The full log
  (including reasoning) is for the user, who may be tailing it.
- One turn at a time per label; use separate labels for parallel workstreams.
- `ception interrupt <label>` cancels a runaway turn; `ception list` shows
  what's alive.

## After completion

Review the diff (`jj diff` / `git diff`) against the user's intent — that is
the gap GPT cannot close itself. Run the verification commands yourself;
don't take the report's word for it. GPT is highly corrigible: a corrective
`send` naming the observed divergence ("in lib/foo.mjs:40, X happens; make it
do Y") reliably lands, and the thread retains its context — iterate there
rather than redoing the work yourself. Take over only when the remaining gap
is taste you can't put into words.
