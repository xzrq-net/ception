import fs from "node:fs/promises";
import { createWriteStream } from "node:fs";
import net from "node:net";
import process from "node:process";
import { parseArgs } from "node:util";

import { AppServerClient } from "./appserver.mjs";
import { isSameProcess } from "./claudepid.mjs";
import { TurnAccumulator } from "./render.mjs";
import { updateLabelState } from "./state.mjs";

function writeJson(socket, object) {
  if (!socket.destroyed) {
    socket.write(`${JSON.stringify(object)}\n`);
  }
}

function clientError(socket, message) {
  writeJson(socket, { type: "error", message });
  socket.end();
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
      active.turnId ??= response.turn?.id;
      active.accumulator.turnId = active.turnId ?? "unknown";
      this.logTurnHeader();
    } catch (error) {
      // Watchers may have attached to this turn already; fail them all.
      for (const client of active.clients) {
        clientError(client.socket, error?.message ?? String(error));
      }
      if (this.activeTurn === active) {
        this.activeTurn = null;
      }
    }
  }

  // Atomic send: steer the active turn if there is one, else start a turn.
  // Doing the decision daemon-side closes the client's status/steer race.
  async sendTurn(socket, command) {
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

  async commandInterrupt(socket) {
    const turnId = this.activeTurn?.turnId;
    if (!turnId || !this.threadId) {
      writeJson(socket, { type: "result", status: "idle", report: "no active turn" });
      socket.end();
      return;
    }
    try {
      await this.app.request("turn/interrupt", { threadId: this.threadId, turnId });
      writeJson(socket, {
        type: "result",
        status: "ok",
        report: `interrupt requested for ${turnId}`
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

    if (method === "turn/started" && this.activeTurn && params.threadId === this.threadId) {
      this.activeTurn.turnId ??= params.turn?.id;
      if (this.activeTurn.accumulator) {
        this.activeTurn.accumulator.turnId = this.activeTurn.turnId ?? "unknown";
      }
      this.logTurnHeader();
      return;
    }

    if (!method.startsWith("item/") && method !== "turn/completed") {
      const ignored = [
        "thread/started",
        "thread/status/changed",
        "thread/goal/cleared",
        "remoteControl/status/changed",
        "turn/diff/updated"
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

  completeActiveTurn() {
    const active = this.activeTurn;
    if (!active) {
      return;
    }
    this.log(active.accumulator.footerLine());
    for (const client of active.clients) {
      writeJson(client.socket, {
        type: "result",
        status: active.accumulator.status,
        report: active.accumulator.buildReport(client.report),
        threadId: this.threadId,
        turnId: active.turnId,
        logPath: this.options.logPath
      });
      client.socket.end();
    }
    this.activeTurn = null;
    this.lastActivityAt = Date.now();
    this.persistLabel().catch((error) => this.log(`[state] ${error.message}`));
  }

  handleServerRequest(message) {
    const description = `codex sent ${message.method} despite approvalPolicy never/danger-full-access; failing turn`;
    this.log(`[error] ${description}: ${JSON.stringify(message)}`);
    const active = this.activeTurn;
    this.activeTurn = null;
    if (active?.turnId && this.threadId) {
      this.app
        .request("turn/interrupt", { threadId: this.threadId, turnId: active.turnId })
        .catch((error) => this.log(`[error] interrupt after server request: ${error.message}`));
    }
    for (const client of active?.clients ?? []) {
      writeJson(client.socket, { type: "error", message: description });
      client.socket.end();
    }
  }

  handleAppExit(error) {
    if (this.shuttingDown) {
      return;
    }
    const message = error?.message ?? "codex app-server exited";
    this.log(`[app-server] ${message}`);
    if (this.activeTurn) {
      for (const client of this.activeTurn.clients) {
        writeJson(client.socket, { type: "error", message });
        client.socket.end();
      }
      this.activeTurn = null;
    }
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
    // Settle anyone still waiting on a turn result; server.close() below
    // waits for open connections, so leaving them attached would deadlock
    // the shutdown against the turn it is abandoning.
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
