//! Test fixture: a scriptable stand-in for `codex app-server`, speaking
//! newline-delimited JSON-RPC on stdio. Port of test/fake-appserver.mjs.
//!
//! Env knobs:
//! - `CEPTION_FAKE_STATE`: JSON file recording starts, threads, requests, turn
//!   starts, steers and interrupts; tests read it. Default
//!   `./fake-appserver-state.json`.
//! - `CEPTION_FAKE_BEHAVIOR`: which scripted shape to play, default "happy".
//!   See `turn_start`, `turn_start_special` and `goal_set` for the branches.
//! - `CEPTION_FAKE_CONTINUATION_DELAY_MS` (150), `CEPTION_FAKE_CONTINUATION_RUN_MS`
//!   (0), `CEPTION_FAKE_GOAL_TURN_DELAY_MS` (100): timing of the turns the
//!   server starts by itself.
//!
//! Concurrency mirrors the JS event loop: every request handler and every timer
//! callback runs start to finish under one lock (`server()`), so they never
//! interleave and neither do their stdout writes.

use std::env;
use std::fs;
use std::io::{self, BufRead, ErrorKind, Write};
use std::panic;
use std::path::PathBuf;
use std::process::{self, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

static STATE_PATH: LazyLock<PathBuf> = LazyLock::new(|| match env::var_os("CEPTION_FAKE_STATE") {
    Some(path) => PathBuf::from(path),
    None => env::current_dir()
        .unwrap()
        .join("fake-appserver-state.json"),
});
static BEHAVIOR: LazyLock<String> =
    LazyLock::new(|| env::var("CEPTION_FAKE_BEHAVIOR").unwrap_or_else(|_| "happy".into()));

/// JS `Number(process.env[name] ?? fallback)` used as a timer delay: unset means
/// the default; anything unparsable acts as 0, like NaN does in `setTimeout`.
fn env_ms(name: &str, default: u64) -> u64 {
    env::var(name).map_or(default, |value| value.trim().parse().unwrap_or(0))
}

// ---------------------------------------------------------------------------
// On-disk state

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct State {
    app_server_starts: u64,
    next_thread: u64,
    next_turn: u64,
    threads: Vec<Thread>,
    requests: Vec<Value>,
    last_turn_starts: Vec<Value>,
    steers: Vec<Value>,
    interrupts: Vec<Value>,
    rejected_requests: u64,
    /// Pids of long-lived processes the fake left in its process group.
    #[serde(default)]
    children: Vec<u32>,
    /// Points in time tests order against, e.g. `{ "mark": "turn/start
    /// replied", "requests": <requests recorded by then> }`.
    #[serde(default)]
    marks: Vec<Value>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Thread {
    id: String,
    cwd: Value,
    created_at: u64,
}

fn load_state() -> State {
    match fs::read_to_string(&*STATE_PATH) {
        Ok(text) => serde_json::from_str(&text).expect("parse fake state"),
        Err(err) if err.kind() == ErrorKind::NotFound => State {
            app_server_starts: 0,
            next_thread: 1,
            next_turn: 1,
            threads: vec![],
            requests: vec![],
            last_turn_starts: vec![],
            steers: vec![],
            interrupts: vec![],
            rejected_requests: 0,
            children: vec![],
            marks: vec![],
        },
        Err(err) => panic!("read fake state: {err}"),
    }
}

fn save_state(state: &State) {
    // Atomic rename: tests (and sibling fake instances) poll this file and must
    // never see a partial write. pid-suffixed so concurrent fakes don't collide.
    let mut temp = STATE_PATH.clone().into_os_string();
    temp.push(format!(".{}.tmp", process::id()));
    let text = serde_json::to_string_pretty(state).unwrap() + "\n";
    fs::write(&temp, text).expect("write fake state");
    fs::rename(&temp, &*STATE_PATH).expect("rename fake state");
}

/// Every write goes through here. The rename alone keeps readers from seeing a
/// partial file; the exclusive flock on `${STATE_PATH}.lock` also keeps sibling
/// fakes sharing the file (several daemons in one test) from dropping each
/// other's read-modify-write updates.
fn update_state<T>(f: impl FnOnce(&mut State) -> T) -> T {
    if let Some(dir) = STATE_PATH.parent() {
        fs::create_dir_all(dir).expect("create fake state dir");
    }
    let mut lock_path = STATE_PATH.clone().into_os_string();
    lock_path.push(".lock");
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .expect("open fake state lock");
    lock.lock().expect("lock fake state");
    let mut state = load_state();
    let result = f(&mut state);
    save_state(&state);
    drop(lock); // closing the file releases the flock
    result
}

fn create_thread(cwd: Value) -> Thread {
    update_state(|state| {
        let thread = Thread {
            id: format!("thr_{}", state.next_thread),
            cwd,
            created_at: now_seconds(),
        };
        state.next_thread += 1;
        state.threads.push(thread.clone());
        thread
    })
}

fn find_thread(thread_id: &Value) -> Result<Thread, String> {
    let state = load_state();
    let found = state
        .threads
        .into_iter()
        .find(|candidate| *thread_id == candidate.id.as_str());
    found.ok_or_else(|| match thread_id.as_str() {
        Some(id) => format!("unknown thread {id}"),
        None => format!("unknown thread {thread_id}"),
    })
}

fn next_turn() -> String {
    update_state(|state| {
        let turn_id = format!("turn_{}", state.next_turn);
        state.next_turn += 1;
        turn_id
    })
}

// ---------------------------------------------------------------------------
// Message builders

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn now_seconds() -> u64 {
    now_ms() / 1000
}

fn cwd() -> String {
    env::current_dir().unwrap().display().to_string()
}

/// JS `value ?? fallback`: absent and null both fall through.
fn given(value: &Value) -> Option<Value> {
    (!value.is_null()).then(|| value.clone())
}

fn prompt_text(input: &Value) -> String {
    let items = input.as_array().map(Vec::as_slice).unwrap_or_default();
    let texts: Vec<&str> = items
        .iter()
        .filter(|item| item["type"] == "text")
        .map(|item| item["text"].as_str().unwrap_or_default())
        .collect();
    texts.join("\n")
}

/// The response to `request`. As with JSON.stringify of `{ id: message.id }`,
/// a notification's absent id stays absent.
fn respond(request: &Value, key: &str, body: Value) -> Value {
    let mut response = serde_json::Map::new();
    if let Some(id) = request.get("id") {
        response.insert("id".into(), id.clone());
    }
    response.insert(key.into(), body);
    Value::Object(response)
}

fn reply(request: &Value, result: Value) -> Value {
    respond(request, "result", result)
}

fn reply_error(request: &Value, code: i64, message: &str) -> Value {
    respond(
        request,
        "error",
        json!({ "code": code, "message": message }),
    )
}

fn build_thread(thread: &Thread) -> Value {
    json!({
        "id": thread.id,
        "sessionId": thread.id,
        "forkedFromId": null,
        "parentThreadId": null,
        "preview": "",
        "ephemeral": false,
        "modelProvider": "openai",
        "createdAt": thread.created_at,
        "updatedAt": now_seconds(),
        "recencyAt": now_seconds(),
        "status": { "type": "idle" },
        "path": null,
        "cwd": thread.cwd,
        "cliVersion": "fake",
        "source": "appServer",
        "threadSource": null,
        "agentNickname": null,
        "agentRole": null,
        "gitInfo": null,
        "name": null,
        "turns": []
    })
}

fn build_turn(id: &str, status: &str, error: Value) -> Value {
    let done = status != "inProgress";
    json!({
        "id": id,
        "items": [],
        "itemsView": "full",
        "status": status,
        "error": error,
        "startedAt": now_seconds(),
        "completedAt": if done { json!(now_seconds()) } else { Value::Null },
        "durationMs": if done { json!(25) } else { Value::Null }
    })
}

fn token_usage() -> Value {
    let breakdown = json!({
        "totalTokens": 42,
        "inputTokens": 20,
        "cachedInputTokens": 0,
        "outputTokens": 22,
        "reasoningOutputTokens": 5
    });
    json!({ "total": breakdown, "last": breakdown, "modelContextWindow": 200000 })
}

fn turn_started(thread_id: &str, turn_id: &str) -> Value {
    json!({
        "method": "turn/started",
        "params": { "threadId": thread_id, "turn": build_turn(turn_id, "inProgress", Value::Null) }
    })
}

fn turn_completed(thread_id: &str, turn_id: &str, status: &str, error: Value) -> Value {
    json!({
        "method": "turn/completed",
        "params": { "threadId": thread_id, "turn": build_turn(turn_id, status, error) }
    })
}

fn item_completed(thread_id: &str, turn_id: &str, item: Value) -> Value {
    json!({
        "method": "item/completed",
        "params": { "threadId": thread_id, "turnId": turn_id, "completedAtMs": now_ms(), "item": item }
    })
}

fn agent_message(thread_id: &str, turn_id: &str, text: &str) -> Value {
    let item = json!({
        "type": "agentMessage",
        "id": format!("msg_{turn_id}"),
        "text": text,
        "phase": "final_answer",
        "memoryCitation": null
    });
    item_completed(thread_id, turn_id, item)
}

fn reasoning_started(thread_id: &str, turn_id: &str) -> Value {
    json!({
        "method": "item/started",
        "params": {
            "threadId": thread_id,
            "turnId": turn_id,
            "startedAtMs": now_ms(),
            "item": { "type": "reasoning", "id": format!("reason_{turn_id}"), "summary": [], "content": [] }
        }
    })
}

fn reasoning_delta(thread_id: &str, turn_id: &str, delta: &str) -> Value {
    json!({
        "method": "item/reasoning/summaryTextDelta",
        "params": {
            "threadId": thread_id,
            "turnId": turn_id,
            "itemId": format!("reason_{turn_id}"),
            "summaryIndex": 0,
            "delta": delta
        }
    })
}

fn token_usage_updated(thread_id: &str, turn_id: &str) -> Value {
    json!({
        "method": "thread/tokenUsage/updated",
        "params": { "threadId": thread_id, "turnId": turn_id, "tokenUsage": token_usage() }
    })
}

fn goal_updated(thread_id: &str, goal: Value) -> Value {
    json!({ "method": "thread/goal/updated", "params": { "threadId": thread_id, "goal": goal } })
}

/// The turn error a server-side safeguard ends a goal's turn with.
fn policy_stop() -> Value {
    json!({
        "message": "Turn stopped by a server-side content policy check.",
        "codexErrorInfo": "policyStop",
        "additionalDetails": null
    })
}

/// The goal `goal-continuation` plays out, independent of `thread/goal/set`.
fn fixture_goal(thread_id: &str, status: &str) -> Value {
    json!({
        "threadId": thread_id,
        "objective": "Finish the fixture work",
        "status": status,
        "tokenBudget": null,
        "tokensUsed": 100,
        "timeUsedSeconds": 1,
        "createdAt": 0,
        "updatedAt": 0
    })
}

/// Shaped like codex's GetAccountRateLimitsResponse. As on the real server, the
/// account snapshot repeats in the by-id map under its own limit id, so
/// `ception quota` has a duplicate to skip; "codex_spark" is one extra named
/// limit. Reset times are relative to now so "resets in 3h" stays stable.
fn rate_limits() -> Value {
    let now = now_seconds();
    let window = |used: u64, mins: u64, resets_in_mins: u64| json!({ "usedPercent": used, "windowDurationMins": mins, "resetsAt": now + resets_in_mins * 60 });
    let snapshot =
        |limit_id: &str, limit_name: Value, primary: Value, secondary: Value, credits: Value| {
            json!({
                "limitId": limit_id,
                "limitName": limit_name,
                "normalModelSlug": null,
                "primary": primary,
                "secondary": secondary,
                "credits": credits,
                "individualLimit": null,
                "spendControlReached": false,
                "planType": "pro",
                "rateLimitReachedType": null
            })
        };
    let credits = json!({ "hasCredits": true, "unlimited": false, "balance": "12.50" });
    let account = snapshot(
        "codex",
        Value::Null,
        window(12, 300, 180),
        window(47, 10080, 3000),
        credits,
    );
    let spark = snapshot(
        "codex_spark",
        json!("Spark"),
        window(3, 300, 240),
        Value::Null,
        Value::Null,
    );
    json!({
        "rateLimits": account,
        "rateLimitsByLimitId": { "codex": account, "codex_spark": spark }
    })
}

// ---------------------------------------------------------------------------
// Server

/// A turn the fake is running (or has parked open).
#[derive(Clone)]
struct Turn {
    thread_id: String,
    turn_id: String,
    prompt: String,
}

impl Turn {
    /// A turn the server starts by itself, with no prompt behind it. Takes the
    /// next turn id from the state file.
    fn unprompted(thread_id: &str) -> Turn {
        Turn {
            thread_id: thread_id.into(),
            turn_id: next_turn(),
            prompt: String::new(),
        }
    }
}

struct Goal {
    objective: Value,
    status: Value,
}

struct Server {
    active_turn: Option<Turn>,
    request_id: u64,
    // Goals live in the app-server for real; the fake keeps them per process,
    // which is enough because one daemon owns one app-server and one thread.
    goal: Option<Goal>,
    goal_turns: u32,
    /// Set inside `with_batch`: messages collect here instead of going out.
    batch: Option<Vec<Value>>,
}

static SERVER: Mutex<Server> = Mutex::new(Server {
    active_turn: None,
    request_id: 9000,
    goal: None,
    goal_turns: 0,
    batch: None,
});

fn server() -> MutexGuard<'static, Server> {
    SERVER.lock().unwrap()
}

/// Only called with the server lock held, which is what keeps lines whole.
fn write_out(text: &str) {
    let mut out = io::stdout().lock();
    out.write_all(text.as_bytes())
        .and_then(|()| out.flush())
        .expect("write stdout");
}

static TIMERS: Mutex<Vec<JoinHandle<()>>> = Mutex::new(Vec::new());

/// Set by "deaf": the stdin reader stops for good, so the client's writes back
/// up once the pipe buffer is full.
static DEAF: AtomicBool = AtomicBool::new(false);

/// A long-lived process in the fake's process group, the way codex's shells
/// and tools are. Never reaped by the fake; recorded for tests to check on.
fn spawn_descendant(script: &str, stdout: Stdio) {
    let child = process::Command::new("sh")
        .args(["-c", script])
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn descendant");
    update_state(|state| state.children.push(child.id()));
}

/// JS `setTimeout`: run `f` after `ms` on its own thread, under the server lock
/// like every other callback.
fn set_timeout(ms: u64, f: impl FnOnce(&mut Server) + Send + 'static) {
    let handle = thread::spawn(move || {
        thread::sleep(Duration::from_millis(ms));
        f(&mut server());
    });
    let mut timers = TIMERS.lock().unwrap();
    timers.retain(|timer| !timer.is_finished());
    timers.push(handle);
}

/// Node stays alive after stdin closes until pending timers have fired; so does
/// the fake. Timers may schedule more timers, hence the loop.
fn wait_for_timers() {
    loop {
        let next = TIMERS.lock().unwrap().pop();
        let Some(timer) = next else { break };
        let _ = timer.join();
    }
}

impl Server {
    fn send(&mut self, message: Value) {
        match &mut self.batch {
            Some(batch) => batch.push(message),
            None => write_out(&format!("{message}\n")),
        }
    }

    // One write, so the reader gets every line in a single chunk and dispatches
    // them all before any of its own awaits resume. The real server can do this
    // whenever it answers and then immediately runs and ends a turn.
    fn with_batch(&mut self, f: impl FnOnce(&mut Self)) {
        self.batch = Some(Vec::new());
        f(self);
        let messages = self.batch.take().unwrap();
        let lines: Vec<String> = messages.iter().map(Value::to_string).collect();
        write_out(&format!("{}\n", lines.join("\n")));
    }

    fn complete_turn(&mut self, turn: &Turn, status: &str, message: Option<&str>) {
        let Turn {
            thread_id,
            turn_id,
            prompt,
        } = turn;
        let final_text = match message {
            Some(text) => text.to_string(),
            None if prompt.contains("follow") => {
                "Resumed the prior run.\nFollow-up prompt accepted.".into()
            }
            None => format!("Handled the requested task.\nPrompt: {prompt}"),
        };
        self.send(turn_started(thread_id, turn_id));
        self.send(reasoning_started(thread_id, turn_id));
        self.send(reasoning_delta(
            thread_id,
            turn_id,
            "Thinking through fixture.",
        ));
        self.send(item_completed(
            thread_id,
            turn_id,
            json!({
                "type": "reasoning",
                "id": format!("reason_{turn_id}"),
                "summary": ["Thinking through fixture."],
                "content": []
            }),
        ));
        self.send(item_completed(
            thread_id,
            turn_id,
            json!({
                "type": "fileChange",
                "id": format!("edit_{turn_id}"),
                "changes": [{ "path": "src/example.js", "kind": { "type": "update", "move_path": null }, "diff": "@@" }],
                "status": "completed"
            }),
        ));
        self.send(json!({
            "method": "item/agentMessage/delta",
            "params": { "threadId": thread_id, "turnId": turn_id, "itemId": format!("msg_{turn_id}"), "delta": final_text }
        }));
        self.send(agent_message(thread_id, turn_id, &final_text));
        self.send(token_usage_updated(thread_id, turn_id));
        let error = if status == "failed" {
            json!({ "message": "fixture failure", "codexErrorInfo": null, "additionalDetails": null })
        } else {
            Value::Null
        };
        self.send(turn_completed(thread_id, turn_id, status, error));
        self.active_turn = None;
    }

    fn start_long_turn(&mut self, turn: Turn) {
        self.send(turn_started(&turn.thread_id, &turn.turn_id));
        self.send(reasoning_started(&turn.thread_id, &turn.turn_id));
        self.send(reasoning_delta(
            &turn.thread_id,
            &turn.turn_id,
            "Waiting for steering.",
        ));
        self.active_turn = Some(turn);
    }

    // Reproduces the observed compaction shape: codex closes the turn with a bare
    // instruction acknowledgement, then continues the real work in a fresh turn.
    fn start_compacted_turn(&mut self, turn: &Turn) {
        let Turn {
            thread_id, turn_id, ..
        } = turn;
        self.send(turn_started(thread_id, turn_id));
        self.send(item_completed(
            thread_id,
            turn_id,
            json!({ "type": "contextCompaction", "id": format!("compact_{turn_id}") }),
        ));
        self.send(agent_message(
            thread_id,
            turn_id,
            "Instructions loaded for `/repo`.",
        ));
        self.send(turn_completed(thread_id, turn_id, "completed", Value::Null));

        fn finish(s: &mut Server, turn: &Turn) {
            let Turn {
                thread_id, turn_id, ..
            } = turn;
            s.send(agent_message(
                thread_id,
                turn_id,
                "Continued past the compaction and finished the work.",
            ));
            s.send(token_usage_updated(thread_id, turn_id));
            s.send(turn_completed(thread_id, turn_id, "completed", Value::Null));
            s.active_turn = None;
        }
        let continuation = Turn::unprompted(thread_id);
        set_timeout(
            env_ms("CEPTION_FAKE_CONTINUATION_DELAY_MS", 150),
            move |s| {
                s.active_turn = Some(continuation.clone());
                s.send(turn_started(&continuation.thread_id, &continuation.turn_id));
                // Optionally keep the continuation running so tests can observe it live.
                let run_ms = env_ms("CEPTION_FAKE_CONTINUATION_RUN_MS", 0);
                if run_ms > 0 {
                    set_timeout(run_ms, move |s| finish(s, &continuation));
                } else {
                    finish(s, &continuation);
                }
            },
        );
    }

    // The real cross-turn mechanism: an active thread goal makes codex start a
    // follow-on turn by itself once the thread goes idle.
    fn start_goal_turn(&mut self, turn: &Turn) {
        let Turn {
            thread_id, turn_id, ..
        } = turn;
        self.send(turn_started(thread_id, turn_id));
        self.send(json!({
            "method": "thread/goal/updated",
            "params": { "threadId": thread_id, "turnId": turn_id, "goal": fixture_goal(thread_id, "active") }
        }));
        self.send(agent_message(thread_id, turn_id, "First half done."));
        self.send(turn_completed(thread_id, turn_id, "completed", Value::Null));
        // A subagent's own thread finishing its own goal, on this same connection,
        // while our run is mid-hold. Nothing about it concerns this thread.
        self.send(json!({
            "method": "thread/goal/updated",
            "params": { "threadId": "thr_subagent", "turnId": null, "goal": fixture_goal("thr_subagent", "complete") }
        }));

        let continuation = Turn::unprompted(thread_id);
        set_timeout(
            env_ms("CEPTION_FAKE_CONTINUATION_DELAY_MS", 150),
            move |s| {
                let Turn {
                    thread_id, turn_id, ..
                } = &continuation;
                s.active_turn = Some(continuation.clone());
                s.send(turn_started(thread_id, turn_id));
                s.send(agent_message(
                    thread_id,
                    turn_id,
                    "Goal continuation finished the work.",
                ));
                s.send(token_usage_updated(thread_id, turn_id));
                s.send(json!({
                    "method": "thread/goal/updated",
                    "params": { "threadId": thread_id, "turnId": turn_id, "goal": fixture_goal(thread_id, "complete") }
                }));
                s.send(turn_completed(thread_id, turn_id, "completed", Value::Null));
                s.active_turn = None;
            },
        );
    }

    /// The goal set through `thread/goal/set`; callers make sure there is one.
    fn build_goal(&self, thread_id: &str) -> Value {
        let goal = self.goal.as_ref().expect("no goal set");
        json!({
            "threadId": thread_id,
            "objective": goal.objective,
            "status": goal.status,
            "tokenBudget": null,
            "tokensUsed": 250,
            "timeUsedSeconds": 3,
            "createdAt": 0,
            "updatedAt": now_seconds()
        })
    }

    fn send_goal_updated(&mut self, thread_id: &str) {
        let goal = self.build_goal(thread_id);
        self.send(goal_updated(thread_id, goal));
    }

    fn set_goal_status(&mut self, thread_id: &str, status: &str) {
        // Timers call this; the client may have cleared the goal meanwhile.
        // (The JS fake crashed here.)
        let Some(goal) = self.goal.as_mut() else {
            return;
        };
        goal.status = json!(status);
        self.send_goal_updated(thread_id);
    }

    fn goal_is_active(&self) -> bool {
        self.goal
            .as_ref()
            .is_some_and(|goal| goal.status == "active")
    }

    // What an active goal actually does: codex starts its own turn on the idle
    // thread. The "goal-stopped" behaviour has the server end that first turn with
    // a policy error, which is the shape a server-side safeguard produces — the
    // turn fails and the goal goes to `blocked` until someone resumes it.
    fn start_goal_turn_from_goal(&mut self, thread_id: &str) {
        let turn = Turn::unprompted(thread_id);
        self.active_turn = Some(turn.clone());
        let stopping =
            matches!(BEHAVIOR.as_str(), "goal-stopped" | "goal-instant") && self.goal_turns == 0;
        self.goal_turns += 1;
        if *BEHAVIOR == "steer" {
            // Park the goal's turn open so a test can steer it mid-flight.
            self.start_long_turn(turn);
            return;
        }
        let turn_id = &turn.turn_id;
        self.send(turn_started(thread_id, turn_id));
        let text = if stopping {
            "Working on the objective."
        } else {
            "Objective met; work finished."
        };
        self.send(agent_message(thread_id, turn_id, text));
        self.send(token_usage_updated(thread_id, turn_id));
        if stopping {
            self.send(turn_completed(thread_id, turn_id, "failed", policy_stop()));
            self.active_turn = None;
            self.set_goal_status(thread_id, "blocked");
            return;
        }
        self.send(turn_completed(thread_id, turn_id, "completed", Value::Null));
        self.active_turn = None;
        self.set_goal_status(thread_id, "complete");
    }

    /// The fresh turn a goal starts after the running one ended, meeting the
    /// objective (`goal-late-active`, `goal-during-turn`).
    fn run_goal_continuation(&mut self, thread_id: &str, text: &str) {
        let continuation = Turn::unprompted(thread_id);
        let turn_id = &continuation.turn_id;
        self.active_turn = Some(continuation.clone());
        self.send(turn_started(thread_id, turn_id));
        self.send(agent_message(thread_id, turn_id, text));
        self.send(token_usage_updated(thread_id, turn_id));
        self.set_goal_status(thread_id, "complete");
        self.send(turn_completed(thread_id, turn_id, "completed", Value::Null));
        self.active_turn = None;
    }

    fn send_unsupported_request(&mut self, turn: &Turn) {
        let id = self.request_id;
        self.request_id += 1;
        self.send(json!({
            "id": id,
            "method": "item/commandExecution/requestApproval",
            "params": {
                "threadId": turn.thread_id,
                "turnId": turn.turn_id,
                "itemId": format!("cmd_{}", turn.turn_id),
                "command": "echo no",
                "cwd": cwd(),
                "reason": "fixture"
            }
        }));
    }

    // -----------------------------------------------------------------------
    // Requests

    fn dispatch(&mut self, message: &Value) {
        let has_method = message["method"]
            .as_str()
            .is_some_and(|method| !method.is_empty());
        if message.get("id").is_some() && !has_method {
            if message["error"]["code"] == -32601 {
                update_state(|state| state.rejected_requests += 1);
            }
            return;
        }

        update_state(|state| {
            state
                .requests
                .push(json!({ "method": message["method"], "params": message["params"] }));
        });

        if let Err(err) = self.handle(message) {
            self.send(reply_error(message, -32000, &err));
        }
    }

    fn handle(&mut self, message: &Value) -> Result<(), String> {
        let params = &message["params"];
        match message["method"].as_str().unwrap_or_default() {
            "initialize" => self.send(reply(
                message,
                json!({ "userAgent": "fake", "codexHome": "/tmp/fake", "platformFamily": "unix", "platformOs": "linux" }),
            )),

            "initialized" => {}

            "thread/start" => {
                let thread = create_thread(given(&params["cwd"]).unwrap_or_else(|| json!(cwd())));
                self.send(reply(message, json!({ "thread": build_thread(&thread) })));
                self.send(json!({ "method": "thread/started", "params": { "thread": build_thread(&thread) } }));
            }

            "thread/resume" => {
                let thread = find_thread(&params["threadId"])?;
                self.send(reply(message, json!({ "thread": build_thread(&thread) })));
            }

            "turn/start" => self.turn_start(message)?,

            "thread/goal/set" => self.goal_set(message)?,

            "thread/goal/get" => {
                let thread_id = params["threadId"].as_str().unwrap_or_default();
                let goal = if self.goal.is_some() { self.build_goal(thread_id) } else { Value::Null };
                self.send(reply(message, json!({ "goal": goal })));
            }

            "thread/goal/clear" => {
                let cleared = self.goal.take().is_some();
                self.send(reply(message, json!({ "cleared": cleared })));
                self.send(json!({ "method": "thread/goal/cleared", "params": { "threadId": params["threadId"] } }));
            }

            "turn/steer" => self.turn_steer(message),

            "turn/interrupt" => {
                update_state(|state| state.interrupts.push(params.clone()));
                if *BEHAVIOR == "interrupt-reject" {
                    self.send(reply_error(message, -32000, "turn no longer active"));
                    return Ok(());
                }
                self.send(reply(message, json!({})));
                if let Some(active) = self.active_turn.clone() {
                    self.complete_turn(&active, "interrupted", Some("Interrupted by fixture."));
                }
            }

            "account/rateLimits/read" => self.send(reply(message, rate_limits())),

            method => self.send(reply_error(message, -32601, &format!("unsupported {method}"))),
        }
        Ok(())
    }

    fn turn_start(&mut self, message: &Value) -> Result<(), String> {
        let params = &message["params"];
        let thread = find_thread(&params["threadId"])?;
        let turn = Turn {
            thread_id: thread.id,
            turn_id: next_turn(),
            prompt: prompt_text(&params["input"]),
        };
        update_state(|state| {
            state.last_turn_starts.push(json!({
                "threadId": turn.thread_id,
                "turnId": turn.turn_id,
                "prompt": turn.prompt,
                "model": params["model"],
                "effort": params["effort"],
                "sandboxPolicy": params["sandboxPolicy"]
            }));
        });
        let Some(turn) = self.turn_start_special(message, turn) else {
            return Ok(());
        };
        self.send(reply(
            message,
            json!({ "turn": build_turn(&turn.turn_id, "inProgress", Value::Null) }),
        ));

        let behavior = BEHAVIOR.as_str();
        if behavior == "stubborn-child" {
            // A shell that shrugs off SIGTERM, like a tool codex started.
            spawn_descendant("trap '' TERM; sleep 1000", Stdio::null());
            self.complete_turn(&turn, "completed", None);
        } else if behavior == "steer" || turn.prompt.contains("slow") {
            self.start_long_turn(turn);
        } else if behavior == "continuation" {
            self.start_compacted_turn(&turn);
        } else if behavior == "goal-continuation" {
            self.start_goal_turn(&turn);
        } else if behavior == "fail" {
            self.complete_turn(&turn, "failed", Some("Partial output before failure."));
        } else if behavior == "server-request" {
            self.send(turn_started(&turn.thread_id, &turn.turn_id));
            self.send_unsupported_request(&turn);
            set_timeout(100, move |s| s.complete_turn(&turn, "completed", None));
        } else {
            self.complete_turn(&turn, "completed", None);
        }
        Ok(())
    }

    /// Turn shapes that break the usual reply-first order or end the server.
    /// Hands the turn back when the behavior is not one of them.
    fn turn_start_special(&mut self, message: &Value, turn: Turn) -> Option<Turn> {
        let started = |turn: &Turn| json!({ "turn": build_turn(&turn.turn_id, "inProgress", Value::Null) });
        match BEHAVIOR.as_str() {
            // Answers and starts the turn, then never reads stdin again.
            "deaf" => {
                self.send(reply(message, started(&turn)));
                self.start_long_turn(turn);
                DEAF.store(true, Ordering::SeqCst);
            }
            // The turn compacts, ends, and continues in turn B before the
            // turn/start reply (naming the first turn) goes out; B's answer and
            // completion come after it.
            "late-start-reply" => {
                let Turn { thread_id, turn_id, .. } = &turn;
                let continuation = Turn::unprompted(thread_id);
                let next = &continuation.turn_id;
                self.with_batch(|s| {
                    s.send(turn_started(thread_id, turn_id));
                    let compaction = json!({ "type": "contextCompaction", "id": format!("compact_{turn_id}") });
                    s.send(item_completed(thread_id, turn_id, compaction));
                    s.send(turn_completed(thread_id, turn_id, "completed", Value::Null));
                    s.send(turn_started(thread_id, next));
                    s.send(reply(message, started(&turn)));
                    s.send(agent_message(thread_id, next, "Continuation turn finished the work."));
                    s.send(turn_completed(thread_id, next, "completed", Value::Null));
                });
            }
            // Starts the turn, leaves a process holding stdout, and exits 17.
            "exit-with-child" => {
                self.send(reply(message, started(&turn)));
                self.send(turn_started(&turn.thread_id, &turn.turn_id));
                spawn_descendant("exec sleep 1000", Stdio::inherit());
                process::exit(17);
            }
            // Dies mid-turn with a last word on stderr.
            "crash" => {
                self.send(reply(message, started(&turn)));
                self.send(turn_started(&turn.thread_id, &turn.turn_id));
                eprintln!("fake: rollout file corrupt (distinctive-crash-7731)");
                process::exit(3);
            }
            // The reply comes a second late; the turn then stays open.
            "slow-turn-start" => {
                let request = message.clone();
                set_timeout(1000, move |s| {
                    update_state(|state| {
                        let requests = state.requests.len();
                        state.marks.push(json!({ "mark": "turn/start replied", "requests": requests }));
                    });
                    s.send(reply(&request, started(&turn)));
                    s.start_long_turn(turn);
                });
            }
            _ => return Some(turn),
        }
        None
    }

    fn goal_set(&mut self, message: &Value) -> Result<(), String> {
        let params = &message["params"];
        let thread_id = find_thread(&params["threadId"])?.id;
        let previous_objective = self.goal.take().map(|goal| goal.objective);
        self.goal = Some(Goal {
            objective: given(&params["objective"])
                .or(previous_objective)
                .unwrap_or_else(|| json!("")),
            status: given(&params["status"]).unwrap_or_else(|| json!("active")),
        });
        // The running turn, if any, addressed on this request's thread.
        let parked = self.active_turn.clone().map(|turn| Turn {
            thread_id: thread_id.clone(),
            ..turn
        });

        match (BEHAVIOR.as_str(), parked) {
            // The goal is accepted and never starts a turn by itself.
            ("slow-turn-start", _) => self.answer_goal(message, &thread_id),

            // Ends the running turn and rejects the goal request in one write.
            ("goal-set-error", parked) => {
                self.goal = None;
                self.with_batch(|s| {
                    if let Some(parked) = &parked {
                        s.complete_turn(parked, "completed", None);
                    }
                    s.send(reply_error(message, -32000, "goal rejected by fixture"));
                });
            }

            // The goal is accepted, then stops without ever starting a turn.
            ("goal-stalls", None) => {
                self.answer_goal(message, &thread_id);
                set_timeout(60, move |s| s.set_goal_status(&thread_id, "usageLimited"));
            }

            // The running turn ends between the reply and the goal taking effect;
            // only then does the goal produce its own turn.
            ("goal-late-active", Some(parked)) => {
                self.with_batch(|s| {
                    let goal = s.build_goal(&thread_id);
                    s.send(reply(message, json!({ "goal": goal })));
                    s.complete_turn(&parked, "completed", Some("Parked turn finished."));
                    s.send_goal_updated(&thread_id);
                });
                set_timeout(60, move |s| {
                    s.run_goal_continuation(&thread_id, "Continuation after the late goal.")
                });
            }

            // The running turn absorbs the goal and meets the objective itself,
            // with no continuation at all.
            ("goal-in-running-turn", Some(parked)) => {
                self.answer_goal(message, &thread_id);
                set_timeout(50, move |s| {
                    s.set_goal_status(&parked.thread_id, "complete");
                    s.complete_turn(
                        &parked,
                        "completed",
                        Some("Objective met inside the running turn."),
                    );
                });
            }

            // The running turn ends and the objective continues in a fresh turn —
            // the one the goal client is actually waiting for.
            ("goal-during-turn", Some(parked)) => {
                self.answer_goal(message, &thread_id);
                set_timeout(50, move |s| {
                    s.complete_turn(&parked, "completed", Some("Parked turn finished."));
                    set_timeout(50, move |s| {
                        s.run_goal_continuation(&parked.thread_id, "Goal turn finished the work.")
                    });
                });
            }

            // Codex snapshots the goal, persists it, then answers. In between, the
            // goal can start a turn and that turn can fail, so the reply arrives
            // last, carrying the stale (active) snapshot. Plain: one write.
            // "-split": each message its own write, 20ms apart.
            (behavior @ ("goal-response-last" | "goal-response-last-split"), None)
                if self.goal_is_active() =>
            {
                let messages = self.goal_turn_before_reply(message, &thread_id);
                if behavior == "goal-response-last" {
                    self.with_batch(|s| messages.into_iter().for_each(|m| s.send(m)));
                } else {
                    self.send_spaced(messages, 20);
                }
            }

            (behavior, parked) => {
                let starts_turn = self.goal_is_active() && parked.is_none();
                // "goal-instant" answers and runs the whole turn in one write, the
                // shape a turn that fails the moment it starts produces.
                if behavior == "goal-instant" && starts_turn {
                    self.with_batch(|s| {
                        s.answer_goal(message, &thread_id);
                        s.start_goal_turn_from_goal(&thread_id);
                    });
                    return Ok(());
                }
                self.answer_goal(message, &thread_id);
                if starts_turn {
                    let delay = env_ms("CEPTION_FAKE_GOAL_TURN_DELAY_MS", 100);
                    set_timeout(delay, move |s| s.start_goal_turn_from_goal(&thread_id));
                }
            }
        }
        Ok(())
    }

    /// The `goal-response-last` sequence, reply last. State changes land at once
    /// (the goal ends `blocked`, no turn left running); only the writes may be
    /// spread out.
    fn goal_turn_before_reply(&mut self, message: &Value, thread_id: &str) -> Vec<Value> {
        let stale = self.build_goal(thread_id);
        let turn = Turn::unprompted(thread_id);
        let turn_id = &turn.turn_id;
        self.goal_turns += 1;
        self.goal.as_mut().expect("goal just set").status = json!("blocked");
        let blocked = self.build_goal(thread_id);
        vec![
            goal_updated(thread_id, stale.clone()),
            turn_started(thread_id, turn_id),
            agent_message(thread_id, turn_id, "Goal turn ran before the reply."),
            token_usage_updated(thread_id, turn_id),
            turn_completed(thread_id, turn_id, "failed", policy_stop()),
            goal_updated(thread_id, blocked),
            reply(message, json!({ "goal": stale })),
        ]
    }

    /// Each message its own write, `gap_ms` apart, in order.
    fn send_spaced(&mut self, messages: Vec<Value>, gap_ms: u64) {
        let mut rest = messages.into_iter();
        if let Some(first) = rest.next() {
            self.send(first);
        }
        if rest.len() > 0 {
            set_timeout(gap_ms, move |s| s.send_spaced(rest.collect(), gap_ms));
        }
    }

    fn answer_goal(&mut self, message: &Value, thread_id: &str) {
        let goal = self.build_goal(thread_id);
        self.send(reply(message, json!({ "goal": goal })));
        self.send_goal_updated(thread_id);
    }

    fn turn_steer(&mut self, message: &Value) {
        let params = &message["params"];
        let steer_prompt = prompt_text(&params["input"]);
        update_state(|state| {
            state.steers.push(json!({
                "threadId": params["threadId"],
                "expectedTurnId": params["expectedTurnId"],
                "prompt": steer_prompt
            }));
        });
        self.send(reply(
            message,
            json!({ "turnId": params["expectedTurnId"] }),
        ));
        if let Some(steered) = self.active_turn.clone() {
            set_timeout(100, move |s| {
                let text = format!("Steered response.\nSteer: {steer_prompt}");
                s.complete_turn(&steered, "completed", Some(&text));
                // A goal would keep starting turns; call it done so the steered
                // turn is the end of the run.
                if s.goal_is_active() {
                    s.set_goal_status(&steered.thread_id, "complete");
                }
            });
        }
    }
}

fn main() {
    // An uncaught exception kills a node process from any callback; a panicking
    // timer thread would otherwise die alone and leave the fake hanging.
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        default_hook(info);
        process::exit(101);
    }));

    update_state(|state| state.app_server_starts += 1);

    for line in io::stdin().lock().lines() {
        let line = line.expect("read stdin");
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = serde_json::from_str(&line).expect("parse message");
        server().dispatch(&message);
        while DEAF.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_secs(3600));
        }
    }
    wait_for_timers();
}
