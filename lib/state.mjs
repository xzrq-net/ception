import crypto from "node:crypto";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";

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

export async function realProjectCwd(cwd) {
  return fs.realpath(cwd);
}

export async function contextFor(cwd, label) {
  validateLabel(label);
  const realCwd = await realProjectCwd(cwd);
  const hash = projectHash(realCwd);
  const runDir = runtimeRoot();
  const root = stateRoot();
  const logsDir = path.join(root, "logs");
  await fs.mkdir(runDir, { recursive: true, mode: 0o700 });
  await fs.mkdir(logsDir, { recursive: true, mode: 0o700 });
  return {
    cwd: realCwd,
    label,
    hash,
    runDir,
    stateDir: root,
    logsDir,
    socketPath: path.join(runDir, `${hash}-${label}.sock`),
    lockPath: path.join(runDir, `${hash}-${label}.lock`),
    statePath: path.join(root, `${hash}.json`),
    logPath: path.join(logsDir, `${hash}-${label}.log`)
  };
}

export async function readProjectStateByPath(statePath, cwd = null) {
  try {
    return JSON.parse(await fs.readFile(statePath, "utf8"));
  } catch (error) {
    if (error.code === "ENOENT") {
      return { cwd, labels: {} };
    }
    throw error;
  }
}

export async function readProjectState(context) {
  const state = await readProjectStateByPath(context.statePath, context.cwd);
  state.cwd ??= context.cwd;
  state.labels ??= {};
  return state;
}

export async function writeProjectState(context, state) {
  await fs.mkdir(path.dirname(context.statePath), { recursive: true, mode: 0o700 });
  const tempPath = `${context.statePath}.${process.pid}.${Date.now()}.tmp`;
  await fs.writeFile(tempPath, `${JSON.stringify(state, null, 2)}\n`, { mode: 0o600 });
  await fs.rename(tempPath, context.statePath);
}

export async function updateLabelState(context, labelPatch) {
  const state = await readProjectState(context);
  state.cwd = context.cwd;
  state.labels ??= {};
  const current = state.labels[context.label] ?? {};
  state.labels[context.label] = {
    ...current,
    ...labelPatch,
    lastUsed: labelPatch.lastUsed ?? new Date().toISOString(),
    logPath: labelPatch.logPath ?? current.logPath ?? context.logPath
  };
  await writeProjectState(context, state);
  return state.labels[context.label];
}

export async function removeLabelState(context) {
  const state = await readProjectState(context);
  if (state.labels) {
    delete state.labels[context.label];
  }
  await writeProjectState(context, state);
}

export async function readLabelState(context) {
  const state = await readProjectState(context);
  return state.labels?.[context.label] ?? null;
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

export async function listProjectLabels(context) {
  const state = await readProjectState(context);
  return Object.entries(state.labels ?? {}).map(([label, data]) => ({
    hash: context.hash,
    cwd: state.cwd ?? context.cwd,
    label,
    ...data,
    socketPath: path.join(runtimeRoot(), `${context.hash}-${label}.sock`)
  }));
}

export async function listAllLabels() {
  const files = await listStateFiles();
  const rows = [];
  for (const file of files) {
    const hash = path.basename(file, ".json");
    const state = await readProjectStateByPath(file);
    for (const [label, data] of Object.entries(state.labels ?? {})) {
      rows.push({
        hash,
        cwd: state.cwd,
        label,
        ...data,
        socketPath: path.join(runtimeRoot(), `${hash}-${label}.sock`)
      });
    }
  }
  return rows;
}

export function sandboxModeToPolicy(mode, cwd) {
  switch (mode) {
    case "read-only":
      return { type: "readOnly", networkAccess: true };
    case "danger-full-access":
      return { type: "dangerFullAccess" };
    case "workspace-write":
    default:
      return {
        type: "workspaceWrite",
        writableRoots: [cwd],
        networkAccess: true,
        excludeTmpdirEnvVar: false,
        excludeSlashTmp: false
      };
  }
}
