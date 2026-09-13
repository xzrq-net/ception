import crypto from "node:crypto";
import fs from "node:fs/promises";
import process from "node:process";

import { getProcStarttime, isSameProcess } from "./claudepid.mjs";

export function pidAlive(pid) {
  if (!pid) {
    return false;
  }
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

// A pid alone does not identify a process: the state dir is shared between
// containers with their own pid namespaces, and a container restart hands the
// low pids out again. The holder line is "<pid> <starttime> <namespace>":
// starttime catches reuse within a namespace, the namespace id (boot id plus
// pid-namespace inode) says whether we can check the first two at all.
async function readNamespaceId() {
  try {
    const bootId = (await fs.readFile("/proc/sys/kernel/random/boot_id", "utf8")).trim();
    const pidNs = await fs.readlink("/proc/self/ns/pid");
    return `${bootId}/${pidNs}`;
  } catch {
    return "unknown";
  }
}

let namespaceId = null;

export async function holderLine(pid = process.pid) {
  namespaceId ??= await readNamespaceId();
  return `${pid} ${await getProcStarttime(pid)} ${namespaceId}\n`;
}

function parseHolder(text) {
  const [pid, starttime, ns] = text.trim().split(/\s+/);
  return pid && starttime && ns ? { pid: Number(pid), starttime, ns } : null;
}

// Live holder in our namespace: authoritative. Anything else (another
// container, or a legacy bare-pid lock) can only be judged by age.
async function holderAlive(lockPath, staleMs) {
  let text;
  let mtimeMs;
  try {
    [text, { mtimeMs }] = await Promise.all([fs.readFile(lockPath, "utf8"), fs.stat(lockPath)]);
  } catch {
    return false; // released between link and read; the steal path handles it
  }
  const holder = parseHolder(text);
  namespaceId ??= await readNamespaceId();
  if (holder && holder.ns === namespaceId) {
    return isSameProcess(holder.pid, holder.starttime);
  }
  return Date.now() - mtimeMs < staleMs;
}

// Two stealers can judge the same debris stale; the slower one then unlinks
// the fresh lock the faster one just published. A stealer therefore waits
// this long after linking and confirms the lock is still its own. The gap
// between a stealer's judgment and its unlink is one event-loop turn, so
// this outlasts it by orders of magnitude.
const STEAL_SETTLE_MS = 50;

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

// The lock is published by link(2) with the holder line already written.
// Returns a release function. `onBusy` runs while a live holder exists — it
// backs off, or returns a non-undefined value to abort the acquisition with
// it. `staleMs` bounds how long a holder we cannot verify is believed.
export async function acquirePidLock(lockPath, { timeoutMs, onBusy, staleMs = 30_000 }) {
  const mine = await holderLine();
  const tempPath = `${lockPath}.${crypto.randomBytes(6).toString("hex")}.tmp`;
  await fs.writeFile(tempPath, mine, { mode: 0o600 });
  const deadline = Date.now() + timeoutMs;
  let stole = false;
  try {
    while (Date.now() < deadline) {
      try {
        await fs.link(tempPath, lockPath);
      } catch (error) {
        if (error.code !== "EEXIST") {
          throw error;
        }
        if (!(await holderAlive(lockPath, staleMs))) {
          await fs.rm(lockPath, { force: true }).catch(() => {});
          stole = true;
          continue;
        }
        const abort = await onBusy();
        if (abort !== undefined) {
          return abort;
        }
        continue;
      }
      if (stole) {
        await sleep(STEAL_SETTLE_MS);
        if ((await fs.readFile(lockPath, "utf8").catch(() => null)) !== mine) {
          continue; // a rival stealer took it; it is live and ours to wait on
        }
      }
      return async () => {
        await fs.rm(lockPath, { force: true }).catch(() => {});
      };
    }
    throw new Error(`timeout acquiring ${lockPath}`);
  } finally {
    await fs.rm(tempPath, { force: true }).catch(() => {});
  }
}
