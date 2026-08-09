#!/usr/bin/env node
import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import readline from "node:readline";

const STATE_PATH = process.env.CEPTION_FAKE_STATE ?? path.join(process.cwd(), "fake-appserver-state.json");
const BEHAVIOR = process.env.CEPTION_FAKE_BEHAVIOR ?? "happy";
const SCENARIO_PATH = process.env.CEPTION_FAKE_SCENARIO ?? null;
const SCENARIO = SCENARIO_PATH ? JSON.parse(fs.readFileSync(SCENARIO_PATH, "utf8")) : null;

let activeTurn = null;
let requestId = 9000;
let scenarioIndex = 0;
// Goals live in the app-server for real; the fake keeps them per process, which
// is enough because one daemon owns one app-server and one thread.
let goal = null;
let goalTurns = 0;

function loadState() {
  if (!fs.existsSync(STATE_PATH)) {
    return {
      appServerStarts: 0,
      nextThread: 1,
      nextTurn: 1,
      threads: [],
      requests: [],
      lastTurnStarts: [],
      steers: [],
      interrupts: [],
      rejectedRequests: 0
    };
  }
  return JSON.parse(fs.readFileSync(STATE_PATH, "utf8"));
}

function saveState(state) {
  fs.mkdirSync(path.dirname(STATE_PATH), { recursive: true });
  // Atomic rename: tests (and sibling fake instances) poll this file and must
  // never see a partial write. pid-suffixed so concurrent fakes don't collide.
  const tempPath = `${STATE_PATH}.${process.pid}.tmp`;
  fs.writeFileSync(tempPath, `${JSON.stringify(state, null, 2)}\n`);
  fs.renameSync(tempPath, STATE_PATH);
}

function updateState(fn) {
  const state = loadState();
  fn(state);
  saveState(state);
  return state;
}

let batch = null;

function send(message) {
  if (batch) {
    batch.push(message);
    return;
  }
  process.stdout.write(`${JSON.stringify(message)}\n`);
}

// One write, so the reader gets every line in a single chunk and dispatches
// them all before any of its own awaits resume. The real server can do this
// whenever it answers and then immediately runs and ends a turn.
function withBatch(fn) {
  batch = [];
  try {
    fn();
  } finally {
    const messages = batch;
    batch = null;
    process.stdout.write(`${messages.map((message) => JSON.stringify(message)).join("\n")}\n`);
  }
}

function recordRequest(message) {
  updateState((state) => {
    state.requests.push({ method: message.method, params: message.params ?? null });
  });
}

function nowSeconds() {
  return Math.floor(Date.now() / 1000);
}

function buildThread(thread) {
  return {
    id: thread.id,
    sessionId: thread.id,
    forkedFromId: null,
    parentThreadId: null,
    preview: "",
    ephemeral: false,
    modelProvider: "openai",
    createdAt: thread.createdAt,
    updatedAt: nowSeconds(),
    recencyAt: nowSeconds(),
    status: { type: "idle" },
    path: null,
    cwd: thread.cwd,
    cliVersion: "fake",
    source: "appServer",
    threadSource: null,
    agentNickname: null,
    agentRole: null,
    gitInfo: null,
    name: null,
    turns: []
  };
}

function buildTurn(id, status = "inProgress", error = null) {
  return {
    id,
    items: [],
    itemsView: "full",
    status,
    error,
    startedAt: nowSeconds(),
    completedAt: status === "inProgress" ? null : nowSeconds(),
    durationMs: status === "inProgress" ? null : 25
  };
}

function createThread(cwd) {
  const state = loadState();
  const thread = {
    id: `thr_${state.nextThread++}`,
    cwd,
    createdAt: nowSeconds()
  };
  state.threads.push(thread);
  saveState(state);
  return thread;
}

function findThread(threadId) {
  const state = loadState();
  const thread = state.threads.find((candidate) => candidate.id === threadId);
  if (!thread) {
    throw new Error(`unknown thread ${threadId}`);
  }
  return thread;
}

function nextTurn() {
  const state = loadState();
  const turnId = `turn_${state.nextTurn++}`;
  saveState(state);
  return turnId;
}

function promptText(input) {
  return (input ?? [])
    .filter((item) => item.type === "text")
    .map((item) => item.text)
    .join("\n");
}

function tokenUsage() {
  return {
    total: {
      totalTokens: 42,
      inputTokens: 20,
      cachedInputTokens: 0,
      outputTokens: 22,
      reasoningOutputTokens: 5
    },
    last: {
      totalTokens: 42,
      inputTokens: 20,
      cachedInputTokens: 0,
      outputTokens: 22,
      reasoningOutputTokens: 5
    },
    modelContextWindow: 200000
  };
}

function completeTurn({ threadId, turnId, prompt, status = "completed", message = null }) {
  const finalText = message ?? (prompt.includes("follow") ? "Resumed the prior run.\nFollow-up prompt accepted." : `Handled the requested task.\nPrompt: ${prompt}`);
  send({ method: "turn/started", params: { threadId, turn: buildTurn(turnId) } });
  send({
    method: "item/started",
    params: {
      threadId,
      turnId,
      startedAtMs: Date.now(),
      item: { type: "reasoning", id: `reason_${turnId}`, summary: [], content: [] }
    }
  });
  send({
    method: "item/reasoning/summaryTextDelta",
    params: {
      threadId,
      turnId,
      itemId: `reason_${turnId}`,
      summaryIndex: 0,
      delta: "Thinking through fixture."
    }
  });
  send({
    method: "item/completed",
    params: {
      threadId,
      turnId,
      completedAtMs: Date.now(),
      item: {
        type: "reasoning",
        id: `reason_${turnId}`,
        summary: ["Thinking through fixture."],
        content: []
      }
    }
  });
  send({
    method: "item/completed",
    params: {
      threadId,
      turnId,
      completedAtMs: Date.now(),
      item: {
        type: "fileChange",
        id: `edit_${turnId}`,
        changes: [{ path: "src/example.js", kind: { type: "update", move_path: null }, diff: "@@" }],
        status: "completed"
      }
    }
  });
  send({
    method: "item/agentMessage/delta",
    params: {
      threadId,
      turnId,
      itemId: `msg_${turnId}`,
      delta: finalText
    }
  });
  send({
    method: "item/completed",
    params: {
      threadId,
      turnId,
      completedAtMs: Date.now(),
      item: {
        type: "agentMessage",
        id: `msg_${turnId}`,
        text: finalText,
        phase: "final_answer",
        memoryCitation: null
      }
    }
  });
  send({
    method: "thread/tokenUsage/updated",
    params: { threadId, turnId, tokenUsage: tokenUsage() }
  });
  const error = status === "failed" ? { message: "fixture failure", codexErrorInfo: null, additionalDetails: null } : null;
  send({ method: "turn/completed", params: { threadId, turn: buildTurn(turnId, status, error) } });
  activeTurn = null;
}

function startLongTurn(threadId, turnId, prompt) {
  activeTurn = { threadId, turnId, prompt };
  send({ method: "turn/started", params: { threadId, turn: buildTurn(turnId) } });
  send({
    method: "item/started",
    params: {
      threadId,
      turnId,
      startedAtMs: Date.now(),
      item: { type: "reasoning", id: `reason_${turnId}`, summary: [], content: [] }
    }
  });
  send({
    method: "item/reasoning/summaryTextDelta",
    params: {
      threadId,
      turnId,
      itemId: `reason_${turnId}`,
      summaryIndex: 0,
      delta: "Waiting for steering."
    }
  });
}

// Reproduces the observed compaction shape: codex closes the turn with a bare
// instruction acknowledgement, then continues the real work in a fresh turn.
function startCompactedTurn(threadId, turnId) {
  send({ method: "turn/started", params: { threadId, turn: buildTurn(turnId) } });
  send({
    method: "item/completed",
    params: {
      threadId,
      turnId,
      completedAtMs: Date.now(),
      item: { type: "contextCompaction", id: `compact_${turnId}` }
    }
  });
  send({
    method: "item/completed",
    params: {
      threadId,
      turnId,
      completedAtMs: Date.now(),
      item: {
        type: "agentMessage",
        id: `msg_${turnId}`,
        text: "Instructions loaded for `/repo`.",
        phase: "final_answer",
        memoryCitation: null
      }
    }
  });
  send({ method: "turn/completed", params: { threadId, turn: buildTurn(turnId, "completed", null) } });

  const continuationId = nextTurn();
  setTimeout(() => {
    activeTurn = { threadId, turnId: continuationId, prompt: "" };
    send({ method: "turn/started", params: { threadId, turn: buildTurn(continuationId) } });
    const finish = () => {
      send({
        method: "item/completed",
        params: {
          threadId,
          turnId: continuationId,
          completedAtMs: Date.now(),
          item: {
            type: "agentMessage",
            id: `msg_${continuationId}`,
            text: "Continued past the compaction and finished the work.",
            phase: "final_answer",
            memoryCitation: null
          }
        }
      });
      send({ method: "thread/tokenUsage/updated", params: { threadId, turnId: continuationId, tokenUsage: tokenUsage() } });
      send({ method: "turn/completed", params: { threadId, turn: buildTurn(continuationId, "completed", null) } });
      activeTurn = null;
    };
    // Optionally keep the continuation running so tests can observe it live.
    const runMs = Number(process.env.CEPTION_FAKE_CONTINUATION_RUN_MS ?? 0);
    if (runMs > 0) {
      setTimeout(finish, runMs);
    } else {
      finish();
    }
  }, Number(process.env.CEPTION_FAKE_CONTINUATION_DELAY_MS ?? 150));
}

// The real cross-turn mechanism: an active thread goal makes codex start a
// follow-on turn by itself once the thread goes idle.
function startGoalTurn(threadId, turnId) {
  const goal = (status) => ({
    method: "thread/goal/updated",
    params: {
      threadId,
      turnId,
      goal: {
        threadId,
        objective: "Finish the fixture work",
        status,
        tokenBudget: null,
        tokensUsed: 100,
        timeUsedSeconds: 1,
        createdAt: 0,
        updatedAt: 0
      }
    }
  });
  send({ method: "turn/started", params: { threadId, turn: buildTurn(turnId) } });
  send(goal("active"));
  send({
    method: "item/completed",
    params: {
      threadId,
      turnId,
      completedAtMs: Date.now(),
      item: { type: "agentMessage", id: `msg_${turnId}`, text: "First half done.", phase: "final_answer", memoryCitation: null }
    }
  });
  send({ method: "turn/completed", params: { threadId, turn: buildTurn(turnId, "completed", null) } });
  // A subagent's own thread finishing its own goal, on this same connection,
  // while our run is mid-hold. Nothing about it concerns this thread.
  send({
    method: "thread/goal/updated",
    params: {
      threadId: "thr_subagent",
      turnId: null,
      goal: { ...goal("complete").params.goal, threadId: "thr_subagent" }
    }
  });

  const continuationId = nextTurn();
  setTimeout(() => {
    activeTurn = { threadId, turnId: continuationId, prompt: "" };
    send({ method: "turn/started", params: { threadId, turn: buildTurn(continuationId) } });
    send({
      method: "item/completed",
      params: {
        threadId,
        turnId: continuationId,
        completedAtMs: Date.now(),
        item: {
          type: "agentMessage",
          id: `msg_${continuationId}`,
          text: "Goal continuation finished the work.",
          phase: "final_answer",
          memoryCitation: null
        }
      }
    });
    send({ method: "thread/tokenUsage/updated", params: { threadId, turnId: continuationId, tokenUsage: tokenUsage() } });
    send({ method: "thread/goal/updated", params: { threadId, turnId: continuationId, goal: { ...goal("complete").params.goal } } });
    send({ method: "turn/completed", params: { threadId, turn: buildTurn(continuationId, "completed", null) } });
    activeTurn = null;
  }, Number(process.env.CEPTION_FAKE_CONTINUATION_DELAY_MS ?? 150));
}

function buildGoal(threadId) {
  return {
    threadId,
    objective: goal.objective,
    status: goal.status,
    tokenBudget: null,
    tokensUsed: 250,
    timeUsedSeconds: 3,
    createdAt: 0,
    updatedAt: nowSeconds()
  };
}

function setGoalStatus(threadId, status) {
  goal.status = status;
  send({ method: "thread/goal/updated", params: { threadId, goal: buildGoal(threadId) } });
}

// What an active goal actually does: codex starts its own turn on the idle
// thread. The "goal-stopped" behaviour has the server end that first turn with
// a policy error, which is the shape a server-side safeguard produces — the
// turn fails and the goal goes to `blocked` until someone resumes it.
function startGoalTurnFromGoal(threadId) {
  const turnId = nextTurn();
  activeTurn = { threadId, turnId, prompt: "" };
  const stopping = (BEHAVIOR === "goal-stopped" || BEHAVIOR === "goal-instant") && goalTurns === 0;
  goalTurns += 1;
  if (BEHAVIOR === "steer") {
    // Park the goal's turn open so a test can steer it mid-flight.
    startLongTurn(threadId, turnId, "");
    return;
  }
  send({ method: "turn/started", params: { threadId, turn: buildTurn(turnId) } });
  send({
    method: "item/completed",
    params: {
      threadId,
      turnId,
      completedAtMs: Date.now(),
      item: {
        type: "agentMessage",
        id: `msg_${turnId}`,
        text: stopping ? "Working on the objective." : "Objective met; work finished.",
        phase: "final_answer",
        memoryCitation: null
      }
    }
  });
  send({ method: "thread/tokenUsage/updated", params: { threadId, turnId, tokenUsage: tokenUsage() } });
  if (stopping) {
    const error = {
      message: "Turn stopped by a server-side content policy check.",
      codexErrorInfo: "policyStop",
      additionalDetails: null
    };
    send({ method: "turn/completed", params: { threadId, turn: buildTurn(turnId, "failed", error) } });
    activeTurn = null;
    setGoalStatus(threadId, "blocked");
    return;
  }
  send({ method: "turn/completed", params: { threadId, turn: buildTurn(turnId, "completed", null) } });
  activeTurn = null;
  setGoalStatus(threadId, "complete");
}

function sendUnsupportedRequest(threadId, turnId) {
  send({
    id: requestId++,
    method: "item/commandExecution/requestApproval",
    params: {
      threadId,
      turnId,
      itemId: `cmd_${turnId}`,
      command: "echo no",
      cwd: process.cwd(),
      reason: "fixture"
    }
  });
}

function materialize(value, message) {
  if (Array.isArray(value)) {
    return value.map((entry) => materialize(entry, message));
  }
  if (value && typeof value === "object") {
    const result = {};
    for (const [key, entry] of Object.entries(value)) {
      result[key] = materialize(entry, message);
    }
    return result;
  }
  if (value === "$id") {
    return message.id;
  }
  return value;
}

function handleScenario(message) {
  if (!SCENARIO) {
    return false;
  }
  const step = SCENARIO[scenarioIndex];
  if (!step) {
    if (message.id !== undefined) {
      send({ id: message.id, error: { code: -32000, message: `unexpected request ${message.method}` } });
    }
    return true;
  }
  const expected = step.on ?? step.method;
  if (expected && expected !== message.method) {
    if (message.id !== undefined) {
      send({ id: message.id, error: { code: -32000, message: `expected ${expected}, got ${message.method}` } });
    }
    return true;
  }
  scenarioIndex += 1;
  const emit = () => {
    if (message.id !== undefined) {
      if (step.error) {
        send({ id: message.id, error: materialize(step.error, message) });
      } else {
        send({ id: message.id, result: materialize(step.result ?? {}, message) });
      }
    }
    for (const notification of step.notifications ?? []) {
      send(materialize(notification, message));
    }
    for (const request of step.requests ?? []) {
      send(materialize(request, message));
    }
  };
  if (step.delayMs) {
    setTimeout(emit, step.delayMs);
  } else {
    emit();
  }
  return true;
}

const bootState = loadState();
bootState.appServerStarts += 1;
saveState(bootState);

const rl = readline.createInterface({ input: process.stdin });
rl.on("line", (line) => {
  if (!line.trim()) {
    return;
  }
  const message = JSON.parse(line);

  if (message.id !== undefined && !message.method) {
    if (message.error?.code === -32601) {
      updateState((state) => {
        state.rejectedRequests += 1;
      });
    }
    return;
  }

  recordRequest(message);

  if (handleScenario(message)) {
    return;
  }

  try {
    switch (message.method) {
      case "initialize":
        send({ id: message.id, result: { userAgent: "fake", codexHome: "/tmp/fake", platformFamily: "unix", platformOs: "linux" } });
        break;

      case "initialized":
        break;

      case "thread/start": {
        const thread = createThread(message.params.cwd ?? process.cwd());
        send({ id: message.id, result: { thread: buildThread(thread) } });
        send({ method: "thread/started", params: { thread: buildThread(thread) } });
        break;
      }

      case "thread/resume": {
        const thread = findThread(message.params.threadId);
        send({ id: message.id, result: { thread: buildThread(thread) } });
        break;
      }

      case "turn/start": {
        findThread(message.params.threadId);
        const turnId = nextTurn();
        const prompt = promptText(message.params.input);
        updateState((state) => {
          state.lastTurnStarts.push({
            threadId: message.params.threadId,
            turnId,
            prompt,
            model: message.params.model ?? null,
            effort: message.params.effort ?? null,
            sandboxPolicy: message.params.sandboxPolicy ?? null
          });
        });
        send({ id: message.id, result: { turn: buildTurn(turnId) } });
        if (BEHAVIOR === "steer" || prompt.includes("slow")) {
          startLongTurn(message.params.threadId, turnId, prompt);
          break;
        }
        if (BEHAVIOR === "continuation") {
          startCompactedTurn(message.params.threadId, turnId);
          break;
        }
        if (BEHAVIOR === "goal-continuation") {
          startGoalTurn(message.params.threadId, turnId);
          break;
        }
        if (BEHAVIOR === "fail") {
          completeTurn({
            threadId: message.params.threadId,
            turnId,
            prompt,
            status: "failed",
            message: "Partial output before failure."
          });
          break;
        }
        if (BEHAVIOR === "server-request") {
          send({ method: "turn/started", params: { threadId: message.params.threadId, turn: buildTurn(turnId) } });
          sendUnsupportedRequest(message.params.threadId, turnId);
          setTimeout(() => completeTurn({ threadId: message.params.threadId, turnId, prompt }), 100);
          break;
        }
        completeTurn({ threadId: message.params.threadId, turnId, prompt });
        break;
      }

      case "thread/goal/set": {
        const threadId = message.params.threadId;
        findThread(threadId);
        goal = {
          objective: message.params.objective ?? goal?.objective ?? "",
          status: message.params.status ?? "active"
        };
        const answer = () => {
          send({ id: message.id, result: { goal: buildGoal(threadId) } });
          send({ method: "thread/goal/updated", params: { threadId, goal: buildGoal(threadId) } });
        };
        // Ends the running turn and rejects the goal request in one write.
        if (BEHAVIOR === "goal-set-error") {
          goal = null;
          withBatch(() => {
            if (activeTurn) {
              completeTurn({ threadId, turnId: activeTurn.turnId, prompt: activeTurn.prompt });
            }
            send({ id: message.id, error: { code: -32000, message: "goal rejected by fixture" } });
          });
          break;
        }
        // The goal is accepted, then stops without ever starting a turn.
        if (BEHAVIOR === "goal-stalls" && !activeTurn) {
          answer();
          setTimeout(() => setGoalStatus(threadId, "usageLimited"), 60);
          break;
        }
        // The running turn ends between the reply and the goal taking effect;
        // only then does the goal produce its own turn.
        if (BEHAVIOR === "goal-late-active" && activeTurn) {
          const parked = activeTurn;
          withBatch(() => {
            send({ id: message.id, result: { goal: buildGoal(threadId) } });
            completeTurn({
              threadId,
              turnId: parked.turnId,
              prompt: parked.prompt,
              message: "Parked turn finished."
            });
            send({ method: "thread/goal/updated", params: { threadId, goal: buildGoal(threadId) } });
          });
          setTimeout(() => {
            const continuationId = nextTurn();
            activeTurn = { threadId, turnId: continuationId, prompt: "" };
            send({ method: "turn/started", params: { threadId, turn: buildTurn(continuationId) } });
            send({
              method: "item/completed",
              params: {
                threadId,
                turnId: continuationId,
                completedAtMs: Date.now(),
                item: {
                  type: "agentMessage",
                  id: `msg_${continuationId}`,
                  text: "Continuation after the late goal.",
                  phase: "final_answer",
                  memoryCitation: null
                }
              }
            });
            send({ method: "thread/tokenUsage/updated", params: { threadId, turnId: continuationId, tokenUsage: tokenUsage() } });
            setGoalStatus(threadId, "complete");
            send({ method: "turn/completed", params: { threadId, turn: buildTurn(continuationId, "completed", null) } });
            activeTurn = null;
          }, 60);
          break;
        }
        // The running turn absorbs the goal and meets the objective itself,
        // with no continuation at all.
        if (BEHAVIOR === "goal-in-running-turn" && activeTurn) {
          answer();
          const parked = activeTurn;
          setTimeout(() => {
            setGoalStatus(threadId, "complete");
            completeTurn({
              threadId,
              turnId: parked.turnId,
              prompt: parked.prompt,
              message: "Objective met inside the running turn."
            });
          }, 50);
          break;
        }
        // The running turn ends and the objective continues in a fresh turn —
        // the one the goal client is actually waiting for.
        if (BEHAVIOR === "goal-during-turn" && activeTurn) {
          answer();
          const parked = activeTurn;
          setTimeout(() => {
            completeTurn({
              threadId,
              turnId: parked.turnId,
              prompt: parked.prompt,
              message: "Parked turn finished."
            });
            setTimeout(() => {
              const continuationId = nextTurn();
              activeTurn = { threadId, turnId: continuationId, prompt: "" };
              send({ method: "turn/started", params: { threadId, turn: buildTurn(continuationId) } });
              send({
                method: "item/completed",
                params: {
                  threadId,
                  turnId: continuationId,
                  completedAtMs: Date.now(),
                  item: {
                    type: "agentMessage",
                    id: `msg_${continuationId}`,
                    text: "Goal turn finished the work.",
                    phase: "final_answer",
                    memoryCitation: null
                  }
                }
              });
              send({ method: "thread/tokenUsage/updated", params: { threadId, turnId: continuationId, tokenUsage: tokenUsage() } });
              setGoalStatus(threadId, "complete");
              send({ method: "turn/completed", params: { threadId, turn: buildTurn(continuationId, "completed", null) } });
              activeTurn = null;
            }, 50);
          }, 50);
          break;
        }
        const startsTurn = goal.status === "active" && !activeTurn;
        // "goal-instant" answers and runs the whole turn in one write, the
        // shape a turn that fails the moment it starts produces.
        if (BEHAVIOR === "goal-instant" && startsTurn) {
          withBatch(() => {
            answer();
            startGoalTurnFromGoal(threadId);
          });
          break;
        }
        answer();
        if (startsTurn) {
          setTimeout(
            () => startGoalTurnFromGoal(threadId),
            Number(process.env.CEPTION_FAKE_GOAL_TURN_DELAY_MS ?? 100)
          );
        }
        break;
      }

      case "thread/goal/get": {
        const threadId = message.params.threadId;
        send({ id: message.id, result: { goal: goal ? buildGoal(threadId) : null } });
        break;
      }

      case "thread/goal/clear": {
        const threadId = message.params.threadId;
        const cleared = Boolean(goal);
        goal = null;
        send({ id: message.id, result: { cleared } });
        send({ method: "thread/goal/cleared", params: { threadId } });
        break;
      }

      case "turn/steer": {
        const steerPrompt = promptText(message.params.input);
        updateState((state) => {
          state.steers.push({
            threadId: message.params.threadId,
            expectedTurnId: message.params.expectedTurnId,
            prompt: steerPrompt
          });
        });
        send({ id: message.id, result: { turnId: message.params.expectedTurnId } });
        if (activeTurn) {
          const steered = activeTurn;
          setTimeout(() => {
            completeTurn({
              threadId: steered.threadId,
              turnId: steered.turnId,
              prompt: steered.prompt,
              message: `Steered response.\nSteer: ${steerPrompt}`
            });
            // A goal would keep starting turns; call it done so the steered
            // turn is the end of the run.
            if (goal?.status === "active") {
              setGoalStatus(steered.threadId, "complete");
            }
          }, 100);
        }
        break;
      }

      case "turn/interrupt": {
        updateState((state) => {
          state.interrupts.push(message.params);
        });
        if (BEHAVIOR === "interrupt-reject") {
          send({ id: message.id, error: { code: -32000, message: "turn no longer active" } });
          break;
        }
        send({ id: message.id, result: {} });
        if (activeTurn) {
          completeTurn({
            threadId: activeTurn.threadId,
            turnId: activeTurn.turnId,
            prompt: activeTurn.prompt,
            status: "interrupted",
            message: "Interrupted by fixture."
          });
        }
        break;
      }

      default:
        send({ id: message.id, error: { code: -32601, message: `unsupported ${message.method}` } });
    }
  } catch (error) {
    send({ id: message.id, error: { code: -32000, message: error.message } });
  }
});
