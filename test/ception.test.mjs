import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import test from "node:test";

const REPO = path.resolve(new URL("..", import.meta.url).pathname);
const BIN = path.join(REPO, "bin", "ception.mjs");
const FAKE = path.join(REPO, "test", "fake-appserver.mjs");

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function procStarttime(pid) {
  const stat = await fs.readFile(`/proc/${pid}/stat`, "utf8");
  const end = stat.lastIndexOf(")");
  return stat.slice(end + 2).trim().split(/\s+/)[19];
}

async function makeEnv(behavior = "happy") {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "ception-test-"));
  const home = path.join(root, "home");
  const runtime = path.join(root, "run");
  const project = path.join(root, "project");
  await fs.mkdir(home, { recursive: true });
  await fs.mkdir(runtime, { recursive: true });
  await fs.mkdir(project, { recursive: true });
  const fakeState = path.join(root, "fake-state.json");
  // Pin the session identity to the test runner so tests are hermetic even
  // when the suite itself runs under Claude Code.
  const starttime = await procStarttime(process.pid);
  const env = {
    ...process.env,
    HOME: home,
    XDG_RUNTIME_DIR: runtime,
    CEPTION_CODEX_CMD: `${process.execPath} ${FAKE}`,
    CEPTION_FAKE_STATE: fakeState,
    CEPTION_FAKE_BEHAVIOR: behavior,
    CEPTION_IDLE_TIMEOUT_SECS: "30",
    CEPTION_CLAUDE_POLL_SECS: "0.2",
    CEPTION_WATCH_PID: String(process.pid),
    CEPTION_WATCH_STARTTIME: starttime
  };
  const sessionKey = `c${process.pid}-${starttime}`;
  return { root, home, runtime, project, fakeState, env, sessionKey };
}

async function spawnDummySession(ctx) {
  const child = spawn(process.execPath, ["-e", "setInterval(() => {}, 1000)"], {
    stdio: "ignore"
  });
  const starttime = await procStarttime(child.pid);
  return {
    child,
    sessionKey: `c${child.pid}-${starttime}`,
    env: {
      ...ctx.env,
      CEPTION_WATCH_PID: String(child.pid),
      CEPTION_WATCH_STARTTIME: starttime
    }
  };
}

function runCeption(args, { env, cwd, input = null, timeoutMs = 8000 } = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [BIN, ...args], {
      cwd,
      env,
      stdio: ["pipe", "pipe", "pipe"]
    });
    let stdout = "";
    let stderr = "";
    const timer = setTimeout(() => {
      child.kill("SIGTERM");
      reject(new Error(`timeout: ception ${args.join(" ")}\nstdout=${stdout}\nstderr=${stderr}`));
    }, timeoutMs);
    child.stdout.on("data", (chunk) => {
      stdout += chunk;
    });
    child.stderr.on("data", (chunk) => {
      stderr += chunk;
    });
    child.on("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
    child.on("exit", (code, signal) => {
      clearTimeout(timer);
      resolve({ code, signal, stdout, stderr });
    });
    if (input !== null) {
      child.stdin.end(input);
    } else {
      child.stdin.end();
    }
  });
}

function spawnCeption(args, { env, cwd }) {
  const child = spawn(process.execPath, [BIN, ...args], {
    cwd,
    env,
    stdio: ["pipe", "pipe", "pipe"]
  });
  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (chunk) => {
    stdout += chunk;
  });
  child.stderr.on("data", (chunk) => {
    stderr += chunk;
  });
  const done = new Promise((resolve) => {
    child.on("exit", (code, signal) => resolve({ code, signal, stdout, stderr }));
  });
  child.stdin.end();
  return { child, done, get stdout() { return stdout; }, get stderr() { return stderr; } };
}

async function readJson(file, fallback = null) {
  try {
    return JSON.parse(await fs.readFile(file, "utf8"));
  } catch (error) {
    if (error.code === "ENOENT") {
      return fallback;
    }
    throw error;
  }
}

async function readProjectState(ctx) {
  const dir = path.join(ctx.home, ".local", "state", "ception");
  const names = await fs.readdir(dir);
  const stateName = names.find((name) => name.endsWith(".json"));
  return readJson(path.join(dir, stateName));
}

async function waitFor(fn, timeoutMs = 5000) {
  const deadline = Date.now() + timeoutMs;
  let last;
  while (Date.now() < deadline) {
    last = await fn();
    if (last) {
      return last;
    }
    await sleep(50);
  }
  return last;
}

async function cleanup(ctx, envs = []) {
  for (const env of [ctx.env, ...envs]) {
    await runCeption(["kill", "--all"], { env, cwd: ctx.project, timeoutMs: 4000 }).catch(() => {});
  }
}

test("happy path: spawn renders report, persists state, logs reasoning", async (t) => {
  const ctx = await makeEnv("happy");
  t.after(() => cleanup(ctx));

  const result = await runCeption(["spawn", "--label", "alpha", "do the thing"], {
    env: ctx.env,
    cwd: ctx.project
  });

  assert.equal(result.code, 0, result.stderr);
  assert.match(result.stdout, /^log: .+alpha\.log/m);
  assert.equal(result.stdout.match(/^log: /gm).length, 1);
  assert.match(result.stdout, /Handled the requested task/);
  assert.match(result.stdout, /files touched:/);
  assert.doesNotMatch(result.stdout, /threadId:/);

  const projectState = await readProjectState(ctx);
  const entry = projectState.sessions[ctx.sessionKey].labels.alpha;
  assert.equal(entry.threadId, "thr_1");

  const log = await fs.readFile(entry.logPath, "utf8");
  assert.match(log, /Thinking through fixture/);
});

test("send to idle live daemon starts a second turn on same app-server", async (t) => {
  const ctx = await makeEnv("happy");
  t.after(() => cleanup(ctx));

  assert.equal((await runCeption(["spawn", "--label", "idle", "first"], { env: ctx.env, cwd: ctx.project })).code, 0);
  const second = await runCeption(["send", "idle", "follow up"], { env: ctx.env, cwd: ctx.project });
  assert.equal(second.code, 0, second.stderr);
  assert.match(second.stdout, /^log: .+idle\.log/m);
  assert.match(second.stdout, /Resumed the prior run/);

  const fakeState = await readJson(ctx.fakeState);
  assert.equal(fakeState.appServerStarts, 1);
  assert.equal(fakeState.lastTurnStarts.length, 2);
  assert.equal(fakeState.lastTurnStarts[0].threadId, fakeState.lastTurnStarts[1].threadId);
});

test("send during active turn steers and original client completes", async (t) => {
  const ctx = await makeEnv("steer");
  t.after(() => cleanup(ctx));

  const first = spawnCeption(["spawn", "--label", "steer", "slow turn"], {
    env: ctx.env,
    cwd: ctx.project
  });

  await waitFor(async () => {
    const state = await readJson(ctx.fakeState, {});
    return state.lastTurnStarts?.length === 1;
  });

  const steered = await runCeption(["send", "steer", "add steering"], {
    env: ctx.env,
    cwd: ctx.project,
    timeoutMs: 3000
  });
  assert.equal(steered.code, 0, steered.stderr);
  assert.match(steered.stdout, /steered active turn/);

  const completed = await first.done;
  assert.equal(completed.code, 0, completed.stderr);
  assert.match(completed.stdout, /Steered response/);

  const fakeState = await readJson(ctx.fakeState);
  assert.equal(fakeState.steers.length, 1);
  assert.equal(fakeState.steers[0].prompt, "add steering");
});

test("dead daemon plus stored thread respawns and resumes", async (t) => {
  const ctx = await makeEnv("happy");
  t.after(() => cleanup(ctx));

  assert.equal((await runCeption(["spawn", "--label", "resume", "first"], { env: ctx.env, cwd: ctx.project })).code, 0);
  assert.equal((await runCeption(["kill", "resume"], { env: ctx.env, cwd: ctx.project })).code, 0);

  const sent = await runCeption(["send", "resume", "follow up after death"], {
    env: ctx.env,
    cwd: ctx.project
  });
  assert.equal(sent.code, 0, sent.stderr);
  assert.match(sent.stdout, /Resumed the prior run/);

  const fakeState = await readJson(ctx.fakeState);
  assert.equal(fakeState.appServerStarts, 2);
  assert.ok(fakeState.requests.some((request) => request.method === "thread/resume"));
});

test("server-initiated request is rejected and fails the turn", async (t) => {
  const ctx = await makeEnv("server-request");
  t.after(() => cleanup(ctx));

  const result = await runCeption(["spawn", "--label", "approval", "needs approval"], {
    env: ctx.env,
    cwd: ctx.project
  });
  assert.equal(result.code, 4, result.stdout);
  assert.match(result.stderr, /codex sent item\/commandExecution\/requestApproval/);

  const projectState = await readProjectState(ctx);
  const entry = projectState.sessions[ctx.sessionKey].labels.approval;
  const log = await fs.readFile(entry.logPath, "utf8");
  assert.match(log, /\[error\] codex sent item\/commandExecution\/requestApproval/);

  const fakeState = await readJson(ctx.fakeState);
  assert.equal(fakeState.rejectedRequests, 1);
  assert.equal(fakeState.interrupts.length, 1);
});

test("spawn race yields one daemon", async (t) => {
  const ctx = await makeEnv("happy");
  t.after(() => cleanup(ctx));

  const left = runCeption(["spawn", "--label", "race", "left"], { env: ctx.env, cwd: ctx.project });
  const right = runCeption(["spawn", "--label", "race", "right"], { env: ctx.env, cwd: ctx.project });
  const results = await Promise.all([left, right]);
  const codes = results.map((result) => result.code).sort();
  assert.deepEqual(codes, [0, 4]);

  const fakeState = await readJson(ctx.fakeState);
  assert.equal(fakeState.appServerStarts, 1);
});

test("failed turn maps to exit code 2", async (t) => {
  const ctx = await makeEnv("fail");
  t.after(() => cleanup(ctx));

  const result = await runCeption(["spawn", "--label", "boom", "explode"], {
    env: ctx.env,
    cwd: ctx.project
  });
  assert.equal(result.code, 2, result.stderr);
  assert.match(result.stdout, /status: failed/);
});

test("interrupted turn maps to exit code 3", async (t) => {
  const ctx = await makeEnv("steer");
  t.after(() => cleanup(ctx));

  const first = spawnCeption(["spawn", "--label", "stop", "slow turn"], {
    env: ctx.env,
    cwd: ctx.project
  });

  await waitFor(async () => {
    const state = await readJson(ctx.fakeState, {});
    return state.lastTurnStarts?.length === 1;
  });

  const interrupted = await runCeption(["interrupt", "stop"], {
    env: ctx.env,
    cwd: ctx.project,
    timeoutMs: 3000
  });
  assert.equal(interrupted.code, 0, interrupted.stderr);
  assert.match(interrupted.stdout, /interrupt requested/);

  const completed = await first.done;
  assert.equal(completed.code, 3, completed.stderr);
  assert.match(completed.stdout, /status: interrupted/);
});

test("daemon is reparented at spawn and survives spawn client death mid-turn", async (t) => {
  const ctx = await makeEnv("steer");
  t.after(() => cleanup(ctx));

  const first = spawnCeption(["spawn", "--label", "orphan", "slow turn"], {
    env: ctx.env,
    cwd: ctx.project
  });

  await waitFor(async () => {
    const state = await readJson(ctx.fakeState, {});
    return state.lastTurnStarts?.length === 1;
  });

  // The daemon must already be out of the client's process tree.
  const projectState = await readProjectState(ctx);
  const entry = projectState.sessions[ctx.sessionKey].labels.orphan;
  const log = await fs.readFile(entry.logPath, "utf8");
  const daemonPid = Number(log.match(/^\[daemon\] starting .* pid=(\d+)$/m)[1]);
  const stat = await fs.readFile(`/proc/${daemonPid}/stat`, "utf8");
  const ppid = Number(stat.slice(stat.lastIndexOf(")") + 2).split(/\s+/)[1]);
  assert.notEqual(ppid, first.child.pid);

  first.child.kill("SIGKILL");
  await first.done;

  // The turn is still running daemon-side; steer it to completion.
  const steered = await runCeption(["send", "orphan", "add steering"], {
    env: ctx.env,
    cwd: ctx.project,
    timeoutMs: 3000
  });
  assert.equal(steered.code, 0, steered.stderr);
  assert.match(steered.stdout, /steered active turn/);

  const row = await waitFor(async () => {
    const listed = await runCeption(["list", "--json"], { env: ctx.env, cwd: ctx.project });
    const found = JSON.parse(listed.stdout).find((candidate) => candidate.label === "orphan");
    return found?.status === "idle" ? found : null;
  });
  assert.equal(row?.status, "idle");
});

test("respawn after daemon death preserves spawn-time options", async (t) => {
  const ctx = await makeEnv("happy");
  t.after(() => cleanup(ctx));

  const spawned = await runCeption(
    ["spawn", "--label", "opts", "--model", "gpt-fixture", "--effort", "low", "first"],
    { env: ctx.env, cwd: ctx.project }
  );
  assert.equal(spawned.code, 0, spawned.stderr);
  assert.equal((await runCeption(["kill", "opts"], { env: ctx.env, cwd: ctx.project })).code, 0);

  const sent = await runCeption(["send", "opts", "follow up after death"], {
    env: ctx.env,
    cwd: ctx.project
  });
  assert.equal(sent.code, 0, sent.stderr);

  const fakeState = await readJson(ctx.fakeState);
  const last = fakeState.lastTurnStarts.at(-1);
  assert.equal(last.model, "gpt-fixture");
  assert.equal(last.effort, "low");
  assert.equal(last.sandboxPolicy.type, "dangerFullAccess");
});

test("Claude pid watch exits daemon after watched process dies", async (t) => {
  const ctx = await makeEnv("happy");
  const session = await spawnDummySession(ctx);
  t.after(() => {
    session.child.kill("SIGTERM");
    return cleanup(ctx);
  });

  assert.equal((await runCeption(["spawn", "--label", "watch", "first"], { env: session.env, cwd: ctx.project })).code, 0);
  session.child.kill("SIGTERM");

  const row = await waitFor(async () => {
    const listed = await runCeption(["list", "--json"], { env: ctx.env, cwd: ctx.project });
    const rows = JSON.parse(listed.stdout);
    const found = rows.find((candidate) => candidate.label === "watch");
    return found?.status === "adoptable" ? found : null;
  }, 5000);
  assert.equal(row?.status, "adoptable");
});

test("two live sessions use the same label without collision", async (t) => {
  const ctx = await makeEnv("happy");
  const sessionA = await spawnDummySession(ctx);
  const sessionB = await spawnDummySession(ctx);
  t.after(() => {
    sessionA.child.kill("SIGTERM");
    sessionB.child.kill("SIGTERM");
    return cleanup(ctx, [sessionA.env, sessionB.env]);
  });

  const first = await runCeption(["spawn", "--label", "impl", "first"], { env: sessionA.env, cwd: ctx.project });
  assert.equal(first.code, 0, first.stderr);
  const second = await runCeption(["spawn", "--label", "impl", "first"], { env: sessionB.env, cwd: ctx.project });
  assert.equal(second.code, 0, second.stderr);

  const fakeState = await readJson(ctx.fakeState);
  assert.equal(fakeState.appServerStarts, 2);
  const threads = fakeState.lastTurnStarts.map((turn) => turn.threadId);
  assert.notEqual(threads[0], threads[1]);

  // Follow-up in session A lands on A's thread, not B's.
  const followUp = await runCeption(["send", "impl", "follow up"], { env: sessionA.env, cwd: ctx.project });
  assert.equal(followUp.code, 0, followUp.stderr);
  const after = await readJson(ctx.fakeState);
  assert.equal(after.lastTurnStarts.at(-1).threadId, threads[0]);
});

test("send refuses a label owned by a live session", async (t) => {
  const ctx = await makeEnv("happy");
  const sessionA = await spawnDummySession(ctx);
  t.after(() => {
    sessionA.child.kill("SIGTERM");
    return cleanup(ctx, [sessionA.env]);
  });

  assert.equal((await runCeption(["spawn", "--label", "impl", "first"], { env: sessionA.env, cwd: ctx.project })).code, 0);

  const stolen = await runCeption(["send", "impl", "mine now"], { env: ctx.env, cwd: ctx.project });
  assert.equal(stolen.code, 4, stolen.stdout);
  assert.match(stolen.stderr, /belongs to live session/);
});

test("dead session's label is adopted with thread and options intact", async (t) => {
  const ctx = await makeEnv("happy");
  const sessionA = await spawnDummySession(ctx);
  t.after(() => {
    sessionA.child.kill("SIGTERM");
    return cleanup(ctx, [sessionA.env]);
  });

  const spawned = await runCeption(
    ["spawn", "--label", "impl", "--model", "gpt-fixture", "--effort", "low", "first"],
    { env: sessionA.env, cwd: ctx.project }
  );
  assert.equal(spawned.code, 0, spawned.stderr);
  assert.equal((await runCeption(["kill", "--all"], { env: sessionA.env, cwd: ctx.project })).code, 0);
  sessionA.child.kill("SIGTERM");
  await waitFor(async () => !(await fs.access(`/proc/${sessionA.child.pid}/stat`).then(() => true, () => false)));

  const sent = await runCeption(["send", "impl", "follow up after death"], {
    env: ctx.env,
    cwd: ctx.project
  });
  assert.equal(sent.code, 0, sent.stderr);
  assert.match(sent.stdout, /Resumed the prior run/);

  const projectState = await readProjectState(ctx);
  assert.ok(projectState.sessions[ctx.sessionKey].labels.impl);
  assert.equal(projectState.sessions[sessionA.sessionKey]?.labels?.impl, undefined);

  const fakeState = await readJson(ctx.fakeState);
  assert.ok(fakeState.requests.some((request) => request.method === "thread/resume"));
  const last = fakeState.lastTurnStarts.at(-1);
  assert.equal(last.model, "gpt-fixture");
  assert.equal(last.effort, "low");
});

test("kill --all only touches the calling session's daemons", async (t) => {
  const ctx = await makeEnv("happy");
  const sessionA = await spawnDummySession(ctx);
  t.after(() => {
    sessionA.child.kill("SIGTERM");
    return cleanup(ctx, [sessionA.env]);
  });

  assert.equal((await runCeption(["spawn", "--label", "theirs", "first"], { env: sessionA.env, cwd: ctx.project })).code, 0);
  assert.equal((await runCeption(["spawn", "--label", "mine", "first"], { env: ctx.env, cwd: ctx.project })).code, 0);

  const killed = await runCeption(["kill", "--all"], { env: ctx.env, cwd: ctx.project });
  assert.match(killed.stdout, /killed 1 daemon/);

  const listed = await runCeption(["list", "--json"], { env: ctx.env, cwd: ctx.project });
  const rows = JSON.parse(listed.stdout);
  assert.equal(rows.find((row) => row.label === "theirs").status, "idle");
  assert.equal(rows.find((row) => row.label === "mine").status, "dead");
});

test("labels resolve to the project root, not the invocation subdirectory", async (t) => {
  const ctx = await makeEnv("happy");
  t.after(() => cleanup(ctx));
  await fs.mkdir(path.join(ctx.project, ".git"));
  const sub = path.join(ctx.project, "src", "deep");
  await fs.mkdir(sub, { recursive: true });

  assert.equal((await runCeption(["spawn", "--label", "rooted", "first"], { env: ctx.env, cwd: sub })).code, 0);
  const sent = await runCeption(["send", "rooted", "follow up"], { env: ctx.env, cwd: ctx.project });
  assert.equal(sent.code, 0, sent.stderr);

  const fakeState = await readJson(ctx.fakeState);
  assert.equal(fakeState.appServerStarts, 1);
  const threadStart = fakeState.requests.find((request) => request.method === "thread/start");
  assert.equal(threadStart.params.cwd, await fs.realpath(ctx.project));
  const stateDir = path.join(ctx.home, ".local", "state", "ception");
  const stateFiles = (await fs.readdir(stateDir)).filter((name) => name.endsWith(".json"));
  assert.equal(stateFiles.length, 1);
});

test("gc drops stale labels of dead sessions and their logs", async (t) => {
  const ctx = await makeEnv("happy");
  t.after(() => cleanup(ctx));

  assert.equal((await runCeption(["spawn", "--label", "keep", "first"], { env: ctx.env, cwd: ctx.project })).code, 0);

  const projectState = await readProjectState(ctx);
  const staleLog = path.join(ctx.root, "stale.log");
  await fs.writeFile(staleLog, "old\n");
  projectState.sessions["c999999-1"] = {
    labels: {
      stale: {
        threadId: "thr_gone",
        lastUsed: new Date(Date.now() - 30 * 86400_000).toISOString(),
        logPath: staleLog
      }
    }
  };
  const stateDir = path.join(ctx.home, ".local", "state", "ception");
  const stateName = (await fs.readdir(stateDir)).find((name) => name.endsWith(".json"));
  await fs.writeFile(path.join(stateDir, stateName), JSON.stringify(projectState));

  const listed = await runCeption(["list", "--json"], { env: ctx.env, cwd: ctx.project });
  const rows = JSON.parse(listed.stdout);
  assert.equal(rows.find((row) => row.label === "stale"), undefined);
  assert.ok(rows.find((row) => row.label === "keep"));
  assert.equal(await fs.access(staleLog).then(() => true, () => false), false);
});
