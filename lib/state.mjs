import crypto from "node:crypto";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import process from "node:process";

import { detectClaudeAncestor, isSameProcess } from "./claudepid.mjs";
import { acquirePidLock } from "./lock.mjs";

export function validateLabel(label) {
  if (!/^[A-Za-z0-9._-]+$/.test(label)) {
    const error = new Error("label must contain only letters, numbers, dot, underscore, and dash");
    error.exitCode = 4;
    throw error;
  }
}

export function stateRoot() {
  return path.join(os.homedir(), ".local", "state", "ception");
}

export function runtimeRoot() {
  return process.env.XDG_RUNTIME_DIR
    ? path.join(process.env.XDG_RUNTIME_DIR, "ception")
    : path.join(stateRoot(), "run");
}

export function projectHash(realCwd) {
  return crypto.createHash("sha256").update(realCwd).digest("hex").slice(0, 12);
}

const VCS_MARKERS = [".jj", ".git", ".hg"];

export async function realProjectCwd(cwd) {
  const real = await fs.realpath(cwd);
  let dir = real;
  for (;;) {
    for (const marker of VCS_MARKERS) {
      try {
        await fs.access(path.join(dir, marker));
        return dir;
      } catch {
        // keep walking
      }
    }
    const parent = path.dirname(dir);
    if (parent === dir) {
      return real;
    }
    dir = parent;
  }
}

export function sessionKeyFor(watched) {
  return watched ? `c${watched.pid}-${watched.starttime}` : "default";
}

export function parseSessionKey(sessionKey) {
  const match = /^c(\d+)-(\d+)$/.exec(sessionKey ?? "");
  return match ? { pid: Number(match[1]), starttime: match[2] } : null;
}

export async function sessionAlive(sessionKey) {
  const parsed = parseSessionKey(sessionKey);
  if (!parsed) {
    return false;
  }
  return isSameProcess(parsed.pid, parsed.starttime);
}

export function socketPathFor(hash, sessionKey, label) {
  return path.join(runtimeRoot(), `${hash}-${sessionKey}-${label}.sock`);
}

export async function contextFor(cwd, label) {
  validateLabel(label);
  const realCwd = await realProjectCwd(cwd);
  const hash = projectHash(realCwd);
  const watched = await detectClaudeAncestor();
  const sessionKey = sessionKeyFor(watched);
  const runDir = runtimeRoot();
  const root = stateRoot();
  const logsDir = path.join(root, "logs");
  await fs.mkdir(runDir, { recursive: true, mode: 0o700 });
  await fs.mkdir(logsDir, { recursive: true, mode: 0o700 });
  return {
    cwd: realCwd,
    label,
    hash,
    watched,
    sessionKey,
    runDir,
    stateDir: root,
    logsDir,
    socketPath: socketPathFor(hash, sessionKey, label),
    lockPath: path.join(runDir, `${hash}-${sessionKey}-${label}.lock`),
    statePath: path.join(root, `${hash}.json`),
    logPath: path.join(logsDir, `${hash}-${sessionKey}-${label}.log`)
  };
}

function normalizeState(state, cwd = null) {
  state.cwd ??= cwd;
  state.sessions ??= {};
  if (state.labels) {
    // Legacy pre-session shape: fold top-level labels into the default session.
    state.sessions.default ??= { labels: {} };
    state.sessions.default.labels = { ...state.labels, ...state.sessions.default.labels };
    delete state.labels;
  }
  for (const session of Object.values(state.sessions)) {
    session.labels ??= {};
  }
  return state;
}

export async function readProjectStateByPath(statePath, cwd = null) {
  try {
    return normalizeState(JSON.parse(await fs.readFile(statePath, "utf8")), cwd);
  } catch (error) {
    if (error.code === "ENOENT") {
      return normalizeState({}, cwd);
    }
    throw error;
  }
}

export async function readProjectState(context) {
  return readProjectStateByPath(context.statePath, context.cwd);
}

async function writeProjectStateByPath(statePath, state) {
  await fs.mkdir(path.dirname(statePath), { recursive: true, mode: 0o700 });
  for (const [key, session] of Object.entries(state.sessions ?? {})) {
    if (Object.keys(session.labels ?? {}).length === 0) {
      delete state.sessions[key];
    }
  }
  const tempPath = `${statePath}.${process.pid}.${Date.now()}.tmp`;
  await fs.writeFile(tempPath, `${JSON.stringify(state, null, 2)}\n`, { mode: 0o600 });
  await fs.rename(tempPath, statePath);
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function withStateLock(statePath, fn, timeoutMs = 10000) {
  const lockPath = `${statePath}.lock`;
  await fs.mkdir(path.dirname(statePath), { recursive: true, mode: 0o700 });
  const release = await acquirePidLock(lockPath, { timeoutMs, onBusy: () => sleep(25) });
  try {
    return await fn();
  } finally {
    await release();
  }
}

export async function updateLabelState(context, labelPatch) {
  return withStateLock(context.statePath, async () => {
    const state = await readProjectStateByPath(context.statePath, context.cwd);
    state.cwd = context.cwd;
    const session = (state.sessions[context.sessionKey] ??= { labels: {} });
    const current = session.labels[context.label] ?? {};
    session.labels[context.label] = {
      ...current,
      ...labelPatch,
      lastUsed: labelPatch.lastUsed ?? new Date().toISOString(),
      logPath: labelPatch.logPath ?? current.logPath ?? context.logPath
    };
    await writeProjectStateByPath(context.statePath, state);
    return session.labels[context.label];
  });
}

export async function removeLabelState(context) {
  await withStateLock(context.statePath, async () => {
    const state = await readProjectStateByPath(context.statePath, context.cwd);
    delete state.sessions[context.sessionKey]?.labels?.[context.label];
    await writeProjectStateByPath(context.statePath, state);
  });
}

export async function readLabelState(context) {
  const state = await readProjectState(context);
  return state.sessions[context.sessionKey]?.labels?.[context.label] ?? null;
}

// Move a label owned by another (dead) session into ours. Returns the entry,
// or null if it disappeared before we got the lock.
export async function takeLabel(context, fromSessionKey) {
  return withStateLock(context.statePath, async () => {
    const state = await readProjectStateByPath(context.statePath, context.cwd);
    const entry = state.sessions[fromSessionKey]?.labels?.[context.label];
    if (!entry) {
      return null;
    }
    delete state.sessions[fromSessionKey].labels[context.label];
    const session = (state.sessions[context.sessionKey] ??= { labels: {} });
    session.labels[context.label] = { ...entry, lastUsed: new Date().toISOString() };
    await writeProjectStateByPath(context.statePath, state);
    return session.labels[context.label];
  });
}

// Drop labels whose owning session is gone and that haven't been used in
// maxAgeDays; delete their logs. Sockets and locks live in the runtime dir and
// are left to the OS.
export async function gcProjectState(context) {
  const maxAgeDays = Number(process.env.CEPTION_GC_DAYS ?? 7);
  const cutoff = Date.now() - maxAgeDays * 86400_000;

  const collect = async (state) => {
    const stale = [];
    for (const [sessionKey, session] of Object.entries(state.sessions)) {
      if (sessionKey === context.sessionKey) {
        continue;
      }
      for (const [label, entry] of Object.entries(session.labels)) {
        const lastUsed = Date.parse(entry.lastUsed ?? "") || 0;
        if (lastUsed >= cutoff) {
          continue;
        }
        if (parseSessionKey(sessionKey) && (await sessionAlive(sessionKey))) {
          continue;
        }
        stale.push({ sessionKey, label, logPath: entry.logPath });
      }
    }
    return stale;
  };

  // Cheap unlocked probe first; take the lock only when there is work.
  const probe = await collect(await readProjectState(context));
  if (probe.length === 0) {
    return;
  }
  const removed = await withStateLock(context.statePath, async () => {
    const state = await readProjectStateByPath(context.statePath, context.cwd);
    const stale = await collect(state);
    for (const { sessionKey, label } of stale) {
      delete state.sessions[sessionKey].labels[label];
    }
    await writeProjectStateByPath(context.statePath, state);
    return stale;
  });
  for (const { logPath } of removed) {
    if (logPath) {
      await fs.rm(logPath, { force: true }).catch(() => {});
    }
  }
}

export async function listStateFiles() {
  try {
    const entries = await fs.readdir(stateRoot(), { withFileTypes: true });
    return entries
      .filter((entry) => entry.isFile() && /^[0-9a-f]{12}\.json$/.test(entry.name))
      .map((entry) => path.join(stateRoot(), entry.name));
  } catch (error) {
    if (error.code === "ENOENT") {
      return [];
    }
    throw error;
  }
}

function labelRows(hash, cwd, state) {
  const rows = [];
  for (const [sessionKey, session] of Object.entries(state.sessions ?? {})) {
    for (const [label, data] of Object.entries(session.labels ?? {})) {
      rows.push({
        hash,
        cwd: state.cwd ?? cwd,
        sessionKey,
        label,
        ...data,
        socketPath: socketPathFor(hash, sessionKey, label)
      });
    }
  }
  return rows;
}

export async function listProjectLabels(context) {
  const state = await readProjectState(context);
  return labelRows(context.hash, context.cwd, state);
}

export async function listAllLabels() {
  const files = await listStateFiles();
  const rows = [];
  for (const file of files) {
    const hash = path.basename(file, ".json");
    const state = await readProjectStateByPath(file);
    rows.push(...labelRows(hash, null, state));
  }
  return rows;
}
