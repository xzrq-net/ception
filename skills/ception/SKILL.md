---
name: ception
description:
  Delegate implementation, investigation, and review work to OpenAI Codex (GPT)
  running as a named background subagent. GPT works at your level; what it
  lacks is the user. Hand it the goal, the reasons and the bounds, not a spec,
  and keep the token churn out of your own context.
---

# ception: Codex as a subagent

`ception` runs Codex (GPT, the user's configured model) as a per-label daemon.
You interact with it like a native subagent: spawn in background, get woken on
completion, steer mid-flight, send follow-ups to the same thread.

**WIP.** If the tool itself misbehaves — confusing errors, hangs, reports that
don't match what happened, docs that disagree with behavior — tell the user
what you hit instead of working around it. They would rather fix the tool.

## Division of labor

GPT works at your level: debugging, algorithms, wide refactors, migrations,
test design, performance work, adversarial review. Do not keep hard work for
yourself out of capability doubt.

What it lacks is the user. Pasting the conversation does not transfer that;
reading the user's intent and taste is your job.

Keep for yourself:

- decisions that need judgment built up in this session, or a read on the user
  you can't articulate
- tasks where writing the intent down costs as much as doing the work
- the final review, always (delegated reviews feed it)

## Prompting

GPT does not see your conversation or tool results. It does see the working
copy, its `AGENTS.md`, and its own codex instructions. The user-level ones
(`~/.codex/AGENTS.md`) are the same text as your `~/.claude/CLAUDE.md`, so
tone, autonomy and engineering preferences need no restating. Send what it
cannot recover and leave out what it can. On `send`, give only the delta; the
thread keeps everything earlier unless the footer shows compactions.

What it cannot recover:

- the goal and what it feeds into, in the user's terms
- decisions already made, by the user or in this session, and what the user
  would object to
- the work mode and stopping point ("review and report; do not edit",
  "diagnose only", "implement and verify") and whether commits are wanted
- taste the repo does not already show, where a sensible default would be
  wrong for this user
- for review: the target and baseline, and findings-only or fixes too

What it can recover, so leave it out: the shape of the solution, which library,
what the tests should assert, the order of steps, how to run the checks. If the
prompt runs much longer than the user's request plus the items above, or you
are numbering implementation steps, you are specifying the solution; send the
goal instead. Anchors you happen to know (a path, a failing test, the line a
review found) are fine.

Imperatives outrank GPT's own judgment, and it complies where compliance is
plainly wrong. Use them only for what you mean categorically and phrase the
rest as defaults ("prefer X unless..."). Open decisions are fine; ask it to
report which way it went, and say which ambiguities should stop the work
instead.

The first report is also a signal about your prompt. If corrections don't
shrink each round, the framing is wrong; restate the goal. When you want a
check on a design of your own, ask for its take before showing yours.

## Choosing model and effort

Pick the model at `spawn` — `send` keeps the label's choice, so switching models
means a new label:

- `--model gpt-5.6-luna` — cheap. For work where the solution is already
  decided and correctness is mechanically checkable: rote renames, lint
  cleanup, narrow edits with deterministic tests. Its failure mode is a
  plausible patch that misses intent or an edge case. If a Luna task turns into
  discovery or design, respawn on the default model rather than compensating
  with follow-ups.
- unset — the user's configured default (`gpt-6-astra`). Everything else.

Do not pass `--effort` unless the user names a level; the codex config default
applies.

## Operating procedure

Start a run in background Bash (heredoc avoids quoting issues; `-` reads the
prompt from stdin). You are woken when the turn completes:

```sh
ception spawn --label impl - <<'EOF'
<goal, bounds, anchors, stopping point>
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
  returns immediately; if idle it starts a new turn and blocks. Steer when you
  see divergence; don't poll.
- `ception quota` shows the account's rate-limit windows (no label, no tokens).
  Check it before committing to a long arc, and first if a run stops with
  `usageLimited`.
- Peek mid-run without ingesting reasoning:
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
standing instruction for every turn, and you will not be consulted between
them. Say what done looks like, what is out of bounds, and what to write down
as it goes; notes on disk survive compaction.

`goal` also works on a name with no daemon yet, on the default model. `spawn`
first for a different model, or for an opening turn on a different footing
than the arc (a scoping pass, say).

**Read the goal line, not just the report.** Every turn report ends with one:

- `goal: complete` — the objective is met. The only status that means done.
- `goal: paused | blocked | usageLimited | budgetLimited` — codex stopped
  short; the report also prints the `--resume` command. A failed turn (server
  policy stops included — `error code:` in the footer names them) leaves the
  goal `blocked`.
- `goal: active` after you already have the report means codex is still going
  (see the section below).

`ception goal <label> --resume` blocks on the restarted run and keeps the same
daemon, app-server and thread, so codex's background shells and subagents
survive the stop. The deadline is the daemon's idle timeout (4h by default);
for an arc that may sit stopped longer, spawn under a raised
`CEPTION_IDLE_TIMEOUT_SECS`. If it stops at the same place twice, read the log
before resuming again.

Steering mid-run works as usual — `ception send <label> "..."` steers the live
turn; setting the objective again changes the standing instruction.
`ception interrupt <label>` pauses the goal and then interrupts, so it really
stops the run; `--pause` lets the current turn finish and stops the next one.

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

Review the diff (`jj diff` / `git diff`) against the user's intent. Run the
verification commands yourself. Check how the acceptance criteria were met, not
just that they pass: a special-cased test, a stubbed hard branch, a weakened
assertion, or a good solution to a slightly different problem all produce a
confident report and green checks.

When the report argues for deviating from your prompt, weigh it: GPT is often
right about the code and wrong about the user.

Corrections go back to the thread: the file and line, what happens instead of
what should, and the boundary that was crossed. Take over only when the
remaining gap is taste you can't put into words.
