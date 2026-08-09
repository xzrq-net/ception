import fs from "node:fs/promises";
import path from "node:path";

async function readProcStat(pid) {
  const stat = await fs.readFile(`/proc/${pid}/stat`, "utf8");
  const end = stat.lastIndexOf(")");
  const fields = stat.slice(end + 2).trim().split(/\s+/);
  return {
    pid: Number(pid),
    ppid: Number(fields[1]),
    starttime: fields[19]
  };
}

async function readCmdline(pid) {
  try {
    const raw = await fs.readFile(`/proc/${pid}/cmdline`);
    return raw.toString("utf8").split("\0").filter(Boolean);
  } catch {
    return [];
  }
}

async function readEnviron(pid) {
  try {
    return await fs.readFile(`/proc/${pid}/environ`, "utf8");
  } catch {
    return "";
  }
}

function isClaudeCmdline(argv) {
  return argv.some((arg, index) => {
    if (index === 0 && path.basename(arg) === "claude") {
      return true;
    }
    return arg === "claude" || arg.endsWith("/claude");
  });
}

export async function getProcStarttime(pid) {
  return (await readProcStat(pid)).starttime;
}

export async function isSameProcess(pid, starttime) {
  try {
    return (await getProcStarttime(pid)) === String(starttime);
  } catch {
    return false;
  }
}

export async function detectClaudeAncestor(startPid = process.pid) {
  if (process.env.CEPTION_WATCH_PID && process.env.CEPTION_WATCH_STARTTIME) {
    return {
      pid: Number(process.env.CEPTION_WATCH_PID),
      starttime: String(process.env.CEPTION_WATCH_STARTTIME)
    };
  }

  const chain = [];
  let pid = Number(startPid);
  const seen = new Set();
  // The outermost match, not the nearest: Claude Code nests shorter-lived
  // claude-looking helpers under the real session, and keying on the nearest
  // one makes the session change underfoot. Costs nested sessions, which
  // collapse onto their parent (see README); CEPTION_WATCH_PID overrides.
  let outermost = null;

  while (pid > 1 && !seen.has(pid)) {
    seen.add(pid);
    let stat;
    try {
      stat = await readProcStat(pid);
    } catch {
      break;
    }
    chain.push(stat);
    const argv = await readCmdline(pid);
    if (isClaudeCmdline(argv)) {
      outermost = stat;
    }
    pid = stat.ppid;
  }
  if (outermost) {
    return { pid: outermost.pid, starttime: outermost.starttime };
  }

  let topmostClaudeEnv = null;
  for (const entry of chain) {
    const environ = await readEnviron(entry.pid);
    if (environ.includes("CLAUDECODE=1")) {
      topmostClaudeEnv = entry;
    }
  }
  if (topmostClaudeEnv?.ppid) {
    try {
      const parent = await readProcStat(topmostClaudeEnv.ppid);
      return { pid: parent.pid, starttime: parent.starttime };
    } catch {
      return null;
    }
  }

  return null;
}
