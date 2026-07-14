function asTextList(value) {
  if (!Array.isArray(value)) {
    return [];
  }
  return value.map((entry) => (typeof entry === "string" ? entry : entry?.text ?? "")).filter(Boolean);
}

function truncate(value, max = 1200) {
  const text = String(value ?? "");
  return text.length <= max ? text : `${text.slice(0, max)}…`;
}

function oneLine(value, max = 200) {
  return truncate(String(value ?? "").replace(/\s+/g, " ").trim(), max);
}

function isCompactionAmnesiaMessage(value) {
  return /^Instructions loaded(?: for [^\r\n]+)?\.$/u.test(String(value ?? "").trim());
}

export function formatTokenUsage(tokenUsage) {
  const usage = tokenUsage?.last ?? tokenUsage?.total ?? tokenUsage;
  if (!usage || typeof usage.totalTokens !== "number") {
    return "n/a";
  }
  return `${usage.totalTokens} total (${usage.inputTokens ?? 0} in, ${usage.outputTokens ?? 0} out, ${usage.reasoningOutputTokens ?? 0} reasoning)`;
}

export function formatDuration(ms) {
  if (!Number.isFinite(ms) || ms < 0) {
    return "n/a";
  }
  const totalSeconds = Math.round(ms / 1000);
  if (totalSeconds < 60) {
    return `${(ms / 1000).toFixed(1)}s`;
  }
  return `${Math.floor(totalSeconds / 60)}m ${totalSeconds % 60}s`;
}

export function statusExitCode(status) {
  if (status === "completed" || status === "ok" || status === "steered" || status === "idle") {
    return 0;
  }
  if (status === "failed") {
    return 2;
  }
  if (status === "interrupted") {
    return 3;
  }
  return 4;
}

export class TurnAccumulator {
  constructor({ label, cwd, threadId, turnId, prompt, logPath }) {
    this.label = label;
    this.cwd = cwd;
    this.threadId = threadId;
    this.turnId = turnId;
    this.prompt = prompt;
    this.logPath = logPath;
    this.startedAt = Date.now();
    this.items = new Map();
    this.itemLines = [];
    this.fullLines = [];
    this.reasoningSummaryDeltas = new Map();
    this.reasoningTextDeltas = new Map();
    this.agentMessageDeltas = new Map();
    this.commandOutputDeltas = new Map();
    this.filesTouched = new Set();
    this.finalMessage = "";
    this.status = "inProgress";
    this.errorMessage = "";
    this.tokenUsage = null;
    this.compactions = 0;
    this.messageAfterCompaction = null;
    this.derailedByCompaction = false;
  }

  headerLine() {
    const firstLine = this.prompt.split(/\r?\n/, 1)[0] ?? "";
    return `\n=== ${new Date(this.startedAt).toISOString()} label=${this.label} thread=${this.threadId} turn=${this.turnId} prompt=${oneLine(firstLine, 160)} ===`;
  }

  footerLine() {
    const durationMs = Date.now() - this.startedAt;
    const compactions = this.compactions > 0 ? ` compactions=${this.compactions}` : "";
    return `=== status=${this.status}${compactions} tokens=${formatTokenUsage(this.tokenUsage)} durationMs=${durationMs} ===`;
  }

  // A mid-turn compaction can end the turn and let codex continue the same
  // work in a fresh turn. Carry the accumulated report across so the client
  // that is still blocked gets one report covering both halves.
  adoptContinuation(turnId) {
    this.turnId = turnId;
    this.status = "inProgress";
    this.errorMessage = "";
    this.messageAfterCompaction = null;
    this.derailedByCompaction = false;
  }

  handleNotification(method, params) {
    switch (method) {
      case "item/started":
        return this.handleItemStarted(params.item);
      case "item/completed":
        return this.handleItemCompleted(params.item);
      case "item/agentMessage/delta":
        this.appendDelta(this.agentMessageDeltas, params.itemId, params.delta);
        return [];
      case "item/reasoning/summaryTextDelta":
        this.appendIndexedDelta(this.reasoningSummaryDeltas, params.itemId, params.summaryIndex, params.delta);
        return [];
      case "item/reasoning/textDelta":
        this.appendIndexedDelta(this.reasoningTextDeltas, params.itemId, params.contentIndex, params.delta);
        return [];
      case "item/commandExecution/outputDelta":
        this.appendDelta(this.commandOutputDeltas, params.itemId, params.delta);
        return [];
      case "item/fileChange/outputDelta":
        return params.delta ? [`[edit] ${params.itemId}: ${truncate(params.delta, 500)}`] : [];
      case "thread/tokenUsage/updated":
        this.tokenUsage = params.tokenUsage;
        return [];
      case "turn/completed":
        this.status = params.turn?.status ?? this.status;
        this.errorMessage = params.turn?.error?.message ?? "";
        // Amnesia signature: after a mid-turn context compaction, Codex emits
        // only its instruction-loading acknowledgement as the final answer.
        // Match the observed message rather than treating a text-only answer
        // as failure: reasoning and an agent message can be a complete turn.
        if (this.status === "completed" && isCompactionAmnesiaMessage(this.messageAfterCompaction)) {
          this.status = "failed";
          this.derailedByCompaction = true;
        }
        return [];
      default:
        return [];
    }
  }

  appendDelta(map, itemId, delta) {
    map.set(itemId, `${map.get(itemId) ?? ""}${delta ?? ""}`);
  }

  appendIndexedDelta(map, itemId, index, delta) {
    const key = `${itemId}:${index ?? 0}`;
    map.set(key, `${map.get(key) ?? ""}${delta ?? ""}`);
  }

  handleItemStarted(item) {
    if (!item?.id) {
      return [];
    }
    this.items.set(item.id, item);
    if (item.type === "commandExecution") {
      const line = `[cmd] ${oneLine(item.command)}`;
      this.fullLines.push(line);
      return [line];
    }
    if (item.type === "fileChange") {
      const files = this.recordFileChanges(item);
      const line = `[edit] ${files.join(", ") || item.id} (${item.status ?? "started"})`;
      this.fullLines.push(line);
      return [line];
    }
    if (item.type === "mcpToolCall") {
      const line = `[mcp] ${item.server}/${item.tool} (${item.status ?? "started"})`;
      this.fullLines.push(line);
      return [line];
    }
    return [];
  }

  handleItemCompleted(item) {
    if (!item?.id) {
      return [];
    }
    this.items.set(item.id, item);

    let line = null;
    switch (item.type) {
      case "contextCompaction": {
        this.compactions += 1;
        this.messageAfterCompaction = null;
        line = `[compaction] context compacted mid-turn (#${this.compactions})`;
        break;
      }
      case "agentMessage": {
        const deltaText = this.agentMessageDeltas.get(item.id);
        const text = item.text || deltaText || "";
        this.finalMessage = text || this.finalMessage;
        if (this.compactions > 0) {
          this.messageAfterCompaction = text;
        }
        // Never truncated: this is the answer, and --report full is built from
        // these lines. Capping it here made `full` deliver less than `brief`.
        line = `[msg] ${text}`;
        break;
      }
      case "reasoning": {
        const deltaSummary = [...this.reasoningSummaryDeltas.entries()]
          .filter(([key]) => key.startsWith(`${item.id}:`))
          .sort()
          .map(([, value]) => value)
          .join("\n");
        const deltaContent = [...this.reasoningTextDeltas.entries()]
          .filter(([key]) => key.startsWith(`${item.id}:`))
          .sort()
          .map(([, value]) => value)
          .join("\n");
        const text = [...asTextList(item.summary), ...asTextList(item.content), deltaSummary, deltaContent]
          .filter(Boolean)
          .join("\n");
        line = text ? `[reasoning]\n${truncate(text, 4000)}` : "[reasoning]";
        break;
      }
      case "commandExecution": {
        const streamed = this.commandOutputDeltas.get(item.id);
        const output = item.aggregatedOutput ?? streamed ?? "";
        line = `[cmd] ${oneLine(item.command)} status=${item.status ?? "unknown"} exit=${item.exitCode ?? "n/a"}${output ? `\n${truncate(output, 1600)}` : ""}`;
        break;
      }
      case "fileChange": {
        const files = this.recordFileChanges(item);
        line = `[edit] ${files.join(", ") || item.id} status=${item.status ?? "unknown"}`;
        break;
      }
      case "mcpToolCall":
        line = `[mcp] ${item.server}/${item.tool} status=${item.status ?? "unknown"}`;
        break;
      case "dynamicToolCall":
        line = `[tool] ${item.namespace ? `${item.namespace}/` : ""}${item.tool} status=${item.status ?? "unknown"}`;
        break;
      case "webSearch":
        line = `[web] ${item.query}`;
        break;
      case "plan":
        line = `[plan] ${truncate(item.text, 1200)}`;
        break;
      case "collabAgentToolCall":
        line = `[agent] ${item.tool} ${item.receiverThreadIds?.join(",") ?? ""} status=${item.status ?? "unknown"}`;
        break;
      case "enteredReviewMode":
      case "exitedReviewMode":
        line = `[review] ${truncate(item.review, 1200)}`;
        break;
      default:
        line = `[item] ${item.type ?? "unknown"} ${item.id}`;
    }

    if (line) {
      this.itemLines.push(line);
      this.fullLines.push(line);
      return [line];
    }
    return [];
  }

  recordFileChanges(item) {
    const files = [];
    for (const change of item.changes ?? []) {
      if (change?.path) {
        this.filesTouched.add(change.path);
        files.push(change.path);
      }
    }
    return files;
  }

  buildReport(reportLevel) {
    let body = this.finalMessage || this.errorMessage || `(turn ${this.status})`;
    if (this.derailedByCompaction) {
      body = [
        "WARNING: turn derailed by mid-turn context compaction. The model lost",
        "its working context, acknowledged its instructions, and stopped without",
        "doing further work. Anything done before the compaction is on disk but",
        "unreported. Send a follow-up prompt to resume (point it at the diff and",
        "its notes file).",
        "",
        `Final message from the model: ${body}`
      ].join("\n");
    }
    const footer = [
      `status: ${this.status}`,
      ...(this.compactions > 0 ? [`compactions: ${this.compactions}`] : []),
      `files touched: ${this.filesTouched.size ? [...this.filesTouched].sort().join(", ") : "none"}`,
      `tokens: ${formatTokenUsage(this.tokenUsage)}`,
      `duration: ${formatDuration(Date.now() - this.startedAt)}`
    ].join("\n");

    if (reportLevel === "full") {
      return [this.headerLine(), ...this.fullLines, this.footerLine()].join("\n");
    }
    if (reportLevel === "items") {
      return [body, "", ...this.itemLines, "", footer].filter((line) => line !== null).join("\n");
    }
    return `${body}\n\n${footer}`;
  }
}
