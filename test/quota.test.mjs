import assert from "node:assert/strict";
import test from "node:test";

import { formatQuota } from "../lib/quota.mjs";

function hoursFromNow(hours) {
  return Math.round((Date.now() + hours * 3600000) / 1000);
}

test("windows, per-limit rows and account dedup render together", () => {
  // The account snapshot repeats in the by-id map under a non-"codex" key;
  // "spark" is unnamed and secondary-only; "Some Model" reports both slots.
  const out = formatQuota({
    rateLimits: {
      limitId: "codex-enterprise",
      limitName: "Enterprise",
      primary: { usedPercent: 12, windowDurationMins: 300, resetsAt: hoursFromNow(3) },
      secondary: { usedPercent: 47, windowDurationMins: 10080, resetsAt: hoursFromNow(50) }
    },
    rateLimitsByLimitId: {
      "codex-enterprise": {
        limitId: "codex-enterprise",
        limitName: "Enterprise",
        primary: { usedPercent: 12, windowDurationMins: 300 }
      },
      spark: { limitId: "spark", limitName: null, secondary: { usedPercent: 8, windowDurationMins: 10080 } },
      other: {
        limitId: "other",
        limitName: "Some Model",
        primary: { usedPercent: 1, windowDurationMins: 300 },
        secondary: { usedPercent: 2, windowDurationMins: 10080 }
      }
    }
  });

  assert.match(out, /^primary\s+5h\s+12% used, resets in 3h/m);
  assert.match(out, /^secondary\s+7d\s+47% used, resets in 2d 2h/m);
  assert.equal(out.match(/12% used/g).length, 1, "the account snapshot printed twice");
  assert.match(out, /^spark\s+7d\s+8% used/m);
  assert.match(out, /^Some Model primary\s+5h\s+1% used/m);
  assert.match(out, /^Some Model secondary\s+7d\s+2% used/m);
});

test("odd window lengths and missing data degrade legibly", () => {
  const out = formatQuota({
    rateLimits: {
      primary: { usedPercent: 3, windowDurationMins: 90, resetsAt: null },
      secondary: null
    }
  });

  // 90m must not round into "2h", an absent reset time must say so, and an
  // absent window must not vanish.
  assert.match(out, /^primary\s+90m\s+3% used, reset time unknown$/m);
  assert.match(out, /^secondary\s+not reported$/m);
});

test("credit and limit states appear only when the account reports them", () => {
  const bare = formatQuota({ rateLimits: { primary: null, secondary: null } });
  assert.doesNotMatch(bare, /credits|spend|reached/);

  const flagged = formatQuota({
    rateLimits: {
      primary: null,
      secondary: null,
      credits: { hasCredits: true, unlimited: false, balance: "12.50" },
      spendControlReached: true,
      rateLimitReachedType: "primary"
    }
  });
  assert.match(flagged, /^credits\s+12\.50$/m);
  assert.match(flagged, /^spend\s+limit reached$/m);
  assert.match(flagged, /^reached\s+primary$/m);

  // The balance is nullable even when hasCredits is true.
  const noBalance = formatQuota({
    rateLimits: { primary: null, secondary: null, credits: { hasCredits: true, unlimited: false, balance: null } }
  });
  assert.match(noBalance, /^credits\s+balance not reported$/m);
});
