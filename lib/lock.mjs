import fs from "node:fs/promises";
import process from "node:process";

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

// The lock is published by link(2) with the pid already written; a lock
// without a readable live pid is debris and gets stolen. Returns a release
// function. `onBusy` runs while a live holder exists — it backs off, or
// returns a non-undefined value to abort the acquisition with it.
export async function acquirePidLock(lockPath, { timeoutMs, onBusy }) {
  const tempPath = `${lockPath}.${process.pid}.tmp`;
  await fs.writeFile(tempPath, `${process.pid}\n`, { mode: 0o600 });
  const deadline = Date.now() + timeoutMs;
  try {
    while (Date.now() < deadline) {
      try {
        await fs.link(tempPath, lockPath);
        return async () => {
          await fs.rm(lockPath, { force: true }).catch(() => {});
        };
      } catch (error) {
        if (error.code !== "EEXIST") {
          throw error;
        }
        let pid = 0;
        try {
          pid = Number((await fs.readFile(lockPath, "utf8")).trim());
        } catch {
          // released between link and read; falls through to the steal path
        }
        if (!pidAlive(pid)) {
          await fs.rm(lockPath, { force: true }).catch(() => {});
          continue;
        }
        const abort = await onBusy();
        if (abort !== undefined) {
          return abort;
        }
      }
    }
    throw new Error(`timeout acquiring ${lockPath}`);
  } finally {
    await fs.rm(tempPath, { force: true }).catch(() => {});
  }
}
