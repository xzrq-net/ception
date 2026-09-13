// One contender for the lock stress test: take the lock, hold it briefly,
// report whether anyone else was holding it at the same time. Usage:
//   node lock-contender.mjs <lockPath> <holdMs>
import fs from "node:fs/promises";
import process from "node:process";

import { acquirePidLock } from "../lib/lock.mjs";

const [lockPath, holdMs] = process.argv.slice(2);
const marker = `${lockPath}.holding`;
const release = await acquirePidLock(lockPath, {
  timeoutMs: 10_000,
  staleMs: 5000,
  onBusy: () => new Promise((resolve) => setTimeout(resolve, 5))
});
let overlap = false;
try {
  await fs.writeFile(marker, String(process.pid), { flag: "wx" });
} catch {
  overlap = true;
}
await new Promise((resolve) => setTimeout(resolve, Number(holdMs)));
if (!overlap) {
  await fs.rm(marker, { force: true });
}
await release();
process.stdout.write(overlap ? "overlap\n" : "ok\n");
