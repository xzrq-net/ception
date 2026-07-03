import { spawn } from "node:child_process";
import fs from "node:fs";
import process from "node:process";
import readline from "node:readline";

import packageJson from "../package.json" with { type: "json" };

function jsonRpcError(code, message, data) {
  return data === undefined ? { code, message } : { code, message, data };
}

function protocolError(message, data) {
  const error = new Error(message);
  error.data = data;
  if (data?.code !== undefined) {
    error.rpcCode = data.code;
  }
  return error;
}

function splitCommandLine(commandLine) {
  const parts = [];
  let current = "";
  let quote = null;
  let escaped = false;
  for (const char of commandLine) {
    if (escaped) {
      current += char;
      escaped = false;
      continue;
    }
    if (char === "\\") {
      escaped = true;
      continue;
    }
    if (quote) {
      if (char === quote) {
        quote = null;
      } else {
        current += char;
      }
      continue;
    }
    if (char === "'" || char === '"') {
      quote = char;
      continue;
    }
    if (/\s/.test(char)) {
      if (current) {
        parts.push(current);
        current = "";
      }
      continue;
    }
    current += char;
  }
  if (current) {
    parts.push(current);
  }
  return parts;
}

function codexCommand() {
  const override = process.env.CEPTION_CODEX_CMD;
  if (override) {
    const [command, ...args] = splitCommandLine(override);
    if (!command) {
      throw new Error("CEPTION_CODEX_CMD is empty");
    }
    return { command, args };
  }
  return { command: "npx", args: ["-y", "@openai/codex", "app-server"] };
}

export class AppServerClient {
  constructor({ cwd, log }) {
    this.cwd = cwd;
    this.log = log ?? (() => {});
    this.pending = new Map();
    this.nextId = 1;
    this.closed = false;
    this.exitResolved = false;
    this.stderr = "";
    this.notificationHandler = null;
    this.serverRequestHandler = null;
    this.exitPromise = new Promise((resolve) => {
      this.resolveExit = resolve;
    });
  }

  onNotification(handler) {
    this.notificationHandler = handler;
  }

  onServerRequest(handler) {
    this.serverRequestHandler = handler;
  }

  async initialize() {
    const { command, args } = codexCommand();
    this.proc = spawn(command, args, {
      cwd: this.cwd,
      env: process.env,
      stdio: ["pipe", "pipe", "pipe"],
      windowsHide: true
    });

    this.proc.stdout.setEncoding("utf8");
    this.proc.stderr.setEncoding("utf8");

    this.proc.stderr.on("data", (chunk) => {
      this.stderr += chunk;
      this.log(`[app-server stderr] ${chunk}`);
    });
    this.proc.on("error", (error) => this.handleExit(error));
    this.proc.on("exit", (code, signal) => {
      if (this.closed && (code === 0 || signal === "SIGTERM")) {
        this.handleExit(null);
        return;
      }
      const detail = this.stderr.trim();
      this.handleExit(
        protocolError(
          `codex app-server exited unexpectedly (${signal ? `signal ${signal}` : `exit ${code}`})${detail ? `\n${detail}` : ""}`
        )
      );
    });

    this.readline = readline.createInterface({ input: this.proc.stdout });
    this.readline.on("line", (line) => this.handleLine(line));

    await this.request("initialize", {
      clientInfo: {
        name: "ception",
        title: "ception",
        version: packageJson.version ?? "0.0.0"
      },
      capabilities: {
        experimentalApi: false
      }
    });
    this.notify("initialized", {});
  }

  request(method, params = {}) {
    if (this.closed) {
      throw new Error("codex app-server client is closed");
    }
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { method, resolve, reject });
      this.send({ id, method, params });
    });
  }

  notify(method, params = {}) {
    if (!this.closed) {
      this.send({ method, params });
    }
  }

  send(message) {
    if (!this.proc?.stdin?.writable) {
      throw new Error("codex app-server stdin is not writable");
    }
    this.proc.stdin.write(`${JSON.stringify(message)}\n`);
  }

  handleLine(line) {
    if (!line.trim()) {
      return;
    }
    let message;
    try {
      message = JSON.parse(line);
    } catch (error) {
      this.handleExit(protocolError(`app-server emitted invalid JSON: ${error.message}`, { line }));
      return;
    }

    if (message.id !== undefined && message.method) {
      this.handleServerRequest(message);
      return;
    }

    if (message.id !== undefined) {
      const pending = this.pending.get(message.id);
      if (!pending) {
        return;
      }
      this.pending.delete(message.id);
      if (message.error) {
        pending.reject(protocolError(message.error.message ?? `${pending.method} failed`, message.error));
      } else {
        pending.resolve(message.result ?? {});
      }
      return;
    }

    if (message.method) {
      this.notificationHandler?.(message);
    }
  }

  handleServerRequest(message) {
    const warning = `unsupported app-server request ${message.method}; rejecting with -32601`;
    this.serverRequestHandler?.(warning, message);
    this.send({
      id: message.id,
      error: jsonRpcError(-32601, `Unsupported server request: ${message.method}`)
    });
  }

  handleExit(error) {
    if (this.exitResolved) {
      return;
    }
    this.exitResolved = true;
    for (const pending of this.pending.values()) {
      pending.reject(error ?? new Error("codex app-server connection closed"));
    }
    this.pending.clear();
    this.resolveExit(error ?? null);
    this.onExit?.(error ?? null);
  }

  async close() {
    if (this.closed) {
      await this.exitPromise;
      return;
    }
    this.closed = true;
    this.readline?.close();
    if (this.proc?.stdin?.writable) {
      this.proc.stdin.end();
    }
    setTimeout(() => {
      if (this.proc && this.proc.exitCode === null && !this.proc.killed) {
        this.proc.kill("SIGTERM");
      }
    }, 100).unref();
    await this.exitPromise;
  }
}
