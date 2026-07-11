import { spawn } from "node:child_process";
import fsSync from "node:fs";
import fs from "node:fs/promises";
import net from "node:net";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { statusExitCode } from "./render.mjs";
import {
  contextFor,
  gcProjectState,
  listAllLabels,
  listProjectLabels,
  readLabelState,
  sessionAlive,
  takeLabel
} from "./state.mjs";

const BIN_PATH = fileURLToPath(new URL("../bin/ception.mjs", import.meta.url));

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function isPidAlive(pid) {
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

function usageError(message) {
  const error = new Error(message);
  error.exitCode = 4;
  return error;
}

function socketConnect(socketPath) {
  return new Promise((resolve, reject) => {
    const socket = net.createConnection({ path: socketPath });
    socket.once("connect", () => resolve(socket));
    socket.once("error", reject);
  });
}

async function waitForSocket(socketPath, timeoutMs = 15000, shouldAbort = null) {
  const deadline = Date.now() + timeoutMs;
  let lastError = null;
  while (Date.now() < deadline) {
    const abort = shouldAbort?.();
    if (abort) {
      throw abort;
    }
    try {
      const socket = await socketConnect(socketPath);
      socket.end();
      return;
    } catch (error) {
      lastError = error;
      if (error.code === "ECONNREFUSED") {
        await fs.rm(socketPath, { force: true }).catch(() => {});
      }
      await sleep(100);
    }
  }
  throw lastError ?? new Error(`timeout waiting for ${socketPath}`);
}

async function commandDaemon(socketPath, command) {
  const socket = await socketConnect(socketPath);
  socket.setEncoding("utf8");
  const result = await new Promise((resolve, reject) => {
    let buffer = "";
    let settled = false;
    const finish = (fn, value) => {
      if (settled) {
        return;
      }
      settled = true;
      fn(value);
    };
    socket.on("data", (chunk) => {
      buffer += chunk;
      let newline = buffer.indexOf("\n");
      while (newline !== -1) {
        const line = buffer.slice(0, newline);
        buffer = buffer.slice(newline + 1);
        newline = buffer.indexOf("\n");
        if (!line.trim()) {
          continue;
        }
        let message;
        try {
          message = JSON.parse(line);
        } catch (error) {
          finish(reject, new Error(`invalid daemon JSON: ${error.message}`));
          socket.destroy();
          return;
        }
        if (message.type === "error") {
          const error = new Error(message.message);
          error.exitCode = 4;
          finish(reject, error);
          socket.destroy();
          return;
        }
        if (message.type === "result") {
          finish(resolve, message);
          socket.end();
          return;
        }
      }
    });
    socket.on("error", (error) => finish(reject, error));
    socket.on("close", () => finish(reject, new Error("daemon connection closed before result")));
    socket.write(`${JSON.stringify(command)}\n`);
  });
  return result;
}

async function daemonStatus(contextOrRow) {
  try {
    const response = await commandDaemon(contextOrRow.socketPath, { cmd: "status" });
    return response.daemon ?? null;
  } catch {
    return null;
  }
}

async function acquireLock(context, timeoutMs = 15000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const handle = await fs.open(context.lockPath, "wx", 0o600);
      await handle.writeFile(`${process.pid}\n`);
      return async () => {
        await handle.close().catch(() => {});
        await fs.rm(context.lockPath, { force: true }).catch(() => {});
      };
    } catch (error) {
      if (error.code !== "EEXIST") {
        throw error;
      }
      let pid = 0;
      try {
        pid = Number((await fs.readFile(context.lockPath, "utf8")).trim());
      } catch {
        // Broken lockfile. Try to steal it.
      }
      if (!isPidAlive(pid)) {
        await fs.rm(context.lockPath, { force: true }).catch(() => {});
        continue;
      }
      try {
        await waitForSocket(context.socketPath, 500);
        return null;
      } catch {
        await sleep(100);
      }
    }
  }
  throw new Error(`timeout acquiring ${context.lockPath}`);
}

async function spawnDaemon(context, { resumeThreadId = null, model = null, effort = null, logPath = null } = {}) {
  const release = await acquireLock(context);
  if (release === null) {
    return { spawned: false };
  }

  try {
    // The lock may have become available because another spawner finished and
    // released it. Recheck while holding the lock before starting a second
    // daemon for the same label.
    if (await daemonStatus(context)) {
      return { spawned: false };
    }

    const daemonLogPath = logPath ?? context.logPath;
    const logFd = fsSync.openSync(daemonLogPath, "a");
    const args = [
      BIN_PATH,
      "daemon",
      "--label",
      context.label,
      "--cwd",
      context.cwd,
      "--hash",
      context.hash,
      "--session",
      context.sessionKey,
      "--socket",
      context.socketPath,
      "--state",
      context.statePath,
      "--log",
      daemonLogPath
    ];
    if (resumeThreadId) {
      args.push("--resume-thread-id", resumeThreadId);
    }
    if (model) {
      args.push("--model", model);
    }
    if (effort) {
      args.push("--effort", effort);
    }

    const env = { ...process.env };
    if (context.watched) {
      env.CEPTION_CLAUDE_PID = String(context.watched.pid);
      env.CEPTION_CLAUDE_STARTTIME = String(context.watched.starttime);
    }

    const child = spawn(process.execPath, args, {
      cwd: context.cwd,
      env,
      detached: true,
      stdio: ["ignore", logFd, logFd]
    });
    let startupFailure = null;
    child.once("exit", (code, signal) => {
      startupFailure = new Error(
        `daemon exited during startup (${signal ?? `exit ${code}`}); see ${daemonLogPath}`
      );
    });
    child.unref();
    fsSync.closeSync(logFd);
    // Generous timeout: first spawn may sit through an npx download of codex.
    const timeoutMs = Number(process.env.CEPTION_SPAWN_TIMEOUT_SECS ?? 120) * 1000;
    await waitForSocket(context.socketPath, timeoutMs, () => startupFailure);
    return { spawned: true };
  } finally {
    await release();
  }
}

async function ensureLiveDaemon(context, options = {}) {
  const live = await daemonStatus(context);
  if (live) {
    return { spawned: false, daemon: live };
  }
  await fs.rm(context.socketPath, { force: true }).catch(() => {});
  const spawnResult = await spawnDaemon(context, options);
  const daemon = await daemonStatus(context);
  if (!daemon) {
    throw new Error("daemon did not become ready");
  }
  return { ...spawnResult, daemon };
}

function printReportAndSetExit(result) {
  if (result.report) {
    process.stdout.write(`${result.report.replace(/\s+$/u, "")}\n`);
  }
  process.exitCode = statusExitCode(result.status);
}

// Find this label under another session and, if its owner is gone, move it
// into ours. Labels owned by a live session are never touched: that would be
// two sessions sharing one rollout.
async function adoptLabel(context) {
  const rows = (await listProjectLabels(context)).filter(
    (row) => row.label === context.label && row.sessionKey !== context.sessionKey && row.threadId
  );
  rows.sort((a, b) => (b.lastUsed ?? "").localeCompare(a.lastUsed ?? ""));
  for (const row of rows) {
    if (await sessionAlive(row.sessionKey)) {
      throw usageError(
        `label ${context.label} belongs to live session ${row.sessionKey}; use a different label`
      );
    }
    if (row.sessionKey === "default" && (await daemonStatus(row))) {
      throw usageError(
        `label ${context.label} has a live daemon outside any Claude session; use a different label or kill it there`
      );
    }
    const entry = await takeLabel(context, row.sessionKey);
    if (entry) {
      return entry;
    }
  }
  return null;
}

export async function cliSpawn({ label, cwd, prompt, model, effort, report }) {
  const context = await contextFor(cwd, label);
  await gcProjectState(context).catch(() => {});
  if (await daemonStatus(context)) {
    throw usageError(`daemon already live for ${label}`);
  }
  await fs.rm(context.socketPath, { force: true }).catch(() => {});
  const spawnResult = await spawnDaemon(context, { model, effort });
  if (!spawnResult.spawned) {
    throw usageError(`daemon already live for ${label}`);
  }
  process.stdout.write(`log: ${context.logPath}\n`);
  const result = await commandDaemon(context.socketPath, { cmd: "turn", prompt, report });
  printReportAndSetExit(result);
}

export async function cliSend({ label, cwd, prompt, report }) {
  const context = await contextFor(cwd, label);
  await gcProjectState(context).catch(() => {});
  const daemon = await daemonStatus(context);
  let logPath = daemon?.logPath ?? context.logPath;
  if (!daemon) {
    await fs.rm(context.socketPath, { force: true }).catch(() => {});
    let labelState = await readLabelState(context);
    if (!labelState?.threadId) {
      labelState = await adoptLabel(context);
    }
    if (!labelState?.threadId) {
      throw usageError(
        `no live daemon or stored thread for ${label} in project ${context.cwd} (session ${context.sessionKey}); labels resolve from the invocation cwd — pass --cwd, or run \`ception list --all\` to see every project`
      );
    }
    await ensureLiveDaemon(context, {
      resumeThreadId: labelState.threadId,
      model: labelState.model ?? null,
      effort: labelState.effort ?? null,
      logPath: labelState.logPath ?? null
    });
    logPath = labelState.logPath ?? context.logPath;
  }
  process.stdout.write(`log: ${logPath}\n`);
  // The daemon decides atomically: steer if a turn is active, else new turn.
  const result = await commandDaemon(context.socketPath, { cmd: "send", prompt, report });
  printReportAndSetExit(result);
}

export async function cliInterrupt({ label, cwd }) {
  const context = await contextFor(cwd, label);
  const live = await daemonStatus(context);
  if (!live) {
    process.stdout.write("no active turn\n");
    return;
  }
  const result = await commandDaemon(context.socketPath, { cmd: "interrupt" });
  printReportAndSetExit(result);
}

export async function cliKill({ label, all, cwd }) {
  if (all) {
    const context = await contextFor(cwd, "_probe");
    const rows = (await listProjectLabels(context)).filter(
      (row) => row.sessionKey === context.sessionKey
    );
    let killed = 0;
    for (const row of rows) {
      const live = await daemonStatus(row);
      if (!live) {
        continue;
      }
      await commandDaemon(row.socketPath, { cmd: "shutdown" }).catch(() => {});
      killed += 1;
    }
    process.stdout.write(`killed ${killed} daemon(s)\n`);
    return;
  }

  const context = await contextFor(cwd, label);
  const live = await daemonStatus(context);
  if (!live) {
    await fs.rm(context.socketPath, { force: true }).catch(() => {});
    process.stdout.write("no live daemon\n");
    return;
  }
  const result = await commandDaemon(context.socketPath, { cmd: "shutdown" });
  printReportAndSetExit(result);
}

export async function cliList({ all, json, cwd }) {
  const context = await contextFor(cwd, "_probe");
  await gcProjectState(context).catch(() => {});
  const rows = all ? await listAllLabels() : await listProjectLabels(context);

  const enriched = [];
  for (const row of rows) {
    const live = await daemonStatus(row);
    const mine = row.sessionKey === context.sessionKey;
    let status;
    if (live) {
      status = live.state;
    } else if (mine || (await sessionAlive(row.sessionKey))) {
      status = "dead";
    } else {
      status = "adoptable";
    }
    enriched.push({
      label: row.label,
      cwd: row.cwd,
      hash: row.hash,
      session: mine ? "mine" : row.sessionKey,
      status,
      threadId: live?.threadId ?? row.threadId ?? null,
      lastUsed: row.lastUsed ?? null,
      logPath: row.logPath ?? null
    });
  }

  if (json) {
    process.stdout.write(`${JSON.stringify(enriched, null, 2)}\n`);
    return;
  }
  for (const row of enriched) {
    process.stdout.write(
      `${row.label}\t${row.session}\t${row.status}\t${row.threadId ?? "-"}\t${row.lastUsed ?? "-"}\t${row.logPath ?? "-"}\n`
    );
  }
}

export async function cliWatch({ label, cwd }) {
  const context = await contextFor(cwd, label);
  let state = await readLabelState(context);
  if (!state?.logPath) {
    // Cross-session peek is fine for a read-only tail.
    const rows = (await listProjectLabels(context)).filter((row) => row.label === label);
    rows.sort((a, b) => (b.lastUsed ?? "").localeCompare(a.lastUsed ?? ""));
    state = rows[0] ?? null;
  }
  const logPath = state?.logPath ?? context.logPath;
  await fs.mkdir(path.dirname(logPath), { recursive: true });
  const child = spawn("tail", ["-F", logPath], { stdio: "inherit" });
  await new Promise((resolve) => child.on("exit", resolve));
}
