---
name: ception
description:
  Delegate implementation, investigation, and review work to OpenAI Codex (GPT)
  running as a named background subagent. GPT is a frontier-class implementor —
  delegate anything whose intent you can put in writing, hard or mechanical, to
  keep the token churn out of your own context. Keep only decisions that need
  the user context in your head.
---

# ception: Codex as a subagent

`ception` runs Codex (GPT, the user's configured model) as a per-label daemon.
You interact with it like a native subagent: spawn in background, get woken on
completion, steer mid-flight, send follow-ups to the same thread.

**WIP.** This is the user's personal utility under active iteration. If the tool
itself misbehaves — confusing errors, hangs, reports that don't match what
happened, docs that disagree with behavior — tell the user what you hit instead
of silently working around it. They would rather fix the tool than absorb a
process breakdown.

## Division of labor

GPT on the default model is a peer intellect with one structural deficit: it
does not share your conversation, so it cannot infer the user's intent or taste.
Everything else — gnarly debugging, subtle algorithms, wide refactors,
migrations, test design, performance hunts — is in scope. Do not reserve hard
work for yourself out of capability doubt; reserve it only when the intent can't
be transferred.

Delegate when you can write the intent down: the goal, the constraints, what the
user would object to. Keep for yourself:

- decisions that need judgment built up in this session, or a read on the user
  you can't articulate
- tasks where writing the intent down costs as much as doing the work
- the final review — always, because the handoff must catch both misread intent
  and ordinary implementation error

## How to prompt GPT

GPT does not share your conversation or tool results, but it does get the repo:
its own instructions, the target's `AGENTS.md`, and a working copy it can
inspect. Transfer what it cannot recover — goal, decisions already made,
constraints — and don't paste discoverable repo context. On `send`, give only
the delta; the thread retains everything earlier.

- Lead with the actual goal and what the work feeds into — the _why_ is what
  lets it make correct micro-decisions on its own.
- Name the work mode and stopping point: "review and report; do not edit",
  "diagnose only", "implement and verify". The stopping point is load-bearing:
  GPT is a relentless executor, and driven accordingly it will keep grinding
  past any reasonable point rather than stop and ask. Review and diagnosis are
  treated as read-only unless you ask for the fix; say whether commits are
  wanted.
- Concrete anchors you actually know: file paths, function names, failing tests,
  a verification command. Guessed anchors are worse than none — if you don't
  know the repo's checks, ask it to find and run them.
- Constraints and non-goals explicitly: "do not refactor X", "no new
  dependencies". GPT is fantastically instruction-compliant — an imperative
  outranks its own judgment — so reserve imperatives for what you mean
  categorically and soften the rest to defaults ("prefer X unless..."), or it
  will comply even where compliance is plainly wrong.
- Latitude is fine and often better than over-specifying: "choose the data
  structure" works. When you leave a decision open, ask it to report which way
  it went and why. Say which ambiguities should instead stop the work and come
  back for direction.
- Taste is the one thing it cannot infer. Encode it as rules: match the
  surrounding code's comment density and idiom, naming conventions, error
  message style, what counts as too clever.
- For review work, say "review" plus the target and baseline (which branch,
  diff, or dirty worktree) and whether you want findings only or fixes too; the
  findings-first, severity-ordered, file/line-ref stance is built in.

## Choosing model and effort

Pick the model at `spawn` — `send` keeps the label's choice, so switching models
means a new label:

- `--model gpt-5.6-luna` — preposterously cheap. Use it when the solution is
  already decided and correctness is mechanically checkable: rote renames,
  formatting/lint cleanup, boilerplate, repetitive fixtures, narrow edits with
  deterministic tests. Its failure mode is a plausible patch that misses intent
  or edge cases, so keep Luna turns small and verifiable. If a Luna task turns
  into discovery or design, respawn on Sol rather than compensating with
  follow-ups.
- unset — the user's configured default (`gpt-5.6-sol`, frontier). Everything
  else: any implementation or debugging needing real code judgment, ambiguity,
  architecture, broad refactors, security/performance work, adversarial review.

The user's config defaults to a high reasoning effort. Pair Luna with
`--effort medium` for bounded edits; leave effort alone for anything with
subtlety. Higher effort is not a substitute for a bigger model when the task
needs judgment.

## Operating procedure

Start a run in background Bash (heredoc avoids quoting issues; `-` reads the
prompt from stdin). You are woken when the turn completes:

```sh
ception spawn --label impl - <<'EOF'
<goal, constraints, anchors, verification>
EOF
```

- The first stdout line (of both `spawn` and `send`) is the log path; the final
  report arrives on completion (message + status/files/tokens/duration footer).
  Exit codes: 0 done, 2 failed, 3 interrupted, 4 infra/usage error.
- `--report` sets how much of the turn the report carries: `brief` (default) is
  the final message plus footer, `items` adds the command/edit trail, `full` is
  everything including reasoning. Use `items` when you'll want to audit what it
  did without opening the log.
- If you spawned with `--cwd` pointing outside the current project, pass that
  same `--cwd` to `send`, `interrupt`, `kill`, and `watch` too — without it they
  look in the wrong project and fail with "no live daemon or stored thread".
- Labels are scoped to the project root (nearest `.jj`/`.git` walking up from
  the shell's cwd, so any subdirectory of the project reaches the same labels)
  and to this Claude Code session — another session's labels are invisible to
  `send` and can't collide with yours. After the user resumes a session, `send`
  transparently adopts the old session's label and resumes its thread; if it
  instead fails with "belongs to live session", pick a different label.
- Follow-ups and course corrections go to the same thread:
  `ception send impl "..."`. If the turn is still running this steers it and
  returns immediately; if idle it starts a new turn and blocks. Steer on
  observed divergence — steering is cheap and reliably lands — but don't hover.
- Peek mid-run without ingesting reasoning spam:
  `grep -E '^\[(cmd|edit|mcp|msg)\]' <logpath> | tail -20`. The full log
  (including reasoning) is for the user, who may be tailing it.
- One turn at a time per label; use separate labels for parallel workstreams.
  Labels isolate threads, not files: parallel labels in the same cwd edit the
  same working copy. Give parallel writers disjoint paths or separate worktrees,
  and tell each which changes belong to someone else.
- If your spawn/send shell was killed mid-turn (harness kill, user stop), the
  daemon and its turn keep running. Reattach with `ception watch <label>`: it
  blocks until the current turn completes and delivers the report and exit code
  (at its own `--report` level, default brief — pass `--report full` to match a
  full-report run). Idle daemon: prints `no active turn`, exits 0; no daemon:
  exit 4, recover with `send`.
- `ception interrupt <label>` cancels a runaway turn; `ception list` shows
  what's alive, including each label's goal status. Daemons exit on their own
  (idle timeout, or this Claude session ending) — `kill` is for stuck ones, not
  routine cleanup. Killing a daemon takes codex's background shells and
  subagents with it; interrupting or pausing does not.

## Long arcs: goals

For work measured in hours rather than turns — an audit, a migration, a sweep
across a large surface — set a **goal** instead of prompting turn by turn. Codex
then starts its own turns, back to back, until the objective is met:

```sh
ception goal audit - <<'EOF'
<the arc: what done looks like, the boundaries, what to report>
EOF
```

This blocks like `spawn` and returns one report for the whole run. Everything
above about prompting applies to the objective, with more weight: it is the
standing instruction for every turn codex starts, and you will not be consulted
between them. Say what done looks like, what is out of bounds, and what to
write down as it goes. Notes on disk are how a long run survives its own
compactions.

`goal` also works on a name with no daemon yet — the objective alone starts the
run, on the configured default model. `spawn` first when you want a different
model, or an opening turn on a different footing from the arc (a scoping pass,
say) before the goal takes over.

**Read the goal line, not just the report.** Every turn report ends with one:

- `goal: complete` — the objective is met. The only status that means done.
- `goal: paused | blocked | usageLimited | budgetLimited` — codex stopped
  short. The report also prints the `--resume` command. `blocked` is what a
  failed turn produces, including a server-side policy stop: the turn fails,
  the footer's `error code:` names it, and codex stops starting turns.
- `goal: active` after you already have the report means codex is still going
  (see the section below).

Resuming is `ception goal <label> --resume`: it blocks on the run codex
restarts, and keeps the same daemon, app-server and thread — so background
shells, subagents and everything else codex had in flight survive the stop. The
deadline is the daemon's idle timeout (4h by default), which starts running the
moment the goal stops, not any property of the stop itself. Resume when you have
decided the run should continue; for an arc that may sit stopped longer than
that, spawn it under a raised `CEPTION_IDLE_TIMEOUT_SECS`.
A stop that keeps recurring at the same place is a signal about the work, not
noise to be retried around; read the log before resuming again.

Steering mid-run works as usual — `ception send <label> "..."` steers whatever
turn is live. To change the standing instruction rather than the current turn,
set the objective again with `ception goal <label> "<revised arc>"`.
`ception interrupt <label>` pauses the goal and then interrupts, so it really
does stop the run; `ception goal <label> --pause` lets the current turn finish
and stops the next one.

## When one `spawn` is more than one Codex turn

Codex sometimes keeps working after a turn ends — with an active thread goal it
starts follow-on turns by itself, and compaction can do the same. The daemon
follows the work across those turns and gives you a single report, so normally
you need not care. Two things make it visible:

- A `compactions: N` line in the footer means the model spent part of the run
  working from a summary. Verify that report against the diff more carefully
  than usual.
- **A label showing `active` in `ception list` after you already got its
  report.** Codex started another turn later than the daemon waited. Your report
  covers only part of the run and the rest is still happening. Attach with
  `ception watch <label>` — but if the continuation already finished, `watch`
  says `no active turn` and only the log has the rest, so check the log before
  concluding nothing happened. Do not start new work on that label, and do not
  assume the working copy is quiescent, until it is idle.

One residual failure: a turn reported **failed** with an `Instructions loaded.`
warning means compaction wiped the context instead of summarising it and the
turn stopped. Work done before it is on disk but unreported — check the diff,
then `ception send <label>` with a resume prompt pointing at the diff and the
agent's notes file. Confirm the label is idle first; sending into a live
continuation steers it instead of resuming.

Prevention: scope each turn small (one gate/phase per turn) and have the agent
update notes at intermediate milestones — plus milestone commits where the
target repo permits agent commits.

## After completion

Review the diff (`jj diff` / `git diff`) against the user's intent — that is the
gap GPT cannot close itself. Run the verification commands yourself; don't take
the report's word for it. Two failure modes to look for, both of which produce a
confident report and passing checks: the brilliant solution to a subtly
different problem, and the intellectual shortcut — a special-cased test, a
stubbed hard branch, a weakened assertion. Check _how_ the acceptance criteria
were met, not just that they pass. GPT is highly corrigible: a corrective `send`
naming the observed divergence ("in lib/foo.mjs:40, X happens; make it do Y")
reliably lands, and the thread retains its context — iterate there rather than
redoing the work yourself. Take over only when the remaining gap is taste you
can't put into words.
