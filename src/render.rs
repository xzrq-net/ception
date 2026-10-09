//! Turn reports and log lines. Port of lib/render.mjs.
//!
//! The text here is an interface: agents grep the log prefixes and the report
//! footer, so wording and truncation lengths follow the JS original.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How much of a turn the client's report carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ReportLevel {
    /// Final message plus footer.
    #[default]
    Brief,
    /// Adds one line per command/edit/tool call.
    Items,
    /// Everything the log gets, reasoning included.
    Full,
}

// --- JS value semantics over serde_json::Value ---------------------------
//
// App-server payloads are loose JSON; these mirror what the JS did with them
// so the rendered text matches. JSON null reads as absent, like `?.`/`??`.

/// JS truthiness.
pub(crate) fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// JS `String(v)` as a template string interpolates it, except that absent
/// and null print as "" instead of `undefined`/`null`.
pub(crate) fn js_string(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => i.to_string(),
            (_, Some(u)) => u.to_string(),
            _ => js_number(n.as_f64().unwrap_or(f64::NAN)),
        },
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => {
            let parts: Vec<String> = items.iter().map(|item| js_string(Some(item))).collect();
            parts.join(",")
        }
        Some(Value::Object(_)) => "[object Object]".to_string(),
    }
}

/// JS `String(v ?? fallback)`.
pub(crate) fn js_string_or(v: Option<&Value>, fallback: &str) -> String {
    match v {
        None | Some(Value::Null) => fallback.to_string(),
        Some(v) => js_string(Some(v)),
    }
}

/// JS number-to-string: whole floats print without a fraction (`12`, not
/// serde_json's `12.0`). Exponent forms for huge/tiny magnitudes not mirrored.
pub(crate) fn js_number(f: f64) -> String {
    if f == 0.0 {
        // Also -0, which JS prints as "0".
        return "0".to_string();
    }
    f.to_string()
}

/// `v ?? null`: the value unless absent or JSON null.
fn present(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| !v.is_null())
}

/// JS regex `\s`: Unicode White_Space minus U+0085, plus U+FEFF.
fn is_js_space(c: char) -> bool {
    (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}'
}

/// Caps `text` at `max` chars, marking the cut with `…`.
fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_string(),
        Some((cut, _)) => format!("{}…", &text[..cut]),
    }
}

/// Collapses whitespace runs to single spaces, trims, then truncates.
fn one_line(text: &str, max: usize) -> String {
    let words: Vec<&str> = text.split(is_js_space).filter(|w| !w.is_empty()).collect();
    truncate(&words.join(" "), max)
}

/// Strings and `{text}` entries of an array, empties dropped.
fn text_list(v: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(entries)) = v else {
        return Vec::new();
    };
    entries
        .iter()
        .map(|entry| match entry {
            Value::String(s) => s.clone(),
            other => js_string(other.get("text")),
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// `/^Instructions loaded(?: for [^\r\n]+)?\.$/u` against the trimmed text.
fn is_compaction_amnesia_message(text: &str) -> bool {
    let trimmed = text.trim_matches(is_js_space);
    let Some(rest) = trimmed.strip_prefix("Instructions loaded") else {
        return false;
    };
    let Some(rest) = rest.strip_suffix('.') else {
        return false;
    };
    rest.is_empty()
        || rest
            .strip_prefix(" for ")
            .is_some_and(|target| !target.is_empty() && !target.contains(['\r', '\n']))
}

/// codexErrorInfo is either a bare name or a single-key object carrying
/// details; rendered generically so codex's vocabulary passes through whole.
fn error_info_name(info: Option<&Value>) -> Option<String> {
    match info? {
        Value::String(name) if !name.is_empty() => Some(name.clone()),
        Value::Object(map) => {
            let (name, detail) = map.iter().next()?;
            if name.is_empty() {
                return None;
            }
            let fields: Vec<String> = match detail {
                Value::Object(detail) => detail
                    .iter()
                    .filter(|(_, value)| !value.is_null())
                    .map(|(key, value)| format!("{key}={}", js_string(Some(value))))
                    .collect(),
                _ => Vec::new(),
            };
            if fields.is_empty() {
                Some(name.clone())
            } else {
                Some(format!("{name} ({})", fields.join(", ")))
            }
        }
        _ => None,
    }
}

pub fn format_token_usage(usage: &Value) -> String {
    let usage = present(usage.get("last"))
        .or_else(|| present(usage.get("total")))
        .unwrap_or(usage);
    let Some(total) = usage.get("totalTokens").filter(|v| v.is_number()) else {
        return "n/a".to_string();
    };
    let count = |key: &str| js_string_or(usage.get(key), "0");
    format!(
        "{} total ({} in, {} out, {} reasoning)",
        js_string(Some(total)),
        count("inputTokens"),
        count("outputTokens"),
        count("reasoningOutputTokens"),
    )
}

pub fn format_duration(elapsed: Duration) -> String {
    // The JS measured whole milliseconds; rounding below depends on that.
    let ms = elapsed.as_millis();
    let total_seconds = (ms + 500) / 1000;
    if total_seconds >= 60 {
        return format!("{}m {}s", total_seconds / 60, total_seconds % 60);
    }
    // `toFixed(1)` rounds an exact tie up where `{:.1}` rounds it to even.
    // Of whole-ms values only x.x25/x.x75 seconds are exact ties in binary.
    if ms % 500 == 250 {
        let tenths = (ms + 50) / 100;
        return format!("{}.{}s", tenths / 10, tenths % 10);
    }
    format!("{:.1}s", ms as f64 / 1000.0)
}

/// Codex's own long-run driver. Anything but `active` means it has stopped
/// starting turns, and `complete` is the only stop that means the work is done.
const GOAL_STOPPED_SHORT: [&str; 4] = ["paused", "blocked", "usageLimited", "budgetLimited"];

pub fn format_goal(goal: Option<&Value>, label: Option<&str>) -> String {
    let Some(goal) = goal.filter(|goal| truthy(Some(goal))) else {
        return "no goal set".to_string();
    };
    let status = js_string(goal.get("status"));
    let objective = one_line(&js_string(goal.get("objective")), 300);
    let mut lines = vec![format!("goal: {status} — {objective}")];
    if GOAL_STOPPED_SHORT.contains(&status.as_str()) {
        let resume = match label.filter(|label| !label.is_empty()) {
            Some(label) => format!("ception goal {label} --resume"),
            None => "ception goal <label> --resume".to_string(),
        };
        lines.push(format!(
            "the goal stopped before completing; restart the run with `{resume}`"
        ));
    }
    lines.join("\n")
}

pub fn status_exit_code(status: &str) -> i32 {
    match status {
        "completed" | "ok" | "steered" | "idle" => 0,
        "failed" => 2,
        "interrupted" => 3,
        _ => 4,
    }
}

/// Streamed text keyed by item id, then by summary/content index.
type IndexedDeltas = HashMap<String, BTreeMap<i64, String>>;

/// Accumulates one logical turn (possibly spanning continuation turns) from
/// app-server notifications.
pub struct TurnAccumulator {
    pub label: String,
    pub thread_id: String,
    pub turn_id: String,
    pub prompt: String,
    pub status: String,
    pub compactions: u32,
    pub derailed_by_compaction: bool,
    /// Wall clock for the log header; `started` times the turn.
    started_at: jiff::Timestamp,
    started: Instant,
    /// Set when the run settles, so a report built later (`watch --run`)
    /// keeps the run's own duration.
    settled: Option<Duration>,
    /// What `items` reports carry: one line per completed item.
    item_lines: Vec<String>,
    /// What `full` reports and the log carry: started items too.
    full_lines: Vec<String>,
    reasoning_summary_deltas: IndexedDeltas,
    reasoning_text_deltas: IndexedDeltas,
    agent_message_deltas: HashMap<String, String>,
    command_output_deltas: HashMap<String, String>,
    files_touched: BTreeSet<String>,
    final_message: String,
    error_message: String,
    error_info: Option<String>,
    token_usage: Value,
    /// The latest agent message since the last compaction, if any.
    message_after_compaction: Option<String>,
}

impl TurnAccumulator {
    pub fn new(label: &str, thread_id: &str, turn_id: &str, prompt: &str) -> Self {
        Self {
            label: label.to_string(),
            thread_id: thread_id.to_string(),
            turn_id: turn_id.to_string(),
            prompt: prompt.to_string(),
            status: "inProgress".to_string(),
            compactions: 0,
            derailed_by_compaction: false,
            started_at: jiff::Timestamp::now(),
            started: Instant::now(),
            settled: None,
            item_lines: Vec::new(),
            full_lines: Vec::new(),
            reasoning_summary_deltas: HashMap::new(),
            reasoning_text_deltas: HashMap::new(),
            agent_message_deltas: HashMap::new(),
            command_output_deltas: HashMap::new(),
            files_touched: BTreeSet::new(),
            final_message: String::new(),
            error_message: String::new(),
            error_info: None,
            token_usage: Value::Null,
            message_after_compaction: None,
        }
    }

    pub fn header_line(&self) -> String {
        let first_line = self.prompt.lines().next().unwrap_or("");
        format!(
            "\n=== {} label={} thread={} turn={} prompt={} ===",
            self.started_at.strftime("%Y-%m-%dT%H:%M:%S%.3fZ"),
            self.label,
            self.thread_id,
            self.turn_id,
            one_line(first_line, 160),
        )
    }

    /// Stop the clock: the run is over.
    pub fn settle(&mut self) {
        self.settled.get_or_insert(self.started.elapsed());
    }

    fn elapsed(&self) -> Duration {
        self.settled.unwrap_or_else(|| self.started.elapsed())
    }

    pub fn footer_line(&self) -> String {
        let error_info = match &self.error_info {
            Some(info) => format!(" errorCode={info}"),
            None => String::new(),
        };
        let compactions = if self.compactions > 0 {
            format!(" compactions={}", self.compactions)
        } else {
            String::new()
        };
        format!(
            "=== status={}{error_info}{compactions} tokens={} durationMs={} ===",
            self.status,
            format_token_usage(&self.token_usage),
            self.elapsed().as_millis(),
        )
    }

    /// A mid-turn compaction can end the turn and let codex continue the same
    /// work in a fresh turn. Carry the accumulated report across so the client
    /// that is still blocked gets one report covering both halves.
    pub fn adopt_continuation(&mut self, turn_id: &str) {
        self.turn_id = turn_id.to_string();
        self.status = "inProgress".to_string();
        self.error_message.clear();
        self.error_info = None;
        self.message_after_compaction = None;
        self.derailed_by_compaction = false;
    }

    /// Feed one notification; returns the log lines it produced.
    pub fn handle_notification(&mut self, method: &str, params: &Value) -> Vec<String> {
        let item = params.get("item").unwrap_or(&Value::Null);
        let item_id = js_string(params.get("itemId"));
        let delta = js_string(params.get("delta"));
        match method {
            "item/started" => self.handle_item_started(item),
            "item/completed" => self.handle_item_completed(item),
            "item/agentMessage/delta" => {
                self.agent_message_deltas.entry(item_id).or_default().push_str(&delta);
                Vec::new()
            }
            "item/reasoning/summaryTextDelta" => {
                append_indexed(&mut self.reasoning_summary_deltas, item_id, params.get("summaryIndex"), &delta);
                Vec::new()
            }
            "item/reasoning/textDelta" => {
                append_indexed(&mut self.reasoning_text_deltas, item_id, params.get("contentIndex"), &delta);
                Vec::new()
            }
            "item/commandExecution/outputDelta" => {
                self.command_output_deltas.entry(item_id).or_default().push_str(&delta);
                Vec::new()
            }
            "item/fileChange/outputDelta" => {
                if !truthy(params.get("delta")) {
                    return Vec::new();
                }
                // `full` promises everything the log gets.
                let line = format!("[edit] {item_id}: {}", truncate(&delta, 500));
                self.full_lines.push(line.clone());
                vec![line]
            }
            "thread/tokenUsage/updated" => {
                self.token_usage = params.get("tokenUsage").cloned().unwrap_or(Value::Null);
                Vec::new()
            }
            "turn/completed" => {
                self.handle_turn_completed(params.get("turn").unwrap_or(&Value::Null));
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn handle_turn_completed(&mut self, turn: &Value) {
        if let Some(status) = present(turn.get("status")) {
            self.status = js_string(Some(status));
        }
        let error = turn.get("error").unwrap_or(&Value::Null);
        self.error_message = js_string(error.get("message"));
        // The only machine-readable handle on why the server ended the turn.
        self.error_info = error_info_name(error.get("codexErrorInfo"));
        // Amnesia signature: after a mid-turn context compaction, Codex emits
        // only its instruction-loading acknowledgement as the final answer.
        // Match the observed message rather than treating a text-only answer
        // as failure: reasoning and an agent message can be a complete turn.
        let amnesia = self
            .message_after_compaction
            .as_deref()
            .is_some_and(is_compaction_amnesia_message);
        if self.status == "completed" && amnesia {
            self.status = "failed".to_string();
            self.derailed_by_compaction = true;
        }
    }

    fn handle_item_started(&mut self, item: &Value) -> Vec<String> {
        if !truthy(item.get("id")) {
            return Vec::new();
        }
        let id = js_string(item.get("id"));
        let status = js_string_or(item.get("status"), "started");
        let line = match item.get("type").and_then(Value::as_str) {
            Some("commandExecution") => {
                format!("[cmd] {}", one_line(&js_string(item.get("command")), 200))
            }
            Some("fileChange") => {
                let files = self.record_file_changes(item);
                format!("[edit] {} ({status})", files_or_id(&files, &id))
            }
            Some("mcpToolCall") => format!(
                "[mcp] {}/{} ({status})",
                js_string(item.get("server")),
                js_string(item.get("tool")),
            ),
            _ => return Vec::new(),
        };
        self.full_lines.push(line.clone());
        vec![line]
    }

    fn handle_item_completed(&mut self, item: &Value) -> Vec<String> {
        if !truthy(item.get("id")) {
            return Vec::new();
        }
        let id = js_string(item.get("id"));
        let status = js_string_or(item.get("status"), "unknown");
        let field = |key: &str| js_string(item.get(key));

        let line = match item.get("type").and_then(Value::as_str).unwrap_or("") {
            "contextCompaction" => {
                self.compactions += 1;
                self.message_after_compaction = None;
                format!("[compaction] context compacted mid-turn (#{})", self.compactions)
            }
            "agentMessage" => {
                let mut text = field("text");
                if text.is_empty() {
                    text = self.agent_message_deltas.get(&id).cloned().unwrap_or_default();
                }
                if !text.is_empty() {
                    self.final_message = text.clone();
                }
                if self.compactions > 0 {
                    self.message_after_compaction = Some(text.clone());
                }
                // Never truncated: this is the answer, and --report full is built
                // from these lines. Capping it here made `full` deliver less than
                // `brief`.
                format!("[msg] {text}")
            }
            "reasoning" => {
                // The completed item repeats what the deltas streamed; use it as
                // the source of truth and fall back to deltas, like agentMessage.
                let mut summary = text_list(item.get("summary")).join("\n");
                if summary.is_empty() {
                    summary = joined_deltas(&self.reasoning_summary_deltas, &id);
                }
                let mut content = text_list(item.get("content")).join("\n");
                if content.is_empty() {
                    content = joined_deltas(&self.reasoning_text_deltas, &id);
                }
                let parts: Vec<&str> =
                    [summary.as_str(), content.as_str()].into_iter().filter(|s| !s.is_empty()).collect();
                let text = parts.join("\n");
                if text.is_empty() {
                    "[reasoning]".to_string()
                } else {
                    format!("[reasoning]\n{}", truncate(&text, 4000))
                }
            }
            "commandExecution" => {
                let output = match present(item.get("aggregatedOutput")) {
                    Some(output) => js_string(Some(output)),
                    None => self.command_output_deltas.get(&id).cloned().unwrap_or_default(),
                };
                let command = one_line(&field("command"), 200);
                let exit = js_string_or(item.get("exitCode"), "n/a");
                let mut line = format!("[cmd] {command} status={status} exit={exit}");
                if !output.is_empty() {
                    line.push('\n');
                    line.push_str(&truncate(&output, 1600));
                }
                line
            }
            "fileChange" => {
                let files = self.record_file_changes(item);
                format!("[edit] {} status={status}", files_or_id(&files, &id))
            }
            "mcpToolCall" => format!("[mcp] {}/{} status={status}", field("server"), field("tool")),
            "dynamicToolCall" => {
                let namespace = if truthy(item.get("namespace")) {
                    format!("{}/", field("namespace"))
                } else {
                    String::new()
                };
                format!("[tool] {namespace}{} status={status}", field("tool"))
            }
            "webSearch" => format!("[web] {}", field("query")),
            "plan" => format!("[plan] {}", truncate(&field("text"), 1200)),
            "collabAgentToolCall" => format!(
                "[agent] {} {} status={status}",
                field("tool"),
                field("receiverThreadIds"),
            ),
            "enteredReviewMode" | "exitedReviewMode" => {
                format!("[review] {}", truncate(&field("review"), 1200))
            }
            _ => format!("[item] {} {id}", js_string_or(item.get("type"), "unknown")),
        };

        self.item_lines.push(line.clone());
        self.full_lines.push(line.clone());
        vec![line]
    }

    fn record_file_changes(&mut self, item: &Value) -> Vec<String> {
        let mut files = Vec::new();
        let Some(Value::Array(changes)) = item.get("changes") else {
            return files;
        };
        for change in changes {
            if truthy(change.get("path")) {
                let path = js_string(change.get("path"));
                self.files_touched.insert(path.clone());
                files.push(path);
            }
        }
        files
    }

    pub fn build_report(&self, level: ReportLevel) -> String {
        let mut body = if self.final_message.is_empty() {
            self.error_message.clone()
        } else {
            self.final_message.clone()
        };
        if self.derailed_by_compaction {
            body = [
                "WARNING: turn derailed by mid-turn context compaction. The model lost",
                "its working context, acknowledged its instructions, and stopped without",
                "doing further work. Anything done before the compaction is on disk but",
                "unreported. Send a follow-up prompt to resume (point it at the diff and",
                "its notes file).",
                "",
                &format!("Final message from the model: {body}"),
            ]
            .join("\n");
        }

        let mut footer = vec![format!("status: {}", self.status)];
        if let Some(info) = &self.error_info {
            footer.push(format!("error code: {info}"));
        }
        if self.compactions > 0 {
            footer.push(format!("compactions: {}", self.compactions));
        }
        let files: Vec<&str> = self.files_touched.iter().map(String::as_str).collect();
        let files = if files.is_empty() { "none".to_string() } else { files.join(", ") };
        footer.push(format!("files touched: {files}"));
        footer.push(format!("tokens: {}", format_token_usage(&self.token_usage)));
        footer.push(format!("duration: {}", format_duration(self.elapsed())));
        let footer = footer.join("\n");

        let no_message = format!("(turn {}, no final message)", self.status);
        match level {
            ReportLevel::Full => {
                let mut lines = vec![self.header_line()];
                lines.extend(self.full_lines.iter().cloned());
                lines.push(self.footer_line());
                lines.join("\n")
            }
            ReportLevel::Items => {
                let mut lines = vec![if body.is_empty() { no_message } else { body }, String::new()];
                lines.extend(self.item_lines.iter().cloned());
                lines.push(String::new());
                lines.push(footer);
                lines.join("\n")
            }
            ReportLevel::Brief => {
                // Turns codex starts for a goal often end without an agent
                // message. An almost empty report is worse than a terse trail of
                // what it did — but brief must stay brief, so the trail is what
                // it touched, not how it thought.
                if body.is_empty() {
                    let acted: Vec<&String> =
                        self.item_lines.iter().filter(|line| !line.starts_with("[reasoning]")).collect();
                    let recent = &acted[acted.len().saturating_sub(20)..];
                    let mut lines = vec![no_message];
                    lines.extend(recent.iter().map(|line| one_line(line, 200)));
                    body = lines.join("\n");
                }
                format!("{body}\n\n{footer}")
            }
        }
    }
}

fn files_or_id(files: &[String], id: &str) -> String {
    if files.is_empty() { id.to_string() } else { files.join(", ") }
}

fn append_indexed(deltas: &mut IndexedDeltas, item_id: String, index: Option<&Value>, delta: &str) {
    let index = index.and_then(Value::as_i64).unwrap_or(0);
    deltas.entry(item_id).or_default().entry(index).or_default().push_str(delta);
}

/// An item's streamed parts in index order, one per line.
fn joined_deltas(deltas: &IndexedDeltas, item_id: &str) -> String {
    let Some(parts) = deltas.get(item_id) else {
        return String::new();
    };
    let parts: Vec<&str> = parts.values().map(String::as_str).collect();
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn accumulator() -> TurnAccumulator {
        TurnAccumulator::new("test", "thread", "turn", "prompt")
    }

    fn complete_item(turn: &mut TurnAccumulator, item: Value) -> Vec<String> {
        turn.handle_notification("item/completed", &json!({ "item": item }))
    }

    fn complete_turn(turn: &mut TurnAccumulator) {
        turn.handle_notification("turn/completed", &json!({ "turn": { "status": "completed" } }));
    }

    #[test]
    fn instruction_acknowledgement_after_compaction_is_treated_as_a_derailed_turn() {
        let mut turn = accumulator();
        complete_item(&mut turn, json!({ "id": "compaction", "type": "contextCompaction" }));
        complete_item(
            &mut turn,
            json!({ "id": "message", "type": "agentMessage", "text": "Instructions loaded for `/tmp/project`." }),
        );
        complete_turn(&mut turn);

        assert_eq!(turn.status, "failed");
        assert!(turn.derailed_by_compaction);
        assert!(
            turn.build_report(ReportLevel::Brief)
                .contains("WARNING: turn derailed by mid-turn context compaction")
        );
    }

    #[test]
    fn instruction_acknowledgement_without_compaction_remains_a_completed_turn() {
        let mut turn = accumulator();
        complete_item(&mut turn, json!({ "id": "message", "type": "agentMessage", "text": "Instructions loaded." }));
        complete_turn(&mut turn);

        assert_eq!(turn.status, "completed");
        assert!(!turn.derailed_by_compaction);
    }

    #[test]
    fn substantive_text_only_answer_after_compaction_remains_completed() {
        let mut turn = accumulator();
        complete_item(&mut turn, json!({ "id": "compaction", "type": "contextCompaction" }));
        complete_item(
            &mut turn,
            json!({ "id": "reasoning", "type": "reasoning", "summary": ["Derived the answer"], "content": [] }),
        );
        complete_item(&mut turn, json!({ "id": "message", "type": "agentMessage", "text": "The answer is 42." }));
        complete_turn(&mut turn);

        assert_eq!(turn.status, "completed");
        assert!(!turn.derailed_by_compaction);
        assert!(turn.build_report(ReportLevel::Brief).starts_with("The answer is 42."));
    }

    #[test]
    fn completion_of_pre_compaction_work_does_not_hide_the_amnesia_signature() {
        let mut turn = accumulator();
        turn.handle_notification(
            "item/started",
            &json!({ "item": { "id": "command", "type": "commandExecution", "command": "long-running-command" } }),
        );
        complete_item(&mut turn, json!({ "id": "compaction", "type": "contextCompaction" }));
        complete_item(
            &mut turn,
            json!({
                "id": "command",
                "type": "commandExecution",
                "command": "long-running-command",
                "status": "completed",
                "exitCode": 0
            }),
        );
        complete_item(&mut turn, json!({ "id": "message", "type": "agentMessage", "text": "Instructions loaded." }));
        complete_turn(&mut turn);

        assert_eq!(turn.status, "failed");
        assert!(turn.derailed_by_compaction);
    }

    #[test]
    fn only_the_final_compactions_subsequent_message_determines_derailment() {
        let mut turn = accumulator();
        complete_item(&mut turn, json!({ "id": "compaction-1", "type": "contextCompaction" }));
        complete_item(&mut turn, json!({ "id": "message-1", "type": "agentMessage", "text": "Instructions loaded." }));
        complete_item(&mut turn, json!({ "id": "compaction-2", "type": "contextCompaction" }));
        complete_item(
            &mut turn,
            json!({ "id": "message-2", "type": "agentMessage", "text": "Recovered and completed the task." }),
        );
        complete_turn(&mut turn);

        assert_eq!(turn.compactions, 2);
        assert_eq!(turn.status, "completed");
        assert!(!turn.derailed_by_compaction);
    }

    #[test]
    fn a_long_final_message_survives_every_report_level_full_included() {
        let mut turn = accumulator();
        let long = format!("{}END", "x".repeat(6000));
        complete_item(&mut turn, json!({ "id": "message", "type": "agentMessage", "text": long }));
        complete_turn(&mut turn);

        for level in [ReportLevel::Brief, ReportLevel::Items, ReportLevel::Full] {
            let report = turn.build_report(level);
            assert!(report.contains(&long), "{level:?} report dropped part of the final message");
            assert!(!report.contains('…'), "{level:?} report still carries a truncation marker");
        }
    }

    #[test]
    fn adopting_a_continuation_clears_the_derailed_verdict_from_the_compacted_half() {
        let mut turn = accumulator();
        complete_item(&mut turn, json!({ "id": "compaction", "type": "contextCompaction" }));
        complete_item(&mut turn, json!({ "id": "ack", "type": "agentMessage", "text": "Instructions loaded for `/repo`." }));
        complete_turn(&mut turn);
        assert_eq!(turn.status, "failed");

        turn.adopt_continuation("turn-2");
        complete_item(&mut turn, json!({ "id": "real", "type": "agentMessage", "text": "Actually finished the work." }));
        complete_turn(&mut turn);

        assert_eq!(turn.status, "completed");
        assert!(!turn.derailed_by_compaction);
        let report = turn.build_report(ReportLevel::Brief);
        assert!(report.contains("Actually finished the work."));
        assert!(report.contains("compactions: 1"));
    }

    #[test]
    fn a_turn_that_ends_without_an_agent_message_reports_what_it_did_instead() {
        let mut turn = TurnAccumulator::new("audit", "t1", "turn1", "");
        complete_item(
            &mut turn,
            json!({ "type": "commandExecution", "id": "c1", "command": "rg TODO", "status": "completed", "exitCode": 0 }),
        );
        complete_turn(&mut turn);

        let report = turn.build_report(ReportLevel::Brief);
        assert!(report.contains("no final message"));
        assert!(report.contains("rg TODO"));
    }

    #[test]
    fn a_reasoning_summary_streamed_as_deltas_and_repeated_by_the_completed_item_renders_once() {
        let mut turn = accumulator();
        turn.handle_notification(
            "item/reasoning/summaryTextDelta",
            &json!({ "itemId": "r1", "summaryIndex": 0, "delta": "Planning the fix" }),
        );
        complete_item(&mut turn, json!({ "id": "r1", "type": "reasoning", "summary": ["Planning the fix"], "content": [] }));
        complete_item(&mut turn, json!({ "id": "message", "type": "agentMessage", "text": "Done." }));
        complete_turn(&mut turn);

        assert_eq!(turn.build_report(ReportLevel::Items).matches("Planning the fix").count(), 1);
    }

    // --- Judgment calls made by the port ---

    #[test]
    fn reasoning_deltas_join_in_numeric_index_order() {
        // The JS sorted "r1:10" before "r1:2" as strings.
        let mut turn = accumulator();
        for (index, delta) in [(10, "tenth"), (2, "second"), (0, "zeroth")] {
            turn.handle_notification(
                "item/reasoning/summaryTextDelta",
                &json!({ "itemId": "r1", "summaryIndex": index, "delta": delta }),
            );
        }
        // An id that extends "r1" must not leak in (the JS matched by prefix).
        turn.handle_notification(
            "item/reasoning/summaryTextDelta",
            &json!({ "itemId": "r1:x", "summaryIndex": 0, "delta": "other item" }),
        );
        let lines = complete_item(&mut turn, json!({ "id": "r1", "type": "reasoning" }));
        assert_eq!(lines, ["[reasoning]\nzeroth\nsecond\ntenth"]);
    }

    #[test]
    fn truncation_counts_chars_and_never_splits_one() {
        assert_eq!(truncate("héllo", 5), "héllo");
        assert_eq!(truncate("héllo", 2), "hé…");
        assert_eq!(truncate("🦀🦀🦀", 1), "🦀…");
        assert_eq!(one_line("  a \n\t b\u{a0}c\u{feff} ", 200), "a b c");
        // U+0085 is not `\s` in JS.
        assert_eq!(one_line("a\u{85}b", 200), "a\u{85}b");
    }

    #[test]
    fn a_settled_run_keeps_its_duration_for_later_reports() {
        let mut turn = accumulator();
        turn.settle();
        std::thread::sleep(Duration::from_millis(150));
        assert!(turn.build_report(ReportLevel::Brief).contains("duration: 0.0s"));
    }

    #[test]
    fn log_lines_match_the_js_shapes() {
        let mut turn = accumulator();
        let started = turn.handle_notification(
            "item/started",
            &json!({ "item": { "id": "e1", "type": "fileChange", "changes": [{ "path": "b.rs" }, { "path": "a.rs" }] } }),
        );
        assert_eq!(started, ["[edit] b.rs, a.rs (started)"]);
        let edit = turn.handle_notification("item/fileChange/outputDelta", &json!({ "itemId": "e1", "delta": "+x" }));
        assert_eq!(edit, ["[edit] e1: +x"]);
        assert!(turn.build_report(ReportLevel::Full).contains("[edit] e1: +x"));

        turn.handle_notification("item/commandExecution/outputDelta", &json!({ "itemId": "c1", "delta": "streamed\n" }));
        let cmd = complete_item(
            &mut turn,
            json!({ "id": "c1", "type": "commandExecution", "command": "cargo\n  test", "exitCode": null }),
        );
        assert_eq!(cmd, ["[cmd] cargo test status=unknown exit=n/a\nstreamed\n"]);

        // Missing fields print empty, not "undefined".
        let mcp = complete_item(&mut turn, json!({ "id": "m1", "type": "mcpToolCall", "tool": "search" }));
        assert_eq!(mcp, ["[mcp] /search status=unknown"]);
        let agent = complete_item(
            &mut turn,
            json!({
                "id": "a1",
                "type": "collabAgentToolCall",
                "tool": "spawn",
                "receiverThreadIds": ["t2", "t3"],
                "status": "completed"
            }),
        );
        assert_eq!(agent, ["[agent] spawn t2,t3 status=completed"]);
        assert_eq!(complete_item(&mut turn, json!({ "id": "x1", "type": "novel" })), ["[item] novel x1"]);

        turn.handle_notification(
            "thread/tokenUsage/updated",
            &json!({ "tokenUsage": {
                "total": { "totalTokens": 900 },
                "last": { "totalTokens": 120, "inputTokens": 100, "outputTokens": 20 }
            } }),
        );
        turn.handle_notification(
            "turn/completed",
            &json!({ "turn": { "status": "failed", "error": {
                "message": "boom",
                "codexErrorInfo": { "httpConnectionFailed": { "httpStatusCode": 502 } }
            } } }),
        );
        let footer = turn.footer_line();
        assert!(
            footer.starts_with(
                "=== status=failed errorCode=httpConnectionFailed (httpStatusCode=502) tokens=120 total (100 in, 20 out, 0 reasoning) durationMs="
            ),
            "{footer}"
        );
        let report = turn.build_report(ReportLevel::Brief);
        let expected_head = [
            "boom",
            "",
            "status: failed",
            "error code: httpConnectionFailed (httpStatusCode=502)",
            "files touched: a.rs, b.rs",
            "tokens: 120 total (100 in, 20 out, 0 reasoning)",
            "duration: ",
        ]
        .join("\n");
        assert!(report.starts_with(&expected_head), "{report}");
    }

    #[test]
    fn header_line_carries_a_millisecond_utc_timestamp() {
        let mut turn = TurnAccumulator::new("alpha", "th", "tu", "first   line\r\nsecond line");
        turn.started_at = "2026-10-08T12:34:56.789999Z".parse().unwrap();
        assert_eq!(turn.header_line(), "\n=== 2026-10-08T12:34:56.789Z label=alpha thread=th turn=tu prompt=first line ===");
        turn.started_at = "2026-10-08T12:34:56Z".parse().unwrap();
        assert!(turn.header_line().starts_with("\n=== 2026-10-08T12:34:56.000Z label=alpha"));
    }

    #[test]
    fn durations_round_like_js_to_fixed() {
        let ms = Duration::from_millis;
        assert_eq!(format_duration(ms(0)), "0.0s");
        // Exact binary ties round up in toFixed, unlike Rust's `{:.1}`.
        assert_eq!(format_duration(ms(250)), "0.3s");
        assert_eq!(format_duration(ms(1250)), "1.3s");
        // 0.15 is stored as 0.1499…, so toFixed(1) gives "0.1".
        assert_eq!(format_duration(ms(150)), "0.1s");
        assert_eq!(format_duration(ms(59_499)), "59.5s");
        assert_eq!(format_duration(ms(59_500)), "1m 0s");
        assert_eq!(format_duration(ms(125_400)), "2m 5s");
    }

    #[test]
    fn goal_lines() {
        assert_eq!(format_goal(None, None), "no goal set");
        assert_eq!(format_goal(Some(&Value::Null), Some("x")), "no goal set");
        let active = json!({ "status": "active", "objective": "ship\nit" });
        assert_eq!(format_goal(Some(&active), Some("audit")), "goal: active — ship it");
        let blocked = json!({ "status": "blocked", "objective": "ship it" });
        assert_eq!(
            format_goal(Some(&blocked), Some("audit")),
            "goal: blocked — ship it\nthe goal stopped before completing; restart the run with `ception goal audit --resume`"
        );
        assert!(format_goal(Some(&blocked), None).ends_with("`ception goal <label> --resume`"));
    }

    #[test]
    fn token_usage_and_exit_codes() {
        assert_eq!(format_token_usage(&Value::Null), "n/a");
        assert_eq!(format_token_usage(&json!({ "last": null, "total": { "totalTokens": 7.0 } })), "7 total (0 in, 0 out, 0 reasoning)");
        assert_eq!(format_token_usage(&json!({ "totalTokens": "7" })), "n/a");
        assert_eq!(status_exit_code("idle"), 0);
        assert_eq!(status_exit_code("failed"), 2);
        assert_eq!(status_exit_code("interrupted"), 3);
        assert_eq!(status_exit_code("inProgress"), 4);
    }
}
