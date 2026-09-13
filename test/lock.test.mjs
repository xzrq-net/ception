import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import test from "node:test";
import { fileURLToPath } from "node:url";

import { acquirePidLock, holderLine } from "../lib/lock.mjs";

const CONTENDER = fileURLToPath(new URL("./lock-contender.mjs", import.meta.url));

async function lockDir(t) {
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), "ception-lock-"));
  t.after(() => fs.rm(dir, { recursive: true, force: true }));
  return path.join(dir, "state.json.lock");
}

async function age(file, ms) {
  const then = new Date(Date.now() - ms);
  await fs.utimes(file, then, then);
}

function busyCounter() {
  let calls = 0;
  return {
    onBusy: () => {
      calls += 1;
      return new Promise((resolve) => setTimeout(resolve, 10));
    },
    get calls() {
      return calls;
    }
  };
}

test("a legacy bare-pid lock is judged by age, not by whether the pid exists", async (t) => {
  const lockPath = await lockDir(t);
  // Our own pid is as alive as a pid gets; the old check would have waited.
  await fs.writeFile(lockPath, `${process.pid}\n`);
  await age(lockPath, 60_000);
  const busy = busyCounter();
  const release = await acquirePidLock(lockPath, { timeoutMs: 2000, staleMs: 5000, onBusy: busy.onBusy });
  assert.equal(busy.calls, 0);
  assert.equal((await fs.readFile(lockPath, "utf8")).split(" ")[0], String(process.pid));
  await release();
  await assert.rejects(fs.stat(lockPath));
});

test("a fresh lock from another namespace is believed until it goes stale", async (t) => {
  const lockPath = await lockDir(t);
  await fs.writeFile(lockPath, `${process.pid} 1 otherboot/pid:[1]\n`);
  const busy = busyCounter();
  await assert.rejects(
    acquirePidLock(lockPath, { timeoutMs: 300, staleMs: 60_000, onBusy: busy.onBusy }),
    /timeout acquiring/
  );
  assert.ok(busy.calls > 0);

  await age(lockPath, 120_000);
  const release = await acquirePidLock(lockPath, { timeoutMs: 300, staleMs: 60_000, onBusy: busy.onBusy });
  await release();
});

test("a dead holder in our namespace is stolen at once regardless of age", async (t) => {
  const lockPath = await lockDir(t);
  const mine = await holderLine();
  // Same pid, wrong starttime: the pid was reused after the holder died.
  await fs.writeFile(lockPath, mine.replace(/^(\d+) \d+/, "$1 1"));
  const busy = busyCounter();
  const release = await acquirePidLock(lockPath, { timeoutMs: 2000, staleMs: 3600_000, onBusy: busy.onBusy });
  assert.equal(busy.calls, 0);
  await release();
});

test("a live holder in our namespace blocks even when the file is old", async (t) => {
  const lockPath = await lockDir(t);
  await fs.writeFile(lockPath, await holderLine());
  await age(lockPath, 3600_000);
  const busy = busyCounter();
  await assert.rejects(
    acquirePidLock(lockPath, { timeoutMs: 300, staleMs: 1000, onBusy: busy.onBusy }),
    /timeout acquiring/
  );
  assert.ok(busy.calls > 0);
});

test("onBusy can abort the acquisition with a value", async (t) => {
  const lockPath = await lockDir(t);
  await fs.writeFile(lockPath, await holderLine());
  const result = await acquirePidLock(lockPath, { timeoutMs: 2000, onBusy: async () => null });
  assert.equal(result, null);
  assert.equal(await fs.readFile(lockPath, "utf8"), await holderLine());
});

// Separate processes: a holder is identified by process, so in-process
// contenders would all look like the same holder to the settle check.
function contend(lockPath, delayMs) {
  return new Promise((resolve, reject) => {
    setTimeout(() => {
      execFile(process.execPath, [CONTENDER, lockPath, "30"], (error, stdout) => {
        if (error) {
          reject(error);
        } else {
          resolve(stdout.trim());
        }
      });
    }, delayMs);
  });
}

// Sanity, not proof: the judge-then-unlink gap the settle wait guards is one
// event-loop turn, far below the jitter of process startup, so this does not
// reproduce the race even with the settle disabled.
test("contended stealers of one stale lock never hold it together", async (t) => {
  for (let round = 0; round < 3; round++) {
    const lockPath = await lockDir(t);
    await fs.writeFile(lockPath, "2476\n");
    await age(lockPath, 60_000);
    // Staggered so late contenders judge the debris stale after an earlier
    // one has already replaced it.
    const outcomes = await Promise.all(Array.from({ length: 6 }, (_, i) => contend(lockPath, i * 4)));
    assert.deepEqual(outcomes, Array(6).fill("ok"));
  }
});
