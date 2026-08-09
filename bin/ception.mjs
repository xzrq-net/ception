#!/usr/bin/env node
import process from "node:process";
import { parseArgs } from "node:util";

import {
  cliGoal,
  cliInterrupt,
  cliKill,
  cliList,
  cliSend,
  cliSpawn,
  cliSpawnOrphan,
  cliWatch
} from "../lib/client.mjs";
import { runDaemon } from "../lib/daemon.mjs";
import { validateLabel } from "../lib/state.mjs";

function usage(message) {
  if (message) {
    console.error(message);
  }
  console.error(`usage:
  ception spawn --label L [--cwd D] [--model M] [--effort E] [--report brief|items|full] [PROMPT | -]
  ception send L [--cwd D] [--report brief|items|full] [PROMPT | -]
  ception goal L [--cwd D] [--report brief|items|full] [OBJECTIVE | -]
  ception goal L --resume | --pause | --show | --clear [--cwd D]
  ception interrupt L [--cwd D]
  ception kill L | --all [--cwd D]
  ception list [--all] [--json] [--cwd D]
  ception watch L [--cwd D] [--report brief|items|full] [--follow]

labels are scoped to the project root resolved from the invocation cwd
(or --cwd); a label spawned with --cwd must be addressed with the same --cwd`);
}

function normalizeReport(report) {
  const value = report ?? "brief";
  if (!["brief", "items", "full"].includes(value)) {
    throw new UsageError(`invalid --report ${value}`);
  }
  return value;
}

class UsageError extends Error {
  constructor(message) {
    super(message);
    this.exitCode = 4;
  }
}

async function readStdin() {
  const chunks = [];
  for await (const chunk of process.stdin) {
    chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
  }
  return Buffer.concat(chunks).toString("utf8");
}

async function promptFromPositionals(positionals, offset = 0) {
  const promptParts = positionals.slice(offset);
  if (promptParts.length > 0) {
    if (promptParts.length === 1 && promptParts[0] === "-") {
      return readStdin();
    }
    return promptParts.join(" ");
  }
  if (!process.stdin.isTTY) {
    return readStdin();
  }
  return "";
}

function requireLabel(label) {
  if (!label) {
    throw new UsageError("missing label");
  }
  validateLabel(label);
  return label;
}

async function main(argv) {
  const [command, ...rest] = argv;

  // `spawn --help` used to die on parseArgs' positional advice. Only honour
  // the flag before `--`, so it stays usable as prompt text after it.
  const flagArgs = rest.slice(0, rest.indexOf("--") === -1 ? rest.length : rest.indexOf("--"));
  if (flagArgs.includes("--help") || flagArgs.includes("-h")) {
    usage();
    return;
  }

  if (command === "daemon") {
    await runDaemon(rest);
    return;
  }

  if (command === "spawn-orphan") {
    await cliSpawnOrphan(rest);
    return;
  }

  switch (command) {
    case "spawn": {
      const { values, positionals } = parseArgs({
        args: rest,
        allowPositionals: true,
        options: {
          label: { type: "string" },
          cwd: { type: "string" },
          model: { type: "string" },
          effort: { type: "string" },
          report: { type: "string" }
        }
      });
      const label = requireLabel(values.label);
      const prompt = await promptFromPositionals(positionals);
      if (!prompt.trim()) {
        throw new UsageError("spawn requires a prompt");
      }
      await cliSpawn({
        label,
        cwd: values.cwd ?? process.cwd(),
        prompt,
        model: values.model,
        effort: values.effort,
        report: normalizeReport(values.report)
      });
      return;
    }

    case "send": {
      const { values, positionals } = parseArgs({
        args: rest,
        allowPositionals: true,
        options: {
          report: { type: "string" },
          cwd: { type: "string" }
        }
      });
      const label = requireLabel(positionals[0]);
      const prompt = await promptFromPositionals(positionals, 1);
      if (!prompt.trim()) {
        throw new UsageError("send requires a prompt");
      }
      await cliSend({
        label,
        cwd: values.cwd ?? process.cwd(),
        prompt,
        report: normalizeReport(values.report)
      });
      return;
    }

    case "goal": {
      const { values, positionals } = parseArgs({
        args: rest,
        allowPositionals: true,
        options: {
          cwd: { type: "string" },
          report: { type: "string" },
          resume: { type: "boolean" },
          pause: { type: "boolean" },
          show: { type: "boolean" },
          clear: { type: "boolean" }
        }
      });
      const label = requireLabel(positionals[0]);
      const chosen = ["resume", "pause", "show", "clear"].filter((name) => values[name]);
      if (chosen.length > 1) {
        throw new UsageError(`goal takes one of --${chosen.join(", --")}`);
      }
      const objective = chosen.length > 0 ? "" : await promptFromPositionals(positionals, 1);
      if (chosen.length === 0 && !objective.trim()) {
        throw new UsageError("goal requires an objective, or one of --resume/--pause/--show/--clear");
      }
      await cliGoal({
        label,
        cwd: values.cwd ?? process.cwd(),
        action: chosen[0] ?? "set",
        objective: objective.trim() || null,
        report: normalizeReport(values.report)
      });
      return;
    }

    case "interrupt": {
      const { values, positionals } = parseArgs({
        args: rest,
        allowPositionals: true,
        options: {
          cwd: { type: "string" }
        }
      });
      await cliInterrupt({
        label: requireLabel(positionals[0]),
        cwd: values.cwd ?? process.cwd()
      });
      return;
    }

    case "kill": {
      const { values, positionals } = parseArgs({
        args: rest,
        allowPositionals: true,
        options: {
          all: { type: "boolean" },
          cwd: { type: "string" }
        }
      });
      if (!values.all && !positionals[0]) {
        throw new UsageError("kill requires a label or --all");
      }
      await cliKill({
        label: values.all ? null : requireLabel(positionals[0]),
        all: Boolean(values.all),
        cwd: values.cwd ?? process.cwd()
      });
      return;
    }

    case "list": {
      const { values } = parseArgs({
        args: rest,
        options: {
          all: { type: "boolean" },
          json: { type: "boolean" },
          cwd: { type: "string" }
        }
      });
      await cliList({
        all: Boolean(values.all),
        json: Boolean(values.json),
        cwd: values.cwd ?? process.cwd()
      });
      return;
    }

    case "watch": {
      const { values, positionals } = parseArgs({
        args: rest,
        allowPositionals: true,
        options: {
          cwd: { type: "string" },
          report: { type: "string" },
          follow: { type: "boolean" }
        }
      });
      await cliWatch({
        label: requireLabel(positionals[0]),
        cwd: values.cwd ?? process.cwd(),
        report: normalizeReport(values.report),
        follow: Boolean(values.follow)
      });
      return;
    }

    case "-h":
    case "--help":
    case undefined:
      usage();
      process.exitCode = command ? 0 : 4;
      return;

    default:
      throw new UsageError(`unknown command ${command}`);
  }
}

main(process.argv.slice(2)).catch((error) => {
  if (error instanceof UsageError) {
    usage(error.message);
    process.exitCode = 4;
    return;
  }
  console.error(error?.message ?? String(error));
  process.exitCode = error?.exitCode ?? 4;
});
