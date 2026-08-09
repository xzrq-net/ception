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

test("a long final message survives every report level, full included", () => {
  const turn = accumulator();
  const long = `${"x".repeat(6000)}END`;
  completeItem(turn, { id: "message", type: "agentMessage", text: long });
  completeTurn(turn);

  for (const level of ["brief", "items", "full"]) {
    const report = turn.buildReport(level);
    assert.ok(report.includes(long), `${level} report dropped part of the final message`);
    assert.ok(!report.includes("…"), `${level} report still carries a truncation marker`);
  }
});

test("adopting a continuation clears the derailed verdict from the compacted half", () => {
  const turn = accumulator();
  completeItem(turn, { id: "compaction", type: "contextCompaction" });
  completeItem(turn, { id: "ack", type: "agentMessage", text: "Instructions loaded for `/repo`." });
  completeTurn(turn);
  assert.equal(turn.status, "failed");

  turn.adoptContinuation("turn-2");
  completeItem(turn, { id: "real", type: "agentMessage", text: "Actually finished the work." });
  completeTurn(turn);

  assert.equal(turn.status, "completed");
  assert.equal(turn.derailedByCompaction, false);
  assert.match(turn.buildReport("brief"), /Actually finished the work\./);
  assert.match(turn.buildReport("brief"), /compactions: 1/);
});

test("a turn that ends without an agent message reports what it did instead", () => {
  const accumulator = new TurnAccumulator({
    label: "audit",
    cwd: "/repo",
    threadId: "t1",
    turnId: "turn1",
    prompt: "",
    logPath: "/tmp/x.log"
  });
  accumulator.handleNotification("item/completed", {
    item: { type: "commandExecution", id: "c1", command: "rg TODO", status: "completed", exitCode: 0 }
  });
  accumulator.handleNotification("turn/completed", { turn: { status: "completed" } });

  const report = accumulator.buildReport("brief");
  assert.match(report, /no final message/);
  assert.match(report, /rg TODO/);
});
