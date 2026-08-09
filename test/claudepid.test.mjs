import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { detectClaudeAncestor } from "../lib/claudepid.mjs";

async function ppidOf(pid) {
  const stat = await fs.readFile(`/proc/${pid}/stat`, "utf8");
  return Number(stat.slice(stat.lastIndexOf(")") + 2).trim().split(/\s+/)[1]);
}

// Two nested claude-looking processes with a leaf below them.
async function nestedClaudeChain() {
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), "ception-pid-"));
  const script = path.join(dir, "claude");
  await fs.writeFile(script, '#!/usr/bin/env bash\n"$@"\n');
  await fs.chmod(script, 0o755);

  // write, not console.log: the latter inspects the number and can colour it.
  const leafSource = 'process.stdout.write(`${process.pid}\\n`); setInterval(() => {}, 1000);';
  // detached: its own process group, so the teardown kill cannot reach ours.
  const outer = spawn(script, [script, process.execPath, "-e", leafSource], {
    stdio: ["ignore", "pipe", "ignore"],
    detached: true
  });
  const leafPid = await new Promise((resolve) => {
    outer.stdout.setEncoding("utf8");
    outer.stdout.once("data", (chunk) => resolve(Number(chunk.trim())));
  });

  return {
    outerPid: outer.pid,
    innerPid: await ppidOf(leafPid),
    leafPid,
    cleanup: async () => {
      try {
        process.kill(-outer.pid, "SIGKILL");
      } catch {
        // Already gone.
      }
      await fs.rm(dir, { recursive: true, force: true });
    }
  };
}

test("the nearest claude-looking ancestor does not win over an outer one", async (t) => {
  const chain = await nestedClaudeChain();
  t.after(chain.cleanup);

  const detected = await detectClaudeAncestor(chain.leafPid);

  // Which process wins depends on whether this suite itself runs under a real
  // claude session, so assert the regression rather than a particular pid.
  assert.ok(detected, "expected a claude ancestor");
  assert.notEqual(detected.pid, chain.innerPid);
  assert.notEqual(detected.pid, chain.leafPid);
});

test("an explicit pin short-circuits ancestor detection", async () => {
  process.env.CEPTION_WATCH_PID = "4242";
  process.env.CEPTION_WATCH_STARTTIME = "99";
  try {
    assert.deepEqual(await detectClaudeAncestor(), { pid: 4242, starttime: "99" });
  } finally {
    delete process.env.CEPTION_WATCH_PID;
    delete process.env.CEPTION_WATCH_STARTTIME;
  }
});
