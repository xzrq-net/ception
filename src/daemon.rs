//! The per-label daemon: owns one codex app-server and one thread, serves
//! CLI clients over a unix socket.
//!
//! One actor owns all state and handles events in order. App-server output
//! arrives on one channel in stream order, and RPC responses are handled at
//! their position in that stream as stored continuations ([`Pending`]).
//! Stream order is not the whole story: codex snapshots a goal, persists it,
//! and only then answers, so goal notifications and even whole turns can land
//! before the reply that started them. Goal continuations therefore check for
//! fresher notifications and for turns that settled while they were in flight.

use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;
use tokio::time::Instant;

use crate::appserver::{AppEvent, AppServer, Incoming};
use crate::paths::{LabelPaths, project_hash};
use crate::procfs::PidWatch;
use crate::proto::{DaemonStatus, GoalAction, Outcome, Reply, Request};
use crate::render::{ReportLevel, TurnAccumulator, format_goal};
use crate::store::{self, Record};

#[derive(clap::Args, Clone, Debug)]
pub struct Options {
    #[arg(long)]
    pub project: PathBuf,
    #[arg(long)]
    pub session: String,
    #[arg(long)]
    pub label: String,
    /// Resume the thread in the label's record (read under the label lock,
    /// together with its model and effort) instead of starting fresh.
    #[arg(long, conflicts_with_all = ["model", "effort"])]
    pub resume: bool,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub effort: Option<String>,
    #[arg(long, requires = "watch_starttime")]
    pub watch_pid: Option<u32>,
    #[arg(long)]
    pub watch_starttime: Option<u64>,
}

fn env_duration(name: &str, default_ms: u64) -> Duration {
    let ms = std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default_ms);
    Duration::from_millis(ms)
}

pub fn spawn_timeout() -> Duration {
    let secs = std::env::var("CEPTION_SPAWN_TIMEOUT_SECS").ok().and_then(|v| v.parse().ok());
    Duration::from_secs(secs.unwrap_or(120))
}

/// gc's probe can hold a label lock for a moment; only a lock held longer
/// than this means another daemon.
const LOCK_PATIENCE: Duration = Duration::from_millis(500);

/// Entry point for `ception daemon`. Stdout is the spawning client's
/// readiness pipe: exactly one line, `ready`, `busy` or `error: ...`.
pub async fn run(options: Options) -> Result<()> {
    let hash = project_hash(&options.project);
    let paths = LabelPaths::new(&hash, &options.session, &options.label)?;
    paths.ensure_dirs()?;

    let deadline = Instant::now() + LOCK_PATIENCE;
    let lock = loop {
        if let Some(lock) = store::try_lock(&paths.lock)? {
            break lock;
        }
        if Instant::now() >= deadline {
            signal_ready("busy");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    // Opened only once the lock is ours: gc deletes logs under the lock.
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.log)
        .with_context(|| format!("open {}", paths.log.display()))?;
    // Panics and anything else on stderr land in the log too.
    unsafe { libc::dup2(log_file.as_raw_fd(), 2) };
    let mut log = Log(log_file);
    log.line(&format!(
        "[daemon] starting label={} cwd={} pid={}",
        options.label,
        options.project.display(),
        std::process::id()
    ));

    let mut signals = Signals {
        term: signal(SignalKind::terminate())?,
        int: signal(SignalKind::interrupt())?,
    };
    match boot(options, paths, &mut log, &mut signals).await {
        Ok(booted) => {
            signal_ready("ready");
            Daemon::new(booted, log).serve(signals).await;
        }
        Err(error) => {
            log.line(&format!("[daemon] startup failed: {error:#}"));
            signal_ready(&format!("error: {error:#}"));
        }
    }
    // The app-server is closed by now; only then may a successor take over.
    drop(lock);
    Ok(())
}

/// Answer the spawning client, then detach stdout so the pipe closes. A
/// client that already went away costs nothing: SIGPIPE is ignored.
fn signal_ready(line: &str) {
    let mut stdout = std::io::stdout();
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
    if let Ok(null) = File::options().write(true).open("/dev/null") {
        unsafe { libc::dup2(null.as_raw_fd(), 1) };
    }
}

struct Log(File);

impl Log {
    fn line(&mut self, line: &str) {
        let _ = writeln!(self.0, "{}", line.trim_end());
    }
}

struct Signals {
    term: Signal,
    int: Signal,
}

impl Signals {
    async fn recv(&mut self) -> &'static str {
        tokio::select! {
            _ = self.term.recv() => "SIGTERM",
            _ = self.int.recv() => "SIGINT",
        }
    }
}

struct Booted {
    options: Options,
    paths: LabelPaths,
    app: AppServer,
    app_events: UnboundedReceiver<AppEvent>,
    /// What arrived during startup besides the responses it waited for,
    /// replayed into the actor in order. Resuming can report a goal and
    /// even start its turn.
    backlog: Vec<AppEvent>,
    listener: UnixListener,
    watch: Option<PidWatch>,
    thread_id: Option<String>,
    model: Option<String>,
    effort: Option<String>,
}

/// Start the app-server, resume the thread, listen. Bounded, and abandoned if
/// the watched process dies or a signal arrives; dropping the half-started
/// app-server kills it.
async fn boot(options: Options, paths: LabelPaths, log: &mut Log, signals: &mut Signals) -> Result<Booted> {
    let watch = match (options.watch_pid, options.watch_starttime) {
        (Some(pid), Some(starttime)) => {
            Some(PidWatch::open(pid, starttime)?.ok_or_else(|| anyhow!("watched process {pid} is gone"))?)
        }
        _ => None,
    };
    let (thread_id, model, effort) = if options.resume {
        let record = store::read(&paths.record)?
            .ok_or_else(|| anyhow!("no stored thread for {}", options.label))?;
        (Some(record.thread_id), record.model, record.effort)
    } else {
        (None, options.model.clone(), options.effort.clone())
    };

    let start = async {
        let (mut app, mut app_events) = AppServer::spawn(&options.project)?;
        let mut backlog = Vec::new();
        app.initialize(&mut app_events, |event| backlog.push(event)).await?;
        let mut thread_id = thread_id;
        if let Some(resume) = &thread_id {
            let mut params = thread_params(&options.project, model.as_deref());
            params["threadId"] = json!(resume);
            let response = app
                .call(&mut app_events, "thread/resume", params, |event| backlog.push(event))
                .await?;
            if let Some(id) = response["thread"]["id"].as_str() {
                thread_id = Some(id.to_string());
            }
        }
        let _ = std::fs::remove_file(&paths.socket);
        let listener = UnixListener::bind(&paths.socket)
            .with_context(|| format!("listen on {}", paths.socket.display()))?;
        std::fs::set_permissions(&paths.socket, std::fs::Permissions::from_mode(0o600))?;
        anyhow::Ok((app, app_events, backlog, listener, thread_id))
    };
    let watch_exit = async {
        match &watch {
            Some(watch) => watch.exited().await,
            None => std::future::pending().await,
        }
    };
    let (app, app_events, backlog, listener, thread_id) = tokio::select! {
        started = start => started?,
        _ = tokio::time::sleep(spawn_timeout()) => bail!("startup timed out"),
        _ = watch_exit => bail!("watched process exited during startup"),
        signal = signals.recv() => bail!("{signal} during startup"),
    };
    if let Some(thread_id) = &thread_id {
        log.line(&format!("[thread] resumed {thread_id}"));
    }
    log.line(&format!("[daemon] listening {}", paths.socket.display()));
    Ok(Booted { options, paths, app, app_events, backlog, listener, watch, thread_id, model, effort })
}

fn thread_params(project: &std::path::Path, model: Option<&str>) -> Value {
    let mut params = json!({
        "cwd": project,
        "approvalPolicy": "never",
        "sandbox": "danger-full-access",
    });
    if let Some(model) = model {
        params["model"] = json!(model);
    }
    params
}

/// Someone waiting on a reply.
struct Client {
    reply: oneshot::Sender<Reply>,
    report: ReportLevel,
}

impl Client {
    fn answer(self, reply: Reply) {
        let _ = self.reply.send(reply);
    }
}

/// One logical run: a turn plus any continuation turns folded into it.
struct ActiveTurn {
    /// Identifies the run to the request that created it.
    op: u64,
    /// None until turn/start answers or turn/started names it.
    turn_id: Option<String>,
    acc: TurnAccumulator,
    clients: Vec<Client>,
    header_logged: bool,
    /// Compactions that already paid a grace hold, so a continuation that
    /// ends clean doesn't wait again.
    compactions_held: u32,
}

/// A request's continuation, run when its response arrives.
enum Pending {
    ThreadStart { then: AfterThread },
    /// Its clients live on the active run `op`.
    TurnStart { op: u64 },
    Steer { client: Client },
    GoalSet { client: Client, action: GoalAction, goal_updates: u64, turns_settled: u64 },
    GoalGet { client: Client, goal_updates: u64 },
    GoalClear { client: Client, goal_updates: u64 },
    /// An interrupt pausing the active goal first, so freeing the thread
    /// doesn't just start the goal's next turn.
    InterruptPause { client: Client, goal_updates: u64 },
    Interrupt { client: Option<Client>, turn_id: String, paused: bool },
}

enum AfterThread {
    /// The active run `op` already holds the prompt and clients.
    Turn { op: u64 },
    Goal { client: Client, action: GoalAction, objective: Option<String> },
}

/// Work that waits for the thread, a turn id, or a held report to resolve.
enum Deferred {
    Command(Request, Client),
    /// The second half of an interrupt, after its goal pause.
    Interrupt { client: Client, paused: bool },
}

enum Event {
    Command { request: Request, reply: oneshot::Sender<Reply> },
    Disconnected,
    GraceExpired(u64),
    GoalStartExpired(u64),
}

struct GoalWaiter {
    id: u64,
    client: Client,
}

struct Daemon {
    options: Options,
    paths: LabelPaths,
    log: Log,
    app: AppServer,
    app_events: Option<UnboundedReceiver<AppEvent>>,
    backlog: Vec<AppEvent>,
    listener: Option<UnixListener>,
    watch: Option<PidWatch>,
    events: UnboundedSender<Event>,
    event_rx: Option<UnboundedReceiver<Event>>,
    model: Option<String>,
    effort: Option<String>,

    thread_id: Option<String>,
    thread_starting: bool,
    active: Option<ActiveTurn>,
    /// Generation of the grace timer while a finished turn's report is held
    /// for a continuation.
    hold: Option<u64>,
    /// Anything but an "active" goal means codex starts no more turns.
    goal: Option<Value>,
    /// Goal notifications seen, so a reply can tell it is older than them.
    goal_updates: u64,
    goal_waiters: Vec<GoalWaiter>,
    pending: HashMap<u64, Pending>,
    deferred: VecDeque<Deferred>,
    turns_settled: u64,
    /// For a goal whose reply arrived after the turn it started had settled.
    last_settled: Option<TurnAccumulator>,

    next_id: u64,
    connections: usize,
    last_activity: Instant,
    last_rate_line: String,
    shutdown_request: Option<String>,
    shutting_down: bool,
}

impl Daemon {
    fn new(booted: Booted, log: Log) -> Self {
        let (events, event_rx) = unbounded_channel();
        Self {
            options: booted.options,
            paths: booted.paths,
            log,
            app: booted.app,
            app_events: Some(booted.app_events),
            backlog: booted.backlog,
            listener: Some(booted.listener),
            watch: booted.watch,
            events,
            event_rx: Some(event_rx),
            model: booted.model,
            effort: booted.effort,
            thread_id: booted.thread_id,
            thread_starting: false,
            active: None,
            hold: None,
            goal: None,
            goal_updates: 0,
            goal_waiters: Vec::new(),
            pending: HashMap::new(),
            deferred: VecDeque::new(),
            turns_settled: 0,
            last_settled: None,
            next_id: 0,
            connections: 0,
            last_activity: Instant::now(),
            last_rate_line: String::new(),
            shutdown_request: None,
            shutting_down: false,
        }
    }

    async fn serve(mut self, mut signals: Signals) {
        if self.thread_id.is_some() {
            if let Err(error) = self.persist() {
                self.log(&format!("[state] {error:#}; shutting down"));
                self.shutdown("label not persisted").await;
                return;
            }
        }
        let mut app_events = self.app_events.take().expect("serve runs once");
        let mut events = self.event_rx.take().expect("serve runs once");
        let listener = self.listener.take().expect("serve runs once");
        let watch = self.watch.take();
        for event in std::mem::take(&mut self.backlog) {
            self.on_app(event).await;
        }
        let idle = Duration::from_secs(
            std::env::var("CEPTION_IDLE_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(4 * 60 * 60),
        );

        while !self.shutting_down {
            let idle_at = self.idle_eligible().then(|| self.last_activity + idle);
            let watch_exit = async {
                match &watch {
                    Some(watch) => watch.exited().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                Some(event) = app_events.recv() => self.on_app(event).await,
                Some(event) = events.recv() => self.on_event(event).await,
                accepted = listener.accept() => {
                    if let Ok((stream, _)) = accepted {
                        self.connections += 1;
                        tokio::spawn(serve_connection(stream, self.events.clone()));
                    }
                }
                _ = watch_exit => {
                    let pid = self.options.watch_pid.unwrap_or_default();
                    self.shutdown_request = Some(format!("watched process {pid} exited"));
                }
                _ = tokio::time::sleep_until(idle_at.unwrap_or_else(Instant::now)), if idle_at.is_some() => {
                    self.shutdown_request = Some("idle timeout".into());
                }
                signal = signals.recv() => self.shutdown_request = Some(signal.into()),
            }
            if let Some(reason) = self.shutdown_request.take() {
                self.interrupt_for_shutdown(&mut app_events).await;
                self.shutdown(&reason).await;
                break;
            }
            self.drain_deferred().await;
        }
        drop(listener);
        let _ = std::fs::remove_file(&self.paths.socket);
    }

    fn idle_eligible(&self) -> bool {
        self.active.is_none()
            && self.hold.is_none()
            && self.connections == 0
            && self.goal_waiters.is_empty()
            && self.pending.is_empty()
            && self.deferred.is_empty()
    }

    fn log(&mut self, line: &str) {
        self.log.line(line);
    }

    fn label(&self) -> &str {
        &self.options.label
    }

    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn goal_status(&self) -> Option<&str> {
        self.goal.as_ref()?.get("status")?.as_str()
    }

    fn goal_active(&self) -> bool {
        self.goal_status() == Some("active")
    }

    fn goal_line(&self) -> String {
        format_goal(self.goal.as_ref(), Some(self.label()))
    }

    fn persist(&self) -> Result<()> {
        let Some(thread_id) = &self.thread_id else {
            return Ok(());
        };
        let record = Record {
            cwd: self.options.project.clone(),
            thread_id: thread_id.clone(),
            model: self.model.clone(),
            effort: self.effort.clone(),
        };
        store::write(&self.paths.record, &record)
    }

    fn persist_or_log(&mut self) {
        if let Err(error) = self.persist() {
            self.log(&format!("[state] {error:#}"));
        }
    }

    fn after(&self, delay: Duration, event: Event) {
        let events = self.events.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = events.send(event);
        });
    }

    async fn request(&mut self, method: &str, params: Value, pending: Pending) -> Result<()> {
        let id = self.app.request(method, params).await?;
        self.pending.insert(id, pending);
        Ok(())
    }

    // ----- client commands -------------------------------------------------

    async fn on_event(&mut self, event: Event) {
        match event {
            Event::Command { request, reply } => {
                // Probes (list, watch on an idle daemon) don't count as use,
                // or anything polling `ception list` would keep daemons alive.
                if !matches!(request, Request::Status | Request::Watch { .. }) {
                    self.last_activity = Instant::now();
                }
                let report = match &request {
                    Request::Send { report, .. } | Request::Goal { report, .. } | Request::Watch { report } => *report,
                    _ => ReportLevel::Brief,
                };
                let client = Client { reply, report };
                if self.shutting_down {
                    client.answer(Reply::error("daemon shutting down"));
                    return;
                }
                let item = Deferred::Command(request, client);
                if self.must_defer(&item) {
                    self.deferred.push_back(item);
                } else {
                    self.dispatch(item).await;
                }
            }
            Event::Disconnected => {
                self.connections = self.connections.saturating_sub(1);
            }
            Event::GraceExpired(generation) => {
                if self.hold == Some(generation) {
                    self.hold = None;
                    self.log("[turn] no continuation arrived; settling");
                    self.finish_active_turn();
                }
            }
            Event::GoalStartExpired(id) => {
                if let Some(index) = self.goal_waiters.iter().position(|w| w.id == id) {
                    let waiter = self.goal_waiters.remove(index);
                    let ms = env_duration("CEPTION_GOAL_START_MS", 30_000).as_millis();
                    self.log(&format!("[goal] no turn started within {ms}ms"));
                    let report = format!("{}\n\n(codex started no turn within {ms}ms)", self.goal_line());
                    waiter.client.answer(self.goal_reply("ok", report));
                }
            }
        }
    }

    /// Whether `item` must wait. A held report may still continue, a turn
    /// without an id cannot be steered or interrupted, and nothing that
    /// needs the thread can run while it is being created.
    fn must_defer(&self, item: &Deferred) -> bool {
        let unsettled = self.active.as_ref().is_some_and(|turn| turn.turn_id.is_none());
        let held = self.hold.is_some();
        let starting = self.thread_starting;
        match item {
            Deferred::Command(Request::Send { .. }, _) => held || starting || unsettled,
            Deferred::Command(Request::Goal { action, .. }, _) => match action {
                GoalAction::Set | GoalAction::Resume => held || starting,
                GoalAction::Pause | GoalAction::Clear => starting,
                GoalAction::Show => false,
            },
            // An active goal is paused at once, which settles any hold; a
            // compaction hold has to play out to know what to interrupt.
            Deferred::Command(Request::Interrupt, _) | Deferred::Interrupt { .. } => {
                starting || unsettled || (held && !self.goal_active())
            }
            Deferred::Command(..) => false,
        }
    }

    async fn drain_deferred(&mut self) {
        while !self.shutting_down {
            match self.deferred.front() {
                Some(item) if !self.must_defer(item) => {
                    let item = self.deferred.pop_front().expect("front exists");
                    self.dispatch(item).await;
                }
                _ => return,
            }
        }
    }

    async fn dispatch(&mut self, item: Deferred) {
        let result = match item {
            Deferred::Interrupt { client, paused } => self.interrupt_current(client, paused).await,
            Deferred::Command(request, client) => match request {
                Request::Status => {
                    client.answer(self.status_reply());
                    Ok(())
                }
                Request::Send { prompt, .. } => self.send(prompt, client).await,
                Request::Goal { action, objective, .. } => self.goal_command(action, objective, client).await,
                Request::Interrupt => self.interrupt(client).await,
                Request::Watch { .. } => {
                    match &mut self.active {
                        Some(turn) => turn.clients.push(client),
                        None => client.answer(Reply::ok("idle", "no active turn")),
                    }
                    Ok(())
                }
                Request::Shutdown => {
                    client.answer(Reply::ok("ok", "shutdown accepted"));
                    self.shutdown_request = Some("client shutdown".into());
                    Ok(())
                }
            },
        };
        if let Err(error) = result {
            self.log(&format!("[error] {error:#}"));
        }
    }

    fn status_reply(&self) -> Reply {
        Reply::Result(Outcome {
            status: "ok".into(),
            daemon: Some(DaemonStatus {
                pid: std::process::id(),
                thread_id: self.thread_id.clone(),
                turn_id: self.active.as_ref().and_then(|turn| turn.turn_id.clone()),
                state: if self.active.is_some() { "active" } else { "idle" }.into(),
                goal: self.goal.clone(),
                log: self.paths.log.clone(),
                watch_pid: self.options.watch_pid,
                watch_starttime: self.options.watch_starttime,
            }),
            ..Default::default()
        })
    }

    fn goal_reply(&self, status: &str, report: String) -> Reply {
        Reply::Result(Outcome { status: status.into(), report, goal: self.goal.clone(), ..Default::default() })
    }

    /// Steer the live turn, else start one.
    async fn send(&mut self, prompt: String, client: Client) -> Result<()> {
        if let Some(turn_id) = self.active.as_ref().and_then(|turn| turn.turn_id.clone()) {
            let params = json!({
                "threadId": self.thread_id,
                "expectedTurnId": turn_id,
                "input": [text_input(&prompt)],
            });
            return self.request("turn/steer", params, Pending::Steer { client }).await;
        }
        // Claim the slot first, so commands arriving while the thread or turn
        // starts wait for it instead of racing.
        let op = self.next_id();
        let thread_id = self.thread_id.clone().unwrap_or_default();
        self.active = Some(ActiveTurn {
            op,
            turn_id: None,
            acc: TurnAccumulator::new(self.label(), &thread_id, "starting", &prompt),
            clients: vec![client],
            header_logged: false,
            compactions_held: 0,
        });
        if self.thread_id.is_some() {
            self.start_turn(op).await
        } else {
            self.start_thread(AfterThread::Turn { op }).await
        }
    }

    async fn start_thread(&mut self, then: AfterThread) -> Result<()> {
        self.thread_starting = true;
        let params = thread_params(&self.options.project, self.model.as_deref());
        let id = match self.app.request("thread/start", params).await {
            Ok(id) => id,
            Err(error) => {
                self.thread_starting = false;
                self.fail_after_thread(then, &format!("{error:#}"));
                return Err(error);
            }
        };
        self.pending.insert(id, Pending::ThreadStart { then });
        Ok(())
    }

    async fn start_turn(&mut self, op: u64) -> Result<()> {
        let Some(turn) = self.active.as_ref().filter(|turn| turn.op == op) else {
            return Ok(());
        };
        let mut params = json!({
            "threadId": self.thread_id,
            "input": [text_input(&turn.acc.prompt)],
            "cwd": self.options.project,
            "approvalPolicy": "never",
            "sandboxPolicy": { "type": "dangerFullAccess" },
        });
        if let Some(model) = &self.model {
            params["model"] = json!(model);
        }
        if let Some(effort) = &self.effort {
            params["effort"] = json!(effort);
        }
        if let Err(error) = self.request("turn/start", params, Pending::TurnStart { op }).await {
            self.fail_active(&format!("{error:#}"));
            return Err(error);
        }
        Ok(())
    }

    /// The active run never got going: tell its clients.
    fn fail_active(&mut self, message: &str) {
        if let Some(turn) = self.active.take() {
            for client in turn.clients {
                client.answer(Reply::error(message));
            }
        }
    }

    fn fail_after_thread(&mut self, then: AfterThread, message: &str) {
        match then {
            AfterThread::Turn { op } => {
                if self.active.as_ref().is_some_and(|turn| turn.op == op) {
                    self.fail_active(message);
                }
            }
            AfterThread::Goal { client, .. } => client.answer(Reply::error(message)),
        }
    }

    async fn goal_command(&mut self, action: GoalAction, objective: Option<String>, client: Client) -> Result<()> {
        let Some(thread_id) = self.thread_id.clone() else {
            return match action {
                GoalAction::Show | GoalAction::Clear => {
                    client.answer(self.goal_reply("ok", self.goal_line()));
                    Ok(())
                }
                _ => self.start_thread(AfterThread::Goal { client, action, objective }).await,
            };
        };
        let goal_updates = self.goal_updates;
        match action {
            GoalAction::Show => {
                let pending = Pending::GoalGet { client, goal_updates };
                self.request("thread/goal/get", json!({ "threadId": thread_id }), pending).await
            }
            GoalAction::Clear => {
                let pending = Pending::GoalClear { client, goal_updates };
                self.request("thread/goal/clear", json!({ "threadId": thread_id }), pending).await
            }
            GoalAction::Set | GoalAction::Resume | GoalAction::Pause => {
                let status = if action == GoalAction::Pause { "paused" } else { "active" };
                let mut params = json!({ "threadId": thread_id, "status": status });
                if let Some(objective) = &objective {
                    params["objective"] = json!(objective);
                }
                let turns_settled = self.turns_settled;
                let pending = Pending::GoalSet { client, action, goal_updates, turns_settled };
                self.request("thread/goal/set", params, pending).await?;
                if let Some(objective) = &objective {
                    self.log(&format!("[goal] objective: {objective}"));
                }
                Ok(())
            }
        }
    }

    async fn interrupt(&mut self, client: Client) -> Result<()> {
        // Paused even with no turn running: a held run or a goal about to
        // start its next turn must stop too.
        if self.goal_active() {
            if let Some(thread_id) = self.thread_id.clone() {
                let params = json!({ "threadId": thread_id, "status": "paused" });
                let pending = Pending::InterruptPause { client, goal_updates: self.goal_updates };
                return self.request("thread/goal/set", params, pending).await;
            }
        }
        self.interrupt_current(client, false).await
    }

    /// Interrupt whatever turn is running now, which after a goal pause may
    /// be a continuation of the one the interrupt was aimed at.
    async fn interrupt_current(&mut self, client: Client, paused: bool) -> Result<()> {
        let held_back = Deferred::Interrupt { client, paused };
        if self.must_defer(&held_back) {
            self.deferred.push_back(held_back);
            return Ok(());
        }
        let Deferred::Interrupt { client, paused } = held_back else {
            unreachable!()
        };
        let turn_id = self.active.as_ref().and_then(|turn| turn.turn_id.clone());
        let (Some(turn_id), Some(thread_id)) = (turn_id, self.thread_id.clone()) else {
            let report = if paused { format!("no active turn\n{}", self.goal_line()) } else { "no active turn".into() };
            client.answer(self.goal_reply("idle", report));
            return Ok(());
        };
        let params = json!({ "threadId": thread_id, "turnId": turn_id });
        let pending = Pending::Interrupt { client: Some(client), turn_id, paused };
        self.request("turn/interrupt", params, pending).await
    }

    // ----- app-server ------------------------------------------------------

    async fn on_app(&mut self, event: AppEvent) {
        match event {
            AppEvent::Stderr(line) => self.log(&format!("[app-server stderr] {line}")),
            AppEvent::Closed(reason) => {
                if self.shutting_down {
                    return;
                }
                let message = self.app.exit_description(reason).await;
                self.log(&format!("[app-server] {message}"));
                self.hold = None;
                self.fail_everyone(&message);
                self.shutdown_request = Some("app-server exit".into());
            }
            AppEvent::Message(message) => match Incoming::classify(message.clone()) {
                Some(Incoming::Notification { method, params }) => self.on_notification(&method, &params, &message),
                Some(Incoming::Response { id, outcome }) => {
                    if let Some(pending) = self.pending.remove(&id) {
                        if let Err(error) = self.on_response(pending, outcome).await {
                            self.log(&format!("[error] {error:#}"));
                        }
                    }
                }
                Some(Incoming::Request { id, method }) => self.on_server_request(id, &method, &message).await,
                None => self.log(&format!("[debug] unrecognised message {message}")),
            },
        }
    }

    /// Take a reply's goal snapshot unless a goal notification arrived since
    /// the request went out; notifications are what settlement keys on.
    fn take_goal_snapshot(&mut self, response: &Value, goal_updates: u64) {
        if self.goal_updates == goal_updates {
            self.goal = non_null(&response["goal"]);
        }
    }

    async fn on_response(&mut self, pending: Pending, outcome: std::result::Result<Value, String>) -> Result<()> {
        match pending {
            Pending::ThreadStart { then } => {
                self.thread_starting = false;
                let thread_id = match outcome {
                    Ok(response) => response["thread"]["id"].as_str().map(str::to_string),
                    Err(error) => {
                        self.fail_after_thread(then, &error);
                        return Ok(());
                    }
                };
                let Some(thread_id) = thread_id else {
                    self.fail_after_thread(then, "thread/start returned no thread id");
                    return Ok(());
                };
                self.thread_id = Some(thread_id.clone());
                if let Err(error) = self.persist() {
                    // Unrecorded, the thread could never be resumed or listed;
                    // fail the client and go away rather than linger.
                    self.thread_id = None;
                    let message = format!("{error:#}");
                    self.log(&format!("[state] {message}; shutting down"));
                    self.fail_after_thread(then, &message);
                    self.shutdown_request = Some("label not persisted".into());
                    return Ok(());
                }
                self.log(&format!("[thread] started {thread_id}"));
                match then {
                    AfterThread::Turn { op } => {
                        if let Some(turn) = self.active.as_mut().filter(|turn| turn.op == op) {
                            turn.acc.thread_id = thread_id;
                        }
                        self.start_turn(op).await
                    }
                    AfterThread::Goal { client, action, objective } => {
                        self.goal_command(action, objective, client).await
                    }
                }
            }
            Pending::TurnStart { op } => {
                let Some(turn) = self.active.as_mut().filter(|turn| turn.op == op) else {
                    // The run this request created already settled.
                    return Ok(());
                };
                match outcome {
                    Ok(response) => {
                        // The response id is authoritative: a continuation
                        // starting meanwhile may have claimed the slot through
                        // turn/started, and keeping its id would filter out
                        // every event of the turn we asked for.
                        let started = response["turn"]["id"].as_str().map(str::to_string);
                        let raced = match (&started, &turn.turn_id) {
                            (Some(started), Some(claimed)) if started != claimed => Some(format!(
                                "[turn] unsolicited turn {claimed} raced our start; re-targeting to {started}"
                            )),
                            _ => None,
                        };
                        if let Some(started) = started {
                            turn.turn_id = Some(started);
                        }
                        turn.acc.turn_id = turn.turn_id.clone().unwrap_or_else(|| "unknown".into());
                        if let Some(line) = raced {
                            self.log(&line);
                        }
                        self.log_turn_header();
                    }
                    Err(error) => {
                        for client in std::mem::take(&mut turn.clients) {
                            client.answer(Reply::error(error.clone()));
                        }
                        // A turn id here came from turn/started for a turn
                        // codex started on its own, which won the thread; keep
                        // tracking it ownerless rather than let it run unseen.
                        match turn.turn_id.clone() {
                            Some(turn_id) => self.log(&format!(
                                "[turn] turn/start failed but turn {turn_id} is running; keeping it tracked"
                            )),
                            None => self.active = None,
                        }
                    }
                }
                Ok(())
            }
            Pending::Steer { client } => {
                match outcome {
                    Ok(response) => {
                        let turn_id = response["turnId"]
                            .as_str()
                            .map(str::to_string)
                            .or_else(|| self.active.as_ref().and_then(|turn| turn.turn_id.clone()))
                            .unwrap_or_default();
                        client.answer(Reply::ok("steered", format!("steered active turn {turn_id}")));
                    }
                    Err(error) => client.answer(Reply::error(error)),
                }
                Ok(())
            }
            Pending::GoalGet { client, goal_updates } => {
                match outcome {
                    Ok(response) => {
                        self.take_goal_snapshot(&response, goal_updates);
                        client.answer(self.goal_reply("ok", self.goal_line()));
                    }
                    Err(error) => client.answer(Reply::error(error)),
                }
                Ok(())
            }
            Pending::GoalClear { client, goal_updates } => {
                match outcome {
                    Ok(_) => {
                        if self.goal_updates == goal_updates {
                            self.goal = None;
                        }
                        self.log("[goal] cleared");
                        let report = match &self.goal {
                            None => "goal cleared".to_string(),
                            Some(_) => format!("goal cleared, but a newer one is already set\n{}", self.goal_line()),
                        };
                        client.answer(self.goal_reply("ok", report));
                    }
                    Err(error) => client.answer(Reply::error(error)),
                }
                Ok(())
            }
            Pending::GoalSet { client, action, goal_updates, turns_settled } => {
                let response = match outcome {
                    Ok(response) => response,
                    Err(error) => {
                        client.answer(Reply::error(error));
                        return Ok(());
                    }
                };
                self.take_goal_snapshot(&response, goal_updates);
                let status = self.goal_status().unwrap_or("unknown").to_string();
                self.log(&format!("[goal] {} -> {status}", goal_action_name(action)));
                if action == GoalAction::Pause {
                    client.answer(self.goal_reply("ok", self.goal_line()));
                } else {
                    self.attach_goal_client(client, turns_settled);
                }
                Ok(())
            }
            Pending::InterruptPause { client, goal_updates } => {
                match outcome {
                    Ok(response) => {
                        self.take_goal_snapshot(&response, goal_updates);
                        self.log("[goal] paused so the interrupt ends the run");
                        self.interrupt_current(client, true).await
                    }
                    Err(error) => {
                        client.answer(Reply::error(error));
                        Ok(())
                    }
                }
            }
            Pending::Interrupt { client, turn_id, paused } => {
                match (outcome, client) {
                    (Ok(_), Some(client)) => {
                        let mut report = format!("interrupt requested for {turn_id}");
                        if paused {
                            report = format!("{report}\n{}", self.goal_line());
                        }
                        client.answer(self.goal_reply("ok", report));
                    }
                    (Err(error), Some(client)) => client.answer(Reply::error(error)),
                    (Err(error), None) => self.log(&format!("[interrupt] {error}")),
                    (Ok(_), None) => {}
                }
                Ok(())
            }
        }
    }

    /// Where a goal client waits for the goal's work: a turn that settled
    /// before the reply arrived, the turn running now (codex folds a goal set
    /// mid-turn into it), or the turn codex is about to start.
    fn attach_goal_client(&mut self, client: Client, turns_settled_before: u64) {
        let goal_active = self.goal_active();
        if self.turns_settled > turns_settled_before && !goal_active {
            if let Some(turn) = &self.last_settled {
                let reply = self.turn_reply(turn, client.report);
                self.log("[goal] the goal's turn ran and stopped while it was being set");
                client.answer(reply);
                return;
            }
        }
        if let Some(turn) = &mut self.active {
            turn.clients.push(client);
            return;
        }
        if !goal_active {
            let report = format!("{}\n\n(codex started no turn)", self.goal_line());
            client.answer(self.goal_reply("ok", report));
            return;
        }
        let id = self.next_id();
        self.goal_waiters.push(GoalWaiter { id, client });
        self.after(env_duration("CEPTION_GOAL_START_MS", 30_000), Event::GoalStartExpired(id));
    }

    fn on_notification(&mut self, method: &str, params: &Value, raw: &Value) {
        let ours = self.thread_id.is_some() && params["threadId"].as_str() == self.thread_id.as_deref();
        match method {
            "thread/tokenUsage/updated" => {
                if let Some(turn) = &mut self.active {
                    if ours && params["turnId"].as_str() == turn.turn_id.as_deref() {
                        turn.acc.handle_notification(method, params);
                    }
                }
                return;
            }
            "account/rateLimits/updated" => {
                let line = format!("[rate] {}", format_rate_limits(&params["rateLimits"]));
                if line != self.last_rate_line {
                    self.log(&line);
                    self.last_rate_line = line;
                }
                return;
            }
            "turn/started" if ours => {
                let turn_id = params["turn"]["id"].as_str().map(str::to_string);
                if self.active.is_none() {
                    // No run in flight, yet codex started a turn on our
                    // thread: the goal's turn, or a continuation we already
                    // reported. Track it either way; an unobserved turn still
                    // edits the repo.
                    self.adopt_orphan_turn(turn_id);
                    return;
                }
                if self.hold.is_some() {
                    self.resume_continuation(turn_id);
                    return;
                }
                let turn = self.active.as_mut().expect("checked above");
                if turn.turn_id.is_none() {
                    turn.turn_id = turn_id;
                }
                turn.acc.turn_id = turn.turn_id.clone().unwrap_or_else(|| "unknown".into());
                // One thread runs one turn: a goal set now is folded into it.
                self.absorb_goal_waiters();
                self.log_turn_header();
                return;
            }
            "thread/goal/updated" | "thread/goal/cleared" => {
                // Codex's own subagents run threads and goals on this
                // app-server; their status must not settle our run.
                if !ours {
                    return;
                }
                let goal = if method == "thread/goal/cleared" { None } else { non_null(&params["goal"]) };
                let before = self.goal_status().map(str::to_string);
                self.goal = goal;
                self.goal_updates += 1;
                let status = self.goal_status().map(str::to_string);
                if status != before {
                    self.log(&format!("[goal] {}", status.as_deref().unwrap_or("cleared")));
                }
                if status.as_deref() != Some("active") {
                    if self.hold.take().is_some() {
                        self.log("[goal] no longer active; settling the turn");
                        self.finish_active_turn();
                    }
                    // The goal stopped instead of starting the awaited turn.
                    if self.active.is_none() {
                        self.release_goal_waiters("(codex started no turn)");
                    }
                }
                return;
            }
            "mcpServer/startupStatus/updated" => {
                // Failed MCP servers explain later tool gaps; the rest of this
                // family is high-frequency noise.
                if params["status"].as_str() == Some("failed") {
                    let name = params["name"].as_str().unwrap_or("?");
                    let error = params["error"].as_str().unwrap_or("unknown error");
                    self.log(&format!("[mcp] {name} failed to start: {error}"));
                }
                return;
            }
            _ => {}
        }

        if !method.starts_with("item/") && method != "turn/completed" {
            const QUIET: [&str; 7] = [
                "thread/started",
                "thread/status/changed",
                "remoteControl/status/changed",
                "turn/diff/updated",
                "turn/plan/updated",
                "model/safetyBuffering/updated",
                "turn/started",
            ];
            if !QUIET.contains(&method) {
                self.log(&format!("[debug] unknown notification {raw}"));
            }
            return;
        }

        if !ours {
            return;
        }
        let Some(turn) = &mut self.active else {
            return;
        };
        let event_turn = params["turnId"].as_str();
        match (event_turn, turn.turn_id.as_deref()) {
            (Some(event_turn), Some(current)) if event_turn != current => return,
            (Some(event_turn), None) => {
                turn.turn_id = Some(event_turn.to_string());
                turn.acc.turn_id = event_turn.to_string();
            }
            _ => {}
        }
        let lines = turn.acc.handle_notification(method, params);
        self.log_turn_header();
        for line in lines {
            self.log(&line);
        }
        if method == "turn/completed" {
            self.complete_active_turn();
        }
    }

    fn log_turn_header(&mut self) {
        let Some(turn) = &mut self.active else {
            return;
        };
        if turn.header_logged || turn.turn_id.is_none() {
            return;
        }
        turn.header_logged = true;
        let header = turn.acc.header_line();
        self.log(&header);
    }

    /// A physical turn ending is not the run ending: while a goal is active,
    /// or after a compaction, codex may start another turn by itself. Hold
    /// the clients and the report until that settles; the timer is a stall
    /// safety net.
    fn complete_active_turn(&mut self) {
        if self.hold.is_some() {
            return;
        }
        let goal_active = self.goal_active();
        let Some(turn) = &mut self.active else {
            return;
        };
        let compacted = turn.acc.compactions > turn.compactions_held;
        if !goal_active && !compacted {
            self.finish_active_turn();
            return;
        }
        turn.compactions_held = turn.acc.compactions;
        let (grace, reason) = if goal_active {
            (env_duration("CEPTION_GOAL_GRACE_MS", 30_000), "goal still active")
        } else {
            (env_duration("CEPTION_CONTINUATION_GRACE_MS", 2_000), "turn compacted")
        };
        self.log(&format!(
            "[turn] {reason}; holding the report up to {}ms for a continuation turn",
            grace.as_millis()
        ));
        let generation = self.next_id();
        self.hold = Some(generation);
        self.after(grace, Event::GraceExpired(generation));
    }

    fn resume_continuation(&mut self, turn_id: Option<String>) {
        self.hold = None;
        let Some(turn) = &mut self.active else {
            return;
        };
        if let Some(turn_id) = turn_id {
            turn.turn_id = Some(turn_id);
        }
        let turn_id = turn.turn_id.clone().unwrap_or_else(|| "unknown".into());
        turn.acc.adopt_continuation(&turn_id);
        self.absorb_goal_waiters();
        self.log(&format!("[turn] continuing in turn {turn_id}"));
    }

    fn adopt_orphan_turn(&mut self, turn_id: Option<String>) {
        let (Some(turn_id), Some(thread_id)) = (turn_id, self.thread_id.clone()) else {
            return;
        };
        if self.shutting_down {
            return;
        }
        let clients: Vec<Client> = self.goal_waiters.drain(..).map(|waiter| waiter.client).collect();
        let goal_pending = self.pending.values().any(|p| matches!(p, Pending::GoalSet { .. }));
        let (prompt, note) = if !clients.is_empty() || goal_pending {
            ("(goal turn)", "[turn] codex started the goal's turn")
        } else {
            (
                "(unattended continuation)",
                "[turn] adopted an unattended continuation turn; attach with `ception watch`",
            )
        };
        let op = self.next_id();
        self.active = Some(ActiveTurn {
            op,
            turn_id: Some(turn_id.clone()),
            acc: TurnAccumulator::new(self.label(), &thread_id, &turn_id, prompt),
            clients,
            header_logged: false,
            compactions_held: 0,
        });
        self.log_turn_header();
        self.log(note);
    }

    fn absorb_goal_waiters(&mut self) {
        if self.goal_waiters.is_empty() {
            return;
        }
        let Some(turn) = &mut self.active else {
            return;
        };
        let count = self.goal_waiters.len();
        turn.clients.extend(self.goal_waiters.drain(..).map(|waiter| waiter.client));
        self.log(&format!("[goal] {count} client(s) attached to the goal's turn"));
    }

    fn release_goal_waiters(&mut self, note: &str) {
        let report = format!("{}\n\n{note}", self.goal_line());
        for waiter in std::mem::take(&mut self.goal_waiters) {
            waiter.client.answer(self.goal_reply("ok", report.clone()));
        }
    }

    fn turn_reply(&self, acc: &TurnAccumulator, level: ReportLevel) -> Reply {
        Reply::Result(Outcome {
            status: acc.status.clone(),
            report: acc.build_report(level),
            goal: self.goal.clone(),
            turn_id: Some(acc.turn_id.clone()),
            daemon: None,
        })
    }

    fn finish_active_turn(&mut self) {
        let Some(turn) = self.active.take() else {
            return;
        };
        self.log(&turn.acc.footer_line());
        for client in turn.clients {
            let reply = self.turn_reply(&turn.acc, client.report);
            client.answer(reply);
        }
        self.last_settled = Some(turn.acc);
        self.turns_settled += 1;
        // With the goal stopped no further turn is coming.
        if !self.goal_active() {
            self.release_goal_waiters("(codex started no turn)");
        }
        self.last_activity = Instant::now();
        self.persist_or_log();
    }

    async fn on_server_request(&mut self, id: Value, method: &str, raw: &Value) {
        let description = format!("codex sent {method} despite approvalPolicy never/danger-full-access; failing turn");
        self.log(&format!("[error] {description}: {raw}"));
        if let Err(error) = self.app.reject(id, method).await {
            self.log(&format!("[error] {error:#}"));
        }
        self.hold = None;
        let turn_id = self.active.as_ref().and_then(|turn| turn.turn_id.clone());
        self.fail_everyone(&description);
        if let (Some(turn_id), Some(thread_id)) = (turn_id, self.thread_id.clone()) {
            let params = json!({ "threadId": thread_id, "turnId": turn_id });
            let pending = Pending::Interrupt { client: None, turn_id, paused: false };
            if let Err(error) = self.request("turn/interrupt", params, pending).await {
                self.log(&format!("[error] interrupt after server request: {error:#}"));
            }
        }
    }

    fn fail_everyone(&mut self, message: &str) {
        self.fail_active(message);
        for waiter in std::mem::take(&mut self.goal_waiters) {
            waiter.client.answer(Reply::error(message));
        }
    }

    /// Best effort and capped: a rejected or wedged interrupt must not keep
    /// the daemon alive. App events keep flowing meanwhile, so the
    /// interrupted turn can still report to its clients; new commands wait in
    /// the queue and are refused once shutdown begins.
    async fn interrupt_for_shutdown(&mut self, app_events: &mut UnboundedReceiver<AppEvent>) {
        let turn_id = self.active.as_ref().and_then(|turn| turn.turn_id.clone());
        let (Some(turn_id), Some(thread_id)) = (turn_id, self.thread_id.clone()) else {
            return;
        };
        let params = json!({ "threadId": thread_id, "turnId": turn_id });
        let Ok(id) = self.app.request("turn/interrupt", params).await else {
            return;
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let event = tokio::select! {
                event = app_events.recv() => event,
                _ = tokio::time::sleep_until(deadline) => return,
            };
            let Some(event) = event else {
                return;
            };
            if let AppEvent::Message(message) = &event {
                if message.get("method").is_none() && message.get("id").and_then(Value::as_u64) == Some(id) {
                    if let Some(error) = message.get("error") {
                        self.log(&format!("[interrupt] {}", error["message"].as_str().unwrap_or("failed")));
                    }
                    return;
                }
            }
            let closed = matches!(event, AppEvent::Closed(_));
            self.on_app(event).await;
            if closed {
                return;
            }
        }
    }

    async fn shutdown(&mut self, reason: &str) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        self.log(&format!("[daemon] shutting down: {reason}"));
        self.hold = None;
        let message = format!("daemon shutting down: {reason}");
        self.fail_everyone(&message);
        for item in std::mem::take(&mut self.deferred) {
            let (Deferred::Command(_, client) | Deferred::Interrupt { client, .. }) = item;
            client.answer(Reply::error(message.clone()));
        }
        for (_, pending) in self.pending.drain() {
            let client = match pending {
                Pending::Steer { client }
                | Pending::GoalSet { client, .. }
                | Pending::GoalGet { client, .. }
                | Pending::GoalClear { client, .. }
                | Pending::InterruptPause { client, .. }
                | Pending::Interrupt { client: Some(client), .. }
                | Pending::ThreadStart { then: AfterThread::Goal { client, .. } } => client,
                _ => continue,
            };
            client.answer(Reply::error(message.clone()));
        }
        self.persist_or_log();
        let _ = std::fs::remove_file(&self.paths.socket);
        self.app.close().await;
    }
}

fn text_input(text: &str) -> Value {
    json!({ "type": "text", "text": text, "text_elements": [] })
}

fn non_null(value: &Value) -> Option<Value> {
    (!value.is_null()).then(|| value.clone())
}

fn goal_action_name(action: GoalAction) -> &'static str {
    match action {
        GoalAction::Set => "set",
        GoalAction::Resume => "resume",
        GoalAction::Pause => "pause",
        GoalAction::Show => "show",
        GoalAction::Clear => "clear",
    }
}

fn format_rate_limits(limits: &Value) -> String {
    let window = |entry: &Value| {
        let Some(used) = entry["usedPercent"].as_f64() else {
            return "n/a".to_string();
        };
        let mins = entry["windowDurationMins"].as_f64().unwrap_or(0.0);
        let span = if mins >= 1440.0 {
            format!("{}d", (mins / 1440.0).round())
        } else {
            format!("{}h", (mins / 60.0).round())
        };
        format!("{used}% of {span}")
    };
    format!("primary {}, secondary {}", window(&limits["primary"]), window(&limits["secondary"]))
}

/// One client connection: read one request, forward it, write the one reply.
/// A client that hangs up while waiting is dropped; its reply goes nowhere.
async fn serve_connection(stream: UnixStream, events: UnboundedSender<Event>) {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let reply = match lines.next_line().await {
        Ok(Some(line)) => match serde_json::from_str::<Request>(&line) {
            Err(error) => Some(Reply::error(format!("invalid command JSON: {error}"))),
            Ok(request) => {
                let (reply_tx, reply_rx) = oneshot::channel();
                let _ = events.send(Event::Command { request, reply: reply_tx });
                tokio::select! {
                    reply = reply_rx => Some(reply.unwrap_or_else(|_| Reply::error("daemon dropped the request"))),
                    _ = lines.next_line() => None,
                }
            }
        },
        _ => None,
    };
    if let Some(reply) = reply {
        if let Ok(mut line) = serde_json::to_vec(&reply) {
            line.push(b'\n');
            let _ = write.write_all(&line).await;
            let _ = write.shutdown().await;
        }
    }
    let _ = events.send(Event::Disconnected);
}
