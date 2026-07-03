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
  fs.writeFileSync(STATE_PATH, `${JSON.stringify(state, null, 2)}\n`);
}

function updateState(fn) {
  const state = loadState();
  fn(state);
  saveState(state);
  return state;
}

function send(message) {
  process.stdout.write(`${JSON.stringify(message)}\n`);
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
          setTimeout(
            () =>
              completeTurn({
                threadId: activeTurn.threadId,
                turnId: activeTurn.turnId,
                prompt: activeTurn.prompt,
                message: `Steered response.\nSteer: ${steerPrompt}`
              }),
            100
          );
        }
        break;
      }

      case "turn/interrupt": {
        updateState((state) => {
          state.interrupts.push(message.params);
        });
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
