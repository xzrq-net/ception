import process from "node:process";

import { AppServerClient } from "./appserver.mjs";

// Exact units — a 30m window that prints "1h" is worse than "30m".
function windowLength(mins) {
  if (typeof mins !== "number" || !Number.isFinite(mins)) {
    return "?";
  }
  if (mins >= 1440 && mins % 1440 === 0) {
    return `${mins / 1440}d`;
  }
  if (mins >= 60 && mins % 60 === 0) {
    return `${mins / 60}h`;
  }
  return `${mins}m`;
}

function untilReset(resetsAt) {
  if (typeof resetsAt !== "number" || !Number.isFinite(resetsAt)) {
    return "reset time unknown";
  }
  const when = new Date(resetsAt * 1000);
  const mins = Math.max(0, Math.round((when.getTime() - Date.now()) / 60000));
  const left =
    mins >= 1440
      ? `${Math.floor(mins / 1440)}d ${Math.floor((mins % 1440) / 60)}h`
      : mins >= 60
        ? `${Math.floor(mins / 60)}h ${mins % 60}m`
        : `${mins}m`;
  return `resets in ${left} (${when.toISOString().slice(0, 16).replace("T", " ")}Z)`;
}

// Which real window sits in each slot changes as OpenAI reshuffles them, so
// report each slot's own length rather than assuming either is the short one.
function windowValue(entry) {
  if (!entry || typeof entry.usedPercent !== "number") {
    return "not reported";
  }
  return `${windowLength(entry.windowDurationMins).padEnd(4)} ${entry.usedPercent}% used, ${untilReset(entry.resetsAt)}`;
}

function creditsValue(credits) {
  if (credits.unlimited) {
    return "unlimited";
  }
  if (!credits.hasCredits) {
    return "none";
  }
  return credits.balance ?? "balance not reported";
}

function snapshotRows(label, snapshot) {
  const reported = [
    ["primary", snapshot?.primary],
    ["secondary", snapshot?.secondary]
  ].filter(([, window]) => window && typeof window.usedPercent === "number");
  if (reported.length === 0) {
    return [[label, "not reported"]];
  }
  if (reported.length === 1) {
    return [[label, windowValue(reported[0][1])]];
  }
  return reported.map(([slot, window]) => [`${label} ${slot}`, windowValue(window)]);
}

export function formatQuota(response) {
  const account = response?.rateLimits ?? {};
  const rows = [
    ["primary", windowValue(account.primary)],
    ["secondary", windowValue(account.secondary)]
  ];

  // The account snapshot repeats in the by-id map under its own limit id
  // (the server's fallback key is "codex"); skip it there.
  const accountKey = account.limitId ?? "codex";
  for (const [limitId, limit] of Object.entries(response?.rateLimitsByLimitId ?? {})) {
    if (limitId !== accountKey) {
      rows.push(...snapshotRows(limit?.limitName ?? limitId, limit));
    }
  }

  if (account.credits) {
    rows.push(["credits", creditsValue(account.credits)]);
  }
  if (account.spendControlReached) {
    rows.push(["spend", "limit reached"]);
  }
  if (account.rateLimitReachedType) {
    rows.push(["reached", account.rateLimitReachedType]);
  }

  const width = Math.max(...rows.map(([label]) => label.length)) + 2;
  return rows.map(([label, value]) => `${label.padEnd(width)}${value}`).join("\n");
}

// Daemon-free: quota is an account fact, answered by a throwaway app-server.
export async function cliQuota({ cwd, json }) {
  const app = new AppServerClient({ cwd, log: () => {} });
  try {
    await app.initialize();
    const response = await app.request("account/rateLimits/read", {});
    process.stdout.write(`${json ? JSON.stringify(response, null, 2) : formatQuota(response)}\n`);
  } finally {
    await app.close().catch(() => {});
  }
}
