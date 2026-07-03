---
name: ception
description: Delegate bulk implementation work to OpenAI Codex (GPT) running as a named background subagent. Use when a task has a crisp spec and a large diff — wide mechanical refactors, test scaffolding, first-draft modules — to keep the heavy token churn out of your own context.
---

# ception: Codex as a subagent

`ception` runs Codex (GPT, the user's configured model) as a per-label daemon.
You interact with it like a native subagent: spawn in background, get woken on
completion, steer mid-flight, send follow-ups to the same thread.

## When to delegate

Delegate when the diff is large but the decisions are small, and you can state
the spec completely:

- bulk implementation against a design you've already settled
- wide mechanical refactors and API migrations
- test scaffolding for existing behavior
- first drafts of well-specified, self-contained modules

Keep for yourself: design, ambiguity resolution, anything where writing a
complete spec costs as much as doing the work, tasks needing judgment you have
already built up in context this session, and the final review — always.

Codex does not share your conversation. The prompt must carry everything:
paths, constraints, conventions, verification commands. If you can't write
that down, the task isn't ready to delegate.

## How to prompt GPT

GPT executes specific instructions extremely well and handles metacognitive
framing poorly. Write the prompt like a work order, not a discussion:

- One line up front: the goal and what the work feeds into.
- Concrete file paths, function names, exact commands.
- Acceptance criteria as a checklist, including a verification command to run
  ("run `npm test`; all tests must pass").
- Explicit non-goals: "do not refactor X", "do not touch files outside Y/".
- Imperative mood. No "consider", no "you might", no options for it to weigh.
- Never ask it to reflect, assess its confidence, or manage scope tradeoffs —
  make those calls yourself and encode the result.
- For review work, just say "review": its system prompt has a code-review
  stance built in (findings first, ordered by severity, file/line refs).

The user's config defaults to a high reasoning effort. For mechanical bulk
work, pass `--effort medium` to cut latency; leave it alone when the task has
any subtlety.

## Operating procedure

Start a run in background Bash (heredoc avoids quoting issues; `-` reads the
prompt from stdin). You are woken when the turn completes:

```sh
ception spawn --label impl - <<'EOF'
<work order>
EOF
```

- The first stdout line is the log path; the final report arrives on
  completion (message + status/files/tokens footer). Exit codes: 0 done,
  2 failed, 3 interrupted, 4 infra/usage error.
- Follow-ups and course corrections go to the same thread:
  `ception send impl "..."`. If the turn is still running this steers it and
  returns immediately; if idle it starts a new turn and blocks. Steer only on
  observed divergence, not to hover.
- Peek mid-run without ingesting reasoning spam:
  `grep -E '^\[(cmd|edit|mcp|msg)\]' <logpath> | tail -20`. The full log
  (including reasoning) is for the user, who may be tailing it.
- One turn at a time per label; use separate labels for parallel workstreams.
- `ception interrupt <label>` cancels a runaway turn; `ception list` shows
  what's alive.

## After completion

Review the diff adversarially (`jj diff` / `git diff`) — treat the output as
untrusted. Run the verification commands yourself; don't take the report's
word for it. For fixes, prefer `ception send <label>` with specific corrective
instructions ("in lib/foo.mjs:40, X happens; make it do Y") over re-explaining
the task — the thread retains its context. Escalate to doing it yourself when
a second corrective pass doesn't land.
