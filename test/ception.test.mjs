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

async function makeEnv(behavior = "happy") {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "ception-test-"));
  const home = path.join(root, "home");
  const runtime = path.join(root, "run");
  const project = path.join(root, "project");
  await fs.mkdir(home, { recursive: true });
  await fs.mkdir(runtime, { recursive: true });
  await fs.mkdir(project, { recursive: true });
  const fakeState = path.join(root, "fake-state.json");
  const env = {
    ...process.env,
    HOME: home,
    XDG_RUNTIME_DIR: runtime,
    CEPTION_CODEX_CMD: `${process.execPath} ${FAKE}`,
    CEPTION_FAKE_STATE: fakeState,
    CEPTION_FAKE_BEHAVIOR: behavior,
    CEPTION_IDLE_TIMEOUT_SECS: "30"
  };
  return { root, home, runtime, project, fakeState, env };
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

async function cleanup(ctx) {
  await runCeption(["kill", "--all"], { env: ctx.env, cwd: ctx.project, timeoutMs: 4000 }).catch(() => {});
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
  assert.match(result.stdout, /Handled the requested task/);
  assert.match(result.stdout, /threadId: thr_1/);

  const stateFiles = await fs.readdir(path.join(ctx.home, ".local", "state", "ception"));
  const projectStateName = stateFiles.find((name) => name.endsWith(".json"));
  const projectState = await readJson(path.join(ctx.home, ".local", "state", "ception", projectStateName));
  assert.equal(projectState.labels.alpha.threadId, "thr_1");

  const log = await fs.readFile(projectState.labels.alpha.logPath, "utf8");
  assert.match(log, /Thinking through fixture/);
});

test("send to idle live daemon starts a second turn on same app-server", async (t) => {
  const ctx = await makeEnv("happy");
  t.after(() => cleanup(ctx));

  assert.equal((await runCeption(["spawn", "--label", "idle", "first"], { env: ctx.env, cwd: ctx.project })).code, 0);
  const second = await runCeption(["send", "idle", "follow up"], { env: ctx.env, cwd: ctx.project });
  assert.equal(second.code, 0, second.stderr);
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

  const stateFiles = await fs.readdir(path.join(ctx.home, ".local", "state", "ception"));
  const projectStateName = stateFiles.find((name) => name.endsWith(".json"));
  const projectState = await readJson(path.join(ctx.home, ".local", "state", "ception", projectStateName));
  const log = await fs.readFile(projectState.labels.approval.logPath, "utf8");
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

async function procStarttime(pid) {
  const stat = await fs.readFile(`/proc/${pid}/stat`, "utf8");
  const end = stat.lastIndexOf(")");
  return stat.slice(end + 2).trim().split(/\s+/)[19];
}

test("Claude pid watch exits daemon after watched process dies", async (t) => {
  const ctx = await makeEnv("happy");
  const watched = spawn(process.execPath, ["-e", "setInterval(() => {}, 1000)"], {
    stdio: "ignore"
  });
  t.after(() => {
    watched.kill("SIGTERM");
    return cleanup(ctx);
  });
  const starttime = await procStarttime(watched.pid);
  const env = {
    ...ctx.env,
    CEPTION_WATCH_PID: String(watched.pid),
    CEPTION_WATCH_STARTTIME: starttime,
    CEPTION_CLAUDE_POLL_SECS: "0.1"
  };

  assert.equal((await runCeption(["spawn", "--label", "watch", "first"], { env, cwd: ctx.project })).code, 0);
  watched.kill("SIGTERM");

  const row = await waitFor(async () => {
    const listed = await runCeption(["list", "--json"], { env: ctx.env, cwd: ctx.project });
    const rows = JSON.parse(listed.stdout);
    const found = rows.find((candidate) => candidate.label === "watch");
    return found?.status === "dead" ? found : null;
  }, 5000);
  assert.equal(row.status, "dead");
});
