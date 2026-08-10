import fs from "node:fs/promises";
import { createWriteStream } from "node:fs";
import net from "node:net";
import process from "node:process";
import { parseArgs } from "node:util";

import { AppServerClient } from "./appserver.mjs";
import { isSameProcess } from "./claudepid.mjs";
import { formatGoal, TurnAccumulator } from "./render.mjs";
import { updateLabelState } from "./state.mjs";

function writeJson(socket, object) {
  if (!socket.destroyed) {
    socket.write(`${JSON.stringify(object)}\n`);
  }
}

function answer(socket, payload) {
  writeJson(socket, payload);
  socket.end();
}

function clientError(socket, message) {
  answer(socket, { type: "error", message });
}

function formatRateLimits(rateLimits) {
  const window = (entry) => {
    if (!entry || typeof entry.usedPercent !== "number") {
      return "n/a";
    }
    const mins = entry.windowDurationMins ?? 0;
    const span = mins >= 1440 ? `${Math.round(mins / 1440)}d` : `${Math.round(mins / 60)}h`;
    return `${entry.usedPercent}% of ${span}`;
  };
  return `primary ${window(rateLimits?.primary)}, secondary ${window(rateLimits?.secondary)}`;
}

function parseDaemonArgs(argv) {
  const { values } = parseArgs({
    args: argv,
    options: {
      label: { type: "string" },
      cwd: { type: "string" },
      hash: { type: "string" },
      session: { type: "string" },
      socket: { type: "string" },
      state: { type: "string" },
      log: { type: "string" },
      "resume-thread-id": { type: "string" },
      model: { type: "string" },
      effort: { type: "string" }
    }
  });
  for (const key of ["label", "cwd", "hash", "session", "socket", "state", "log"]) {
    if (!values[key]) {
      throw new Error(`daemon missing --${key}`);
    }
  }
  return {
    label: values.label,
    cwd: values.cwd,
    hash: values.hash,
    sessionKey: values.session,
    socketPath: values.socket,
    statePath: values.state,
    logPath: values.log,
    resumeThreadId: values["resume-thread-id"] || null,
    model: values.model || null,
    effort: values.effort || null
  };
}

class Daemon {
  constructor(options) {
    this.options = options;
    this.context = {
      cwd: options.cwd,
      label: options.label,
      hash: options.hash,
      sessionKey: options.sessionKey,
      statePath: options.statePath,
      logPath: options.logPath
    };
    this.threadId = options.resumeThreadId;
    this.activeTurn = null;
    this.pendingCompletion = null;
    // An active thread goal makes codex start follow-on turns on its own;
    // anything other than "active" means nothing more is coming.
    this.goal = null;
    // Event counters, so a request's reply can tell whether fresher state
    // (a goal update, a whole settled turn) landed while it was in flight.
    this.goalUpdates = 0;
    this.turnsSettled = 0;
    this.lastSettledTurn = null;
    // Clients waiting for the turn codex starts for an active goal.
    this.goalWaiters = [];
    this.connectedSockets = new Set();
    this.lastActivityAt = Date.now();
    this.shuttingDown = false;
    this.logStream = createWriteStream(options.logPath, { flags: "a" });
  }

  log(line) {
    this.logStream.write(`${String(line).replace(/\s+$/u, "")}\n`);
  }

  async start() {
    this.log(`[daemon] starting label=${this.options.label} cwd=${this.options.cwd} pid=${process.pid}`);
    this.app = new AppServerClient({
      cwd: this.options.cwd,
      log: (line) => this.log(line)
    });
    this.app.onNotification((message) => this.handleAppNotification(message));
    this.app.onServerRequest((message) => this.handleServerRequest(message));
    this.app.onExit = (error) => this.handleAppExit(error);
    await this.app.initialize();

    if (this.threadId) {
      await this.resumeThread(this.threadId);
    }

    await fs.rm(this.options.socketPath, { force: true });
    this.server = net.createServer((socket) => this.handleConnection(socket));
    await new Promise((resolve, reject) => {
      this.server.once("error", reject);
      this.server.listen(this.options.socketPath, () => {
        this.server.off("error", reject);
        resolve();
      });
    });
    await fs.chmod(this.options.socketPath, 0o600);

    this.startLifecycleTimers();
    this.log(`[daemon] listening ${this.options.socketPath}`);
  }

  startLifecycleTimers() {
    const idleSecs = Number(process.env.CEPTION_IDLE_TIMEOUT_SECS ?? 4 * 60 * 60);
    const idleIntervalMs = Math.max(1000, Math.min(30000, Math.floor((idleSecs * 1000) / 4) || 1000));
    this.idleTimer = setInterval(() => {
      if (this.shuttingDown || this.activeTurn || this.connectedSockets.size > 0) {
        return;
      }
      if (Date.now() - this.lastActivityAt >= idleSecs * 1000) {
        this.shutdown("idle timeout");
      }
    }, idleIntervalMs);
    this.idleTimer.unref();

    const watchedPid = Number(process.env.CEPTION_CLAUDE_PID || 0);
    const watchedStarttime = process.env.CEPTION_CLAUDE_STARTTIME;
    if (watchedPid && watchedStarttime) {
      const pollSecs = Number(process.env.CEPTION_CLAUDE_POLL_SECS ?? 30);
      this.claudeTimer = setInterval(async () => {
        if (!(await isSameProcess(watchedPid, watchedStarttime))) {
          this.log(`[daemon] watched Claude pid ${watchedPid} exited; shutting down`);
          await this.interruptActiveTurn();
          await this.shutdown("claude ancestor exited");
        }
      }, Math.max(250, pollSecs * 1000));
      this.claudeTimer.unref();
    }
  }

  async resumeThread(threadId) {
    const params = {
      threadId,
      cwd: this.options.cwd,
      approvalPolicy: "never",
      sandbox: "danger-full-access"
    };
    if (this.options.model) {
      params.model = this.options.model;
    }
    const response = await this.app.request("thread/resume", params);
    this.threadId = response.thread?.id ?? threadId;
    await this.persistLabel();
    this.log(`[thread] resumed ${this.threadId}`);
  }

  handleConnection(socket) {
    socket.setEncoding("utf8");
    this.connectedSockets.add(socket);
    let buffer = "";
    let handled = false;
    socket.on("data", (chunk) => {
      if (handled) {
        return;
      }
      buffer += chunk;
      const newline = buffer.indexOf("\n");
      if (newline === -1) {
        return;
      }
      handled = true;
      const line = buffer.slice(0, newline);
      let command;
      try {
        command = JSON.parse(line);
      } catch (error) {
        clientError(socket, `invalid command JSON: ${error.message}`);
        return;
      }
      this.handleCommand(socket, command).catch((error) => {
        clientError(socket, error?.message ?? String(error));
      });
    });
    socket.on("close", () => {
      this.connectedSockets.delete(socket);
      if (this.activeTurn) {
        this.activeTurn.clients = this.activeTurn.clients.filter((client) => client.socket !== socket);
      }
      this.goalWaiters = this.goalWaiters.filter((waiter) => {
        if (waiter.socket !== socket) {
          return true;
        }
        clearTimeout(waiter.timer);
        return false;
      });
    });
  }

  async handleCommand(socket, command) {
    this.lastActivityAt = Date.now();
    switch (command.cmd) {
      case "status":
        writeJson(socket, {
          type: "result",
          status: "ok",
          daemon: {
            pid: process.pid,
            label: this.options.label,
            cwd: this.options.cwd,
            threadId: this.threadId,
            turnId: this.activeTurn?.turnId ?? null,
            state: this.activeTurn ? "active" : "idle",
            goal: this.goal,
            logPath: this.options.logPath
          }
        });
        socket.end();
        return;

      case "turn":
        await this.startTurn(socket, command);
        return;

      case "send":
        await this.sendTurn(socket, command);
        return;

      case "goal":
        await this.commandGoal(socket, command);
        return;

      case "interrupt":
        await this.commandInterrupt(socket);
        return;

      case "watch":
        this.commandWatch(socket, command);
        return;

      case "shutdown":
        writeJson(socket, { type: "result", status: "ok", report: "shutdown accepted" });
        socket.end();
        await this.interruptActiveTurn();
        await this.shutdown("client shutdown");
        return;

      default:
        clientError(socket, `unknown daemon command ${command.cmd}`);
    }
  }

  async ensureThread() {
    if (this.threadId) {
      return this.threadId;
    }
    const params = {
      cwd: this.options.cwd,
      approvalPolicy: "never",
      sandbox: "danger-full-access"
    };
    if (this.options.model) {
      params.model = this.options.model;
    }
    const response = await this.app.request("thread/start", params);
    this.threadId = response.thread?.id;
    if (!this.threadId) {
      throw new Error("thread/start returned no thread id");
    }
    await this.persistLabel();
    this.log(`[thread] started ${this.threadId}`);
    return this.threadId;
  }

  async startTurn(socket, command) {
    if (this.activeTurn) {
      clientError(socket, "turn already in flight; use send or wait for completion");
      return;
    }
    const prompt = command.prompt ?? "";
    const report = command.report ?? "brief";
    // Claim the slot before any await so concurrent commands can't start a
    // second turn on the same thread.
    const active = {
      turnId: null,
      prompt,
      report,
      accumulator: null,
      clients: [{ socket, report }],
      headerLogged: false
    };
    this.activeTurn = active;

    try {
      const threadId = await this.ensureThread();
      active.accumulator = new TurnAccumulator({
        label: this.options.label,
        cwd: this.options.cwd,
        threadId,
        turnId: "starting",
        prompt,
        logPath: this.options.logPath
      });

      const params = {
        threadId,
        input: [{ type: "text", text: prompt, text_elements: [] }],
        cwd: this.options.cwd,
        approvalPolicy: "never",
        sandboxPolicy: { type: "dangerFullAccess" }
      };
      if (this.options.model) {
        params.model = this.options.model;
      }
      if (this.options.effort) {
        params.effort = this.options.effort;
      }

      const response = await this.app.request("turn/start", params);
      // The response id is authoritative. A goal continuation starting while we
      // awaited it can have claimed the slot via turn/started; keeping that id
      // would filter out every event of the turn we actually asked for.
      const startedId = response.turn?.id;
      if (startedId && active.turnId && active.turnId !== startedId) {
        this.log(`[turn] unsolicited turn ${active.turnId} raced our start; re-targeting to ${startedId}`);
      }
      active.turnId = startedId ?? active.turnId;
      active.accumulator.turnId = active.turnId ?? "unknown";
      this.logTurnHeader();
    } catch (error) {
      // Watchers may have attached to this turn already; fail them all.
      for (const client of active.clients) {
        clientError(client.socket, error?.message ?? String(error));
      }
      if (this.activeTurn !== active) {
        return;
      }
      // A turnId here means a turn/started was consumed while we awaited
      // turn/start: an unsolicited continuation won the thread and our request
      // lost. Keep tracking it ownerless instead of letting it run invisible.
      if (active.turnId && active.accumulator) {
        active.clients = [];
        this.log(`[turn] turn/start failed but turn ${active.turnId} is running; keeping it tracked`);
        return;
      }
      this.activeTurn = null;
    }
  }

  // During the between-turns hold the turn is neither steerable nor finished;
  // wait for the hold to resolve either way. The grace timer bounds this, so
  // no separate deadline — a shorter one would let send/interrupt act on the
  // already-completed turn id.
  async settlePendingCompletion() {
    while (this.pendingCompletion) {
      await new Promise((resolve) => setTimeout(resolve, 25));
    }
  }

  // Atomic send: steer the active turn if there is one, else start a turn.
  // Doing the decision daemon-side closes the client's status/steer race.
  async sendTurn(socket, command) {
    await this.settlePendingCompletion();
    if (this.activeTurn) {
      // The turn may still be waiting on turn/start; give it a moment to get
      // a turnId before steering.
      const deadline = Date.now() + 3000;
      while (this.activeTurn && !this.activeTurn.turnId && Date.now() < deadline) {
        await new Promise((resolve) => setTimeout(resolve, 25));
      }
      if (this.activeTurn?.turnId) {
        await this.steerTurn(socket, command);
        return;
      }
    }
    await this.startTurn(socket, command);
  }

  async steerTurn(socket, command) {
    if (!this.activeTurn?.turnId) {
      clientError(socket, "no active steerable turn");
      return;
    }
    try {
      const response = await this.app.request("turn/steer", {
        threadId: this.threadId,
        expectedTurnId: this.activeTurn.turnId,
        input: [{ type: "text", text: command.prompt ?? "", text_elements: [] }]
      });
      const turnId = response.turnId ?? this.activeTurn.turnId;
      writeJson(socket, {
        type: "result",
        status: "steered",
        report: `steered active turn ${turnId}`
      });
      socket.end();
    } catch (error) {
      clientError(socket, error?.message ?? String(error));
    }
  }

  // Attach to the active turn as one more result recipient; the reply comes
  // from completeActiveTurn alongside the client that started the turn.
  commandWatch(socket, command) {
    if (!this.activeTurn) {
      writeJson(socket, { type: "result", status: "idle", report: "no active turn" });
      socket.end();
      return;
    }
    this.activeTurn.clients.push({ socket, report: command.report ?? "brief" });
  }

  // A thread goal is codex's own long-run driver. Setting or resuming one
  // behaves like starting a turn — the client blocks on the work codex kicks
  // off — while pause, clear and show return at once.
  async commandGoal(socket, command) {
    const action = command.action ?? "show";

    if (action === "show") {
      const goal = this.threadId ? await this.fetchGoal() : null;
      writeJson(socket, { type: "result", status: "ok", goal, report: formatGoal(goal, this.options.label) });
      socket.end();
      return;
    }

    const threadId = await this.ensureThread();

    if (action === "clear") {
      await this.goalRequest("thread/goal/clear", { threadId });
      this.log("[goal] cleared");
      // A goal set concurrently can survive the clear; report what is there.
      const report = this.goal
        ? `goal cleared, but a newer one is already set\n${formatGoal(this.goal, this.options.label)}`
        : "goal cleared";
      writeJson(socket, { type: "result", status: "ok", goal: this.goal, report });
      socket.end();
      return;
    }

    const params = { threadId, status: action === "pause" ? "paused" : "active" };
    if (command.objective) {
      params.objective = command.objective;
    }

    if (action === "pause") {
      await this.setGoal(params, action, command.objective);
      writeJson(socket, {
        type: "result",
        status: "ok",
        goal: this.goal,
        report: formatGoal(this.goal, this.options.label)
      });
      socket.end();
      return;
    }

    await this.settlePendingCompletion();
    // Codex can start *and finish* the goal's turn inside this round trip
    // (the shape of a turn that fails as it starts), so count what settles
    // and decide where to attach afterwards.
    const settledBefore = this.turnsSettled;
    try {
      await this.setGoal(params, action, command.objective);
    } catch (error) {
      clientError(socket, error?.message ?? String(error));
      return;
    }
    this.attachGoalClient(socket, command.report ?? "brief", settledBefore);
  }

  // Where the client waits for the goal's work: a turn that already ran, the
  // turn running now, or the one codex is about to start.
  attachGoalClient(socket, report, settledBefore) {
    const goalActive = this.goal?.status === "active";
    if (this.turnsSettled > settledBefore && !goalActive && this.lastSettledTurn) {
      this.log("[goal] the goal's turn ran and stopped while it was being set");
      answer(socket, this.turnResult(this.lastSettledTurn, report));
      return;
    }
    if (this.activeTurn) {
      // Codex folds a goal set mid-turn into the running turn.
      this.activeTurn.clients.push({ socket, report });
      return;
    }
    if (!goalActive) {
      answer(socket, {
        type: "result",
        status: "ok",
        goal: this.goal,
        report: `${formatGoal(this.goal, this.options.label)}\n\n(codex started no turn)`
      });
      return;
    }
    this.waitForGoalTurn(socket, report);
  }

  async setGoal(params, action, objective) {
    await this.goalRequest("thread/goal/set", params);
    this.log(`[goal] ${action} -> ${this.goal?.status ?? "unknown"}`);
    if (objective) {
      this.log(`[goal] objective: ${objective}`);
    }
  }

  // A goal/updated notification can be dispatched while the reply is in
  // flight (codex may already have run the goal's turn); notifications are
  // what turn settlement keys on, so they win over the reply's snapshot.
  async goalRequest(method, params) {
    const seen = this.goalUpdates;
    const response = await this.app.request(method, params);
    if (this.goalUpdates === seen) {
      this.goal = response?.goal ?? null;
    }
    return this.goal;
  }

  // Park the client until codex starts the goal's turn; every path that
  // starts or adopts a turn takes these waiters as its clients.
  waitForGoalTurn(socket, report) {
    const waitMs = Number(process.env.CEPTION_GOAL_START_MS ?? 30000);
    const waiter = { socket, report };
    waiter.timer = setTimeout(() => {
      this.goalWaiters = this.goalWaiters.filter((other) => other !== waiter);
      this.log(`[goal] no turn started within ${waitMs}ms`);
      answer(socket, {
        type: "result",
        status: "ok",
        goal: this.goal,
        report: `${formatGoal(this.goal, this.options.label)}\n\n(codex started no turn within ${waitMs}ms)`
      });
    }, waitMs);
    waiter.timer.unref?.();
    this.goalWaiters.push(waiter);
  }

  async fetchGoal() {
    return this.goalRequest("thread/goal/get", { threadId: this.threadId });
  }

  // Answer clients that were waiting for a goal turn that will never arrive.
  releaseGoalWaiters(report) {
    const waiters = this.goalWaiters;
    this.goalWaiters = [];
    for (const waiter of waiters) {
      clearTimeout(waiter.timer);
      answer(waiter.socket, { type: "result", status: "ok", goal: this.goal, report });
    }
  }

  // Hand anyone waiting for the goal's turn to the turn now running.
  absorbGoalWaiters() {
    if (this.goalWaiters.length === 0 || !this.activeTurn) {
      return;
    }
    const waiting = this.takeGoalWaiters();
    this.activeTurn.clients.push(...waiting);
    this.log(`[goal] ${waiting.length} client(s) attached to the goal's turn`);
  }

  takeGoalWaiters() {
    const waiters = this.goalWaiters;
    this.goalWaiters = [];
    for (const waiter of waiters) {
      clearTimeout(waiter.timer);
    }
    return waiters;
  }

  async commandInterrupt(socket) {
    await this.settlePendingCompletion();
    const turnId = this.activeTurn?.turnId;
    if (!turnId || !this.threadId) {
      writeJson(socket, { type: "result", status: "idle", report: "no active turn" });
      socket.end();
      return;
    }
    try {
      // Pause first: freeing the thread would just start the goal's next turn.
      const pausedGoal = this.goal?.status === "active";
      if (pausedGoal) {
        await this.goalRequest("thread/goal/set", { threadId: this.threadId, status: "paused" });
        this.log("[goal] paused so the interrupt ends the run");
      }
      await this.app.request("turn/interrupt", { threadId: this.threadId, turnId });
      writeJson(socket, {
        type: "result",
        status: "ok",
        goal: this.goal,
        report: pausedGoal
          ? `interrupt requested for ${turnId}\n${formatGoal(this.goal, this.options.label)}`
          : `interrupt requested for ${turnId}`
      });
      socket.end();
    } catch (error) {
      clientError(socket, error?.message ?? String(error));
    }
  }

  // Best-effort and capped: only shutdown paths call this, and a rejected or
  // wedged interrupt must not keep the daemon (and attached clients) alive.
  async interruptActiveTurn() {
    if (!this.activeTurn?.turnId || !this.threadId) {
      return;
    }
    const request = this.app
      .request("turn/interrupt", {
        threadId: this.threadId,
        turnId: this.activeTurn.turnId
      })
      .catch((error) => this.log(`[interrupt] ${error.message}`));
    const cap = new Promise((resolve) => setTimeout(resolve, 3000).unref());
    await Promise.race([request, cap]);
  }

  handleAppNotification(message) {
    const { method, params = {} } = message;
    if (method === "thread/tokenUsage/updated") {
      if (this.activeTurn?.accumulator && params.threadId === this.threadId && params.turnId === this.activeTurn.turnId) {
        this.activeTurn.accumulator.handleNotification(method, params);
      }
      return;
    }

    if (method === "account/rateLimits/updated") {
      const line = `[rate] ${formatRateLimits(params.rateLimits)}`;
      if (line !== this.lastRateLine) {
        this.lastRateLine = line;
        this.log(line);
      }
      return;
    }

    if (method === "turn/started" && params.threadId === this.threadId) {
      if (this.activeTurn) {
        if (this.pendingCompletion) {
          // The turn we were about to report is continuing in a new turn after
          // a compaction. Keep the clients and the report; just re-target.
          this.resumeContinuation(params.turn?.id);
          return;
        }
        this.activeTurn.turnId ??= params.turn?.id;
        if (this.activeTurn.accumulator) {
          this.activeTurn.accumulator.turnId = this.activeTurn.turnId ?? "unknown";
        }
        // One thread runs one turn: codex folds the goal into this one.
        this.absorbGoalWaiters();
        this.logTurnHeader();
        return;
      }
      // No turn in flight, yet codex started one on our thread: a continuation
      // we already reported as finished. Adopt it so list/watch/send see the
      // truth and the work stays on the log instead of running unobserved.
      this.adoptOrphanTurn(params.turn?.id);
      return;
    }

    if (method === "thread/goal/updated" || method === "thread/goal/cleared") {
      // Codex's own subagents get threads and goals on this app-server;
      // adopting a child's status would settle our report on it.
      if (params.threadId !== this.threadId) {
        return;
      }
      const goal = method === "thread/goal/cleared" ? null : params.goal ?? null;
      const status = goal?.status ?? null;
      const changed = status !== (this.goal?.status ?? null);
      this.goal = goal;
      this.goalUpdates += 1;
      if (changed) {
        this.log(`[goal] ${status ?? "cleared"}`);
      }
      if (status !== "active") {
        if (this.pendingCompletion) {
          this.log("[goal] no longer active; settling the turn");
          clearTimeout(this.pendingCompletion);
          this.pendingCompletion = null;
          this.finishActiveTurn();
        }
        // The goal stopped instead of starting the awaited turn; say so
        // rather than time out.
        if (this.goalWaiters.length > 0 && !this.activeTurn) {
          this.releaseGoalWaiters(`${formatGoal(this.goal, this.options.label)}\n\n(codex started no turn)`);
        }
      }
      return;
    }

    // Failed MCP servers explain later tool gaps, so they stay; the rest of
    // this family is high-frequency restatement that buried the real log.
    if (method === "mcpServer/startupStatus/updated") {
      if (params.status === "failed") {
        this.log(`[mcp] ${params.name} failed to start: ${params.error ?? "unknown error"}`);
      }
      return;
    }

    if (!method.startsWith("item/") && method !== "turn/completed") {
      const ignored = [
        "thread/started",
        "thread/status/changed",
        "thread/goal/cleared",
        "thread/goal/updated",
        "remoteControl/status/changed",
        "turn/diff/updated",
        "turn/plan/updated",
        "model/safetyBuffering/updated"
      ];
      if (!ignored.includes(method)) {
        this.log(`[debug] unknown notification ${JSON.stringify(message)}`);
      }
      return;
    }

    if (!this.activeTurn?.accumulator || params.threadId !== this.threadId) {
      return;
    }
    if (params.turnId && this.activeTurn.turnId && params.turnId !== this.activeTurn.turnId) {
      return;
    }
    if (params.turnId && !this.activeTurn.turnId) {
      this.activeTurn.turnId = params.turnId;
      this.activeTurn.accumulator.turnId = params.turnId;
      this.logTurnHeader();
    }

    const lines = this.activeTurn.accumulator.handleNotification(method, params);
    for (const line of lines) {
      this.log(line);
    }

    if (method === "turn/completed") {
      this.completeActiveTurn();
    }
  }

  logTurnHeader() {
    if (!this.activeTurn?.accumulator || this.activeTurn.headerLogged || !this.activeTurn.turnId) {
      return;
    }
    this.activeTurn.accumulator.turnId = this.activeTurn.turnId;
    this.log(this.activeTurn.accumulator.headerLine());
    this.activeTurn.headerLogged = true;
  }

  // A physical turn ending is not the logical run ending: while a goal is
  // active codex will start another turn by itself, so hold the clients and
  // the report until the goal settles. The timeout is a stall safety net.
  completeActiveTurn() {
    const active = this.activeTurn;
    if (!active) {
      return;
    }
    if (this.pendingCompletion) {
      return;
    }
    const goalActive = this.goal?.status === "active";
    // Only compactions since the last hold count, or every continuation would
    // pay a second grace window after finishing clean.
    const compacted = active.accumulator.compactions > (active.compactionsHeld ?? 0);
    if (goalActive || compacted) {
      const graceMs = goalActive
        ? Number(process.env.CEPTION_GOAL_GRACE_MS ?? 30000)
        : Number(process.env.CEPTION_CONTINUATION_GRACE_MS ?? 2000);
      const reason = goalActive ? "goal still active" : "turn compacted";
      active.compactionsHeld = active.accumulator.compactions;
      this.log(`[turn] ${reason}; holding the report up to ${graceMs}ms for a continuation turn`);
      this.pendingCompletion = setTimeout(() => {
        this.pendingCompletion = null;
        this.log("[turn] no continuation arrived; settling");
        this.finishActiveTurn();
      }, graceMs);
      this.pendingCompletion.unref?.();
      return;
    }
    this.finishActiveTurn();
  }

  resumeContinuation(turnId) {
    clearTimeout(this.pendingCompletion);
    this.pendingCompletion = null;
    const active = this.activeTurn;
    if (!active) {
      return;
    }
    active.turnId = turnId ?? active.turnId;
    active.accumulator.adoptContinuation(active.turnId);
    this.absorbGoalWaiters();
    this.log(`[turn] continuing in turn ${active.turnId}`);
  }

  // A turn codex started on its own: the goal's turn someone is waiting for,
  // or an unattended continuation — track it either way, since an unobserved
  // turn still edits the repo.
  adoptOrphanTurn(turnId) {
    if (!turnId || !this.threadId || this.shuttingDown) {
      return;
    }
    const clients = this.takeGoalWaiters();
    const accumulator = new TurnAccumulator({
      label: this.options.label,
      cwd: this.options.cwd,
      threadId: this.threadId,
      turnId,
      prompt: clients.length > 0 ? "(goal turn)" : "(unattended continuation)",
      logPath: this.options.logPath
    });
    this.activeTurn = { turnId, prompt: "", report: "brief", accumulator, clients, headerLogged: false };
    this.logTurnHeader();
    this.log(
      clients.length > 0
        ? "[turn] codex started the goal's turn"
        : "[turn] adopted an unattended continuation turn; attach with `ception watch`"
    );
  }

  turnResult(turn, report) {
    return {
      type: "result",
      status: turn.accumulator.status,
      report: turn.accumulator.buildReport(report),
      goal: this.goal,
      threadId: this.threadId,
      turnId: turn.turnId,
      logPath: this.options.logPath
    };
  }

  finishActiveTurn() {
    const active = this.activeTurn;
    if (!active) {
      return;
    }
    this.log(active.accumulator.footerLine());
    for (const client of active.clients) {
      answer(client.socket, this.turnResult(active, client.report));
    }
    this.activeTurn = null;
    // Kept for a `goal` whose request outlived the turn; see attachGoalClient.
    this.lastSettledTurn = active;
    this.turnsSettled += 1;
    // With the goal stopped no further turn is coming; answer waiters now.
    if (this.goalWaiters.length > 0 && this.goal?.status !== "active") {
      this.releaseGoalWaiters(`${formatGoal(this.goal, this.options.label)}\n\n(codex started no turn)`);
    }
    this.lastActivityAt = Date.now();
    this.persistLabel().catch((error) => this.log(`[state] ${error.message}`));
  }

  handleServerRequest(message) {
    const description = `codex sent ${message.method} despite approvalPolicy never/danger-full-access; failing turn`;
    this.log(`[error] ${description}: ${JSON.stringify(message)}`);
    clearTimeout(this.pendingCompletion);
    this.pendingCompletion = null;
    const active = this.activeTurn;
    this.activeTurn = null;
    if (active?.turnId && this.threadId) {
      this.app
        .request("turn/interrupt", { threadId: this.threadId, turnId: active.turnId })
        .catch((error) => this.log(`[error] interrupt after server request: ${error.message}`));
    }
    for (const client of [...(active?.clients ?? []), ...this.takeGoalWaiters()]) {
      answer(client.socket, { type: "error", message: description });
    }
  }

  handleAppExit(error) {
    if (this.shuttingDown) {
      return;
    }
    const message = error?.message ?? "codex app-server exited";
    this.log(`[app-server] ${message}`);
    clearTimeout(this.pendingCompletion);
    this.pendingCompletion = null;
    for (const client of [...this.takeGoalWaiters(), ...(this.activeTurn?.clients ?? [])]) {
      answer(client.socket, { type: "error", message });
    }
    this.activeTurn = null;
    this.shutdown("app-server exit").catch(() => {});
  }

  async persistLabel() {
    if (!this.threadId) {
      return;
    }
    await updateLabelState(this.context, {
      threadId: this.threadId,
      lastUsed: new Date().toISOString(),
      logPath: this.options.logPath,
      model: this.options.model,
      effort: this.options.effort
    });
  }

  async shutdown(reason) {
    if (this.shuttingDown) {
      return;
    }
    this.shuttingDown = true;
    this.log(`[daemon] shutting down: ${reason}`);
    clearInterval(this.idleTimer);
    clearInterval(this.claudeTimer);
    clearTimeout(this.pendingCompletion);
    this.pendingCompletion = null;
    // Settle anyone still waiting on a turn result; server.close() below
    // waits for open connections, so leaving them attached would deadlock
    // the shutdown against the turn it is abandoning.
    for (const waiter of this.takeGoalWaiters()) {
      clientError(waiter.socket, `daemon shutting down: ${reason}`);
    }
    if (this.activeTurn) {
      for (const client of this.activeTurn.clients) {
        clientError(client.socket, `daemon shutting down: ${reason}`);
      }
      this.activeTurn = null;
    }
    await this.persistLabel().catch((error) => this.log(`[state] ${error.message}`));
    if (this.server) {
      await new Promise((resolve) => this.server.close(() => resolve()));
    }
    await fs.rm(this.options.socketPath, { force: true }).catch(() => {});
    await this.app?.close().catch((error) => this.log(`[app-server close] ${error.message}`));
    this.logStream.end();
    setTimeout(() => process.exit(0), 10).unref();
  }
}

export async function runDaemon(argv) {
  const options = parseDaemonArgs(argv);
  const daemon = new Daemon(options);
  const signalShutdown = (signal) => {
    daemon
      .interruptActiveTurn()
      .then(() => daemon.shutdown(signal))
      .catch(() => process.exit(1));
  };
  process.on("SIGTERM", () => signalShutdown("SIGTERM"));
  process.on("SIGINT", () => signalShutdown("SIGINT"));
  try {
    await daemon.start();
  } catch (error) {
    daemon.log(`[daemon] startup failed: ${error?.message ?? String(error)}`);
    await fs.rm(options.socketPath, { force: true }).catch(() => {});
    process.exitCode = 1;
    setTimeout(() => process.exit(1), 10).unref();
  }
}
