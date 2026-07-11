import assert from "node:assert/strict";
import test from "node:test";

import { TurnAccumulator } from "../lib/render.mjs";

function accumulator() {
  return new TurnAccumulator({
    label: "test",
    cwd: "/tmp/project",
    threadId: "thread",
    turnId: "turn",
    prompt: "prompt",
    logPath: "/tmp/test.log"
  });
}

function completeItem(turn, item) {
  turn.handleNotification("item/completed", { item });
}

function completeTurn(turn) {
  turn.handleNotification("turn/completed", { turn: { status: "completed" } });
}

test("instruction acknowledgement after compaction is treated as a derailed turn", () => {
  const turn = accumulator();
  completeItem(turn, { id: "compaction", type: "contextCompaction" });
  completeItem(turn, {
    id: "message",
    type: "agentMessage",
    text: "Instructions loaded for `/tmp/project`."
  });
  completeTurn(turn);

  assert.equal(turn.status, "failed");
  assert.equal(turn.derailedByCompaction, true);
  assert.match(turn.buildReport("brief"), /WARNING: turn derailed by mid-turn context compaction/);
});

test("instruction acknowledgement without compaction remains a completed turn", () => {
  const turn = accumulator();
  completeItem(turn, { id: "message", type: "agentMessage", text: "Instructions loaded." });
  completeTurn(turn);

  assert.equal(turn.status, "completed");
  assert.equal(turn.derailedByCompaction, false);
});

test("substantive text-only answer after compaction remains completed", () => {
  const turn = accumulator();
  completeItem(turn, { id: "compaction", type: "contextCompaction" });
  completeItem(turn, {
    id: "reasoning",
    type: "reasoning",
    summary: ["Derived the answer"],
    content: []
  });
  completeItem(turn, { id: "message", type: "agentMessage", text: "The answer is 42." });
  completeTurn(turn);

  assert.equal(turn.status, "completed");
  assert.equal(turn.derailedByCompaction, false);
  assert.match(turn.buildReport("brief"), /^The answer is 42\./);
});

test("completion of pre-compaction work does not hide the amnesia signature", () => {
  const turn = accumulator();
  turn.handleNotification("item/started", {
    item: { id: "command", type: "commandExecution", command: "long-running-command" }
  });
  completeItem(turn, { id: "compaction", type: "contextCompaction" });
  completeItem(turn, {
    id: "command",
    type: "commandExecution",
    command: "long-running-command",
    status: "completed",
    exitCode: 0
  });
  completeItem(turn, { id: "message", type: "agentMessage", text: "Instructions loaded." });
  completeTurn(turn);

  assert.equal(turn.status, "failed");
  assert.equal(turn.derailedByCompaction, true);
});

test("only the final compaction's subsequent message determines derailment", () => {
  const turn = accumulator();
  completeItem(turn, { id: "compaction-1", type: "contextCompaction" });
  completeItem(turn, { id: "message-1", type: "agentMessage", text: "Instructions loaded." });
  completeItem(turn, { id: "compaction-2", type: "contextCompaction" });
  completeItem(turn, { id: "message-2", type: "agentMessage", text: "Recovered and completed the task." });
  completeTurn(turn);

  assert.equal(turn.compactions, 2);
  assert.equal(turn.status, "completed");
  assert.equal(turn.derailedByCompaction, false);
});
