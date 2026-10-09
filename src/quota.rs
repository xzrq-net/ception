//! `ception quota`: the account's rate-limit windows from a throwaway
//! app-server. Port of lib/quota.mjs.

use std::path::Path;

use anyhow::Result;
use jiff::Timestamp;
use serde_json::{Value, json};

use crate::appserver::AppServer;
use crate::render::{js_number, js_string, js_string_or, truthy};

fn window_length(mins: Option<&Value>) -> String {
    let Some(mins) = mins.and_then(Value::as_f64) else {
        return "?".to_string();
    };
    if mins >= 1440.0 && mins % 1440.0 == 0.0 {
        return format!("{}d", js_number(mins / 1440.0));
    }
    if mins >= 60.0 && mins % 60.0 == 0.0 {
        return format!("{}h", js_number(mins / 60.0));
    }
    format!("{}m", js_number(mins))
}

fn until_reset(resets_at: Option<&Value>, now: Timestamp) -> String {
    let unknown = || "reset time unknown".to_string();
    let Some(seconds) = resets_at.and_then(Value::as_f64) else {
        return unknown();
    };
    // `new Date(ms)` drops the fraction; out of range is unknown, not a crash.
    let Ok(when) = Timestamp::from_millisecond((seconds * 1000.0) as i64) else {
        return unknown();
    };
    let delta_ms = when.as_millisecond() - now.as_millisecond();
    let mins = (delta_ms as f64 / 60_000.0).round().max(0.0) as i64;
    let left = if mins >= 1440 {
        format!("{}d {}h", mins / 1440, mins % 1440 / 60)
    } else if mins >= 60 {
        format!("{}h {}m", mins / 60, mins % 60)
    } else {
        format!("{mins}m")
    };
    format!("resets in {left} ({}Z)", when.strftime("%Y-%m-%d %H:%M"))
}

fn used_percent(window: Option<&Value>) -> Option<&Value> {
    window?.get("usedPercent").filter(|used| used.is_number())
}

/// OpenAI reshuffles which real window sits in each slot, so report each
/// slot's own length.
fn window_value(window: Option<&Value>, now: Timestamp) -> String {
    let Some(used) = used_percent(window) else {
        return "not reported".to_string();
    };
    let window = window.unwrap_or(&Value::Null);
    format!(
        "{:<4} {}% used, {}",
        window_length(window.get("windowDurationMins")),
        js_string(Some(used)),
        until_reset(window.get("resetsAt"), now),
    )
}

fn credits_value(credits: &Value) -> String {
    if truthy(credits.get("unlimited")) {
        return "unlimited".to_string();
    }
    if !truthy(credits.get("hasCredits")) {
        return "none".to_string();
    }
    js_string_or(credits.get("balance"), "balance not reported")
}

fn snapshot_rows(label: &str, snapshot: &Value, now: Timestamp) -> Vec<(String, String)> {
    let reported: Vec<(&str, Option<&Value>)> = ["primary", "secondary"]
        .into_iter()
        .map(|slot| (slot, snapshot.get(slot)))
        .filter(|(_, window)| used_percent(*window).is_some())
        .collect();
    match reported.as_slice() {
        [] => vec![(label.to_string(), "not reported".to_string())],
        [(_, window)] => vec![(label.to_string(), window_value(*window, now))],
        _ => reported.iter().map(|(slot, window)| (format!("{label} {slot}"), window_value(*window, now))).collect(),
    }
}

/// The `ception quota` table. `now` is injected so tests can pin reset times.
pub fn format_quota(response: &Value, now: Timestamp) -> String {
    let account = response.get("rateLimits").unwrap_or(&Value::Null);
    let mut rows = vec![
        ("primary".to_string(), window_value(account.get("primary"), now)),
        ("secondary".to_string(), window_value(account.get("secondary"), now)),
    ];

    // The account snapshot repeats in the by-id map under its own limit id
    // (the server's fallback key is "codex"); skip it there.
    let account_key = js_string_or(account.get("limitId"), "codex");
    if let Some(by_id) = response.get("rateLimitsByLimitId").and_then(Value::as_object) {
        for (limit_id, limit) in by_id {
            if *limit_id != account_key {
                let label = js_string_or(limit.get("limitName"), limit_id);
                rows.extend(snapshot_rows(&label, limit, now));
            }
        }
    }

    if let Some(credits) = account.get("credits").filter(|credits| truthy(Some(credits))) {
        rows.push(("credits".to_string(), credits_value(credits)));
    }
    if truthy(account.get("spendControlReached")) {
        rows.push(("spend".to_string(), "limit reached".to_string()));
    }
    if truthy(account.get("rateLimitReachedType")) {
        rows.push(("reached".to_string(), js_string(account.get("rateLimitReachedType"))));
    }

    let width = rows.iter().map(|(label, _)| label.chars().count()).max().unwrap_or(0) + 2;
    let lines: Vec<String> = rows.iter().map(|(label, value)| format!("{label:<width$}{value}")).collect();
    lines.join("\n")
}

pub async fn run(cwd: &Path, json_output: bool) -> Result<()> {
    let (mut app, mut events) = AppServer::spawn(cwd, None)?;
    let result = async {
        app.initialize(&mut events, |_| {}).await?;
        app.call(&mut events, "account/rateLimits/read", json!({}), |_| {}).await
    }
    .await;
    app.close().await;
    let response = result?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else {
        println!("{}", format_quota(&response, jiff::Timestamp::now()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // Expected tables were produced by lib/quota.mjs with Date.now pinned to NOW.
    fn now() -> Timestamp {
        "2026-10-08T12:00:00Z".parse().unwrap()
    }

    fn hours_from_now(hours: i64) -> i64 {
        now().as_second() + hours * 3600
    }

    #[test]
    fn windows_per_limit_rows_and_account_dedup_render_together() {
        // The account snapshot repeats in the by-id map under a non-"codex" key;
        // "spark" is unnamed and secondary-only; "Some Model" reports both slots.
        let out = format_quota(
            &json!({
                "rateLimits": {
                    "limitId": "codex-enterprise",
                    "limitName": "Enterprise",
                    "primary": { "usedPercent": 12, "windowDurationMins": 300, "resetsAt": hours_from_now(3) },
                    "secondary": { "usedPercent": 47, "windowDurationMins": 10080, "resetsAt": hours_from_now(50) }
                },
                "rateLimitsByLimitId": {
                    "codex-enterprise": {
                        "limitId": "codex-enterprise",
                        "limitName": "Enterprise",
                        "primary": { "usedPercent": 12, "windowDurationMins": 300 }
                    },
                    "spark": { "limitId": "spark", "limitName": null, "secondary": { "usedPercent": 8, "windowDurationMins": 10080 } },
                    "other": {
                        "limitId": "other",
                        "limitName": "Some Model",
                        "primary": { "usedPercent": 1, "windowDurationMins": 300 },
                        "secondary": { "usedPercent": 2, "windowDurationMins": 10080 }
                    }
                }
            }),
            now(),
        );

        assert_eq!(out.matches("12% used").count(), 1, "the account snapshot printed twice");
        // Per-limit rows follow serde_json's key order (sorted), where the JS
        // followed insertion order and printed "spark" first. Codex sends the
        // map from a HashMap, so neither order is meaningful.
        assert_eq!(
            out,
            [
                "primary               5h   12% used, resets in 3h 0m (2026-10-08 15:00Z)",
                "secondary             7d   47% used, resets in 2d 2h (2026-10-10 14:00Z)",
                "Some Model primary    5h   1% used, reset time unknown",
                "Some Model secondary  7d   2% used, reset time unknown",
                "spark                 7d   8% used, reset time unknown",
            ]
            .join("\n")
        );
    }

    #[test]
    fn odd_window_lengths_and_missing_data_degrade_legibly() {
        let out = format_quota(
            &json!({
                "rateLimits": {
                    "primary": { "usedPercent": 3, "windowDurationMins": 90, "resetsAt": null },
                    "secondary": null
                }
            }),
            now(),
        );

        // 90m must not round into "2h", an absent reset time must say so, and an
        // absent window must not vanish.
        assert_eq!(out, "primary    90m  3% used, reset time unknown\nsecondary  not reported");
    }

    #[test]
    fn credit_and_limit_states_appear_only_when_the_account_reports_them() {
        let bare = format_quota(&json!({ "rateLimits": { "primary": null, "secondary": null } }), now());
        assert_eq!(bare, "primary    not reported\nsecondary  not reported");

        let flagged = format_quota(
            &json!({
                "rateLimits": {
                    "primary": null,
                    "secondary": null,
                    "credits": { "hasCredits": true, "unlimited": false, "balance": "12.50" },
                    "spendControlReached": true,
                    "rateLimitReachedType": "primary"
                }
            }),
            now(),
        );
        assert_eq!(
            flagged,
            "primary    not reported\nsecondary  not reported\ncredits    12.50\nspend      limit reached\nreached    primary"
        );

        // The balance is nullable even when hasCredits is true.
        let no_balance = format_quota(
            &json!({
                "rateLimits": {
                    "primary": null,
                    "secondary": null,
                    "credits": { "hasCredits": true, "unlimited": false, "balance": null }
                }
            }),
            now(),
        );
        assert!(no_balance.ends_with("\ncredits    balance not reported"), "{no_balance}");
    }

    // --- Judgment calls made by the port ---

    #[test]
    fn float_percentages_print_like_js_numbers() {
        // serde_json would print 12.0; JS prints 12.
        let out = format_quota(
            &json!({ "rateLimits": { "primary": { "usedPercent": 12.0, "windowDurationMins": 1440.0 } } }),
            now(),
        );
        assert!(out.starts_with("primary    1d   12% used, reset time unknown\n"), "{out}");
    }

    #[test]
    fn past_and_unrepresentable_reset_times() {
        let out = format_quota(
            &json!({ "rateLimits": {
                "primary": { "usedPercent": 1, "windowDurationMins": 300, "resetsAt": hours_from_now(-1) },
                // The JS threw a RangeError from toISOString here.
                "secondary": { "usedPercent": 2, "windowDurationMins": 300, "resetsAt": 1e300 }
            } }),
            now(),
        );
        assert_eq!(
            out,
            "primary    5h   1% used, resets in 0m (2026-10-08 11:00Z)\nsecondary  5h   2% used, reset time unknown"
        );
    }
}
