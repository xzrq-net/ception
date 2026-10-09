//! The per-label daemon: owns one codex app-server and one thread, serves
//! CLI clients over a unix socket.
//!
//! One actor owns all state and handles events in order: app-server output
//! (in stream order, with RPC responses handled at their position as stored
//! continuations, [`Pending`]), client commands, timers, child exits.
//!
//! Processes: the daemon is a child subreaper and the only thing that reaps
//! its children, so everything codex starts stays in its process tree and a
//! pid it lists can't be recycled under it. Teardown signals its children
//! (TERM, then KILL) and reaps them until none are left; orphans the kernel
//! hands it are reached the same way.
//!
//! Turns: a run is a turn plus the continuations codex folds into it. Runs
//! are made only from codex's notifications: the first lifecycle event of an
//! unseen turn starts a run, or continues the current one. Requests never
//! make runs; a client waits on the turn codex names in its turn/start reply
//! (or on the next run, for a goal), and is attached once that turn shows up.

use std::collections::hash_map::RandomState;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::hash::BuildHasher;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::time::Instant;

use crate::appserver::{AppEvent, AppServer, Incoming};
use crate::paths::{LabelPaths, project_hash};
use crate::procfs::{self, PidWatch};
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

/// Settled runs kept for `watch --run`.
pub const RETAINED_RUNS: usize = 16;

/// Teardown sends SIGTERM for this long before switching to SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(2);

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

    // Everything codex starts stays ours: orphans are reparented to us, not
    // to init, and teardown reaches them.
    unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) };

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
        child: signal(SignalKind::child())?,
    };
    match boot(options, paths, &mut log, &mut signals).await {
        Ok(booted) => {
            signal_ready("ready");
            Daemon::new(booted, log).serve(signals).await;
        }
        Err(error) => {
            log.line(&format!("[daemon] startup failed: {error:#}"));
            signal_ready(&format!("error: {error:#}"));
            teardown(None, &mut log).await;
        }
    }
    // Everything we started is gone by now; only then may a successor start.
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

/// Reap every child that has exited, telling the app-server if it was one.
fn reap_children(mut app: Option<&mut AppServer>) {
    loop {
        let mut status = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid <= 0 {
            return;
        }
        if let Some(app) = app.as_deref_mut().filter(|app| app.pid() as libc::pid_t == pid) {
            app.reaped(status);
        }
    }
}

/// Kill and reap the daemon's whole process tree: signal every child, reap
/// what exits, and repeat as the kernel hands over orphans, until no child
/// is left. SIGTERM first, SIGKILL after [`TERM_GRACE`]. Only our own
/// unreaped children are signalled, so nothing unrelated can be hit. Not
/// bounded: the label stays locked until the tree is really gone.
async fn teardown(mut app: Option<&mut AppServer>, log: &mut Log) {
    let me = std::process::id();
    let started = Instant::now();
    let mut termed = HashSet::new();
    let mut reported = 0;
    loop {
        reap_children(app.as_deref_mut());
        let children = procfs::children_of(me);
        if children.is_empty() {
            return;
        }
        let elapsed = started.elapsed();
        for pid in children.iter().copied() {
            if elapsed >= TERM_GRACE {
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            } else if termed.insert(pid) {
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
            }
        }
        if elapsed.as_secs() >= 10 * (reported + 1) {
            reported += 1;
            log.line(&format!("[daemon] still waiting for {} process(es) to exit", children.len()));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn random_hex(bytes: usize) -> String {
    let mut out = String::new();
    while out.len() < bytes * 2 {
        out.push_str(&format!("{:016x}", RandomState::new().hash_one(out.len())));
    }
    out.truncate(bytes * 2);
    out
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
    child: Signal,
}

impl Signals {
    async fn shutdown(&mut self) -> &'static str {
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
/// the watched process dies or a signal arrives (the caller then tears down
/// whatever started).
async fn boot(options: Options, paths: LabelPaths, log: &mut Log, signals: &mut Signals) -> Result<Booted> {
    let watch = match (options.watch_pid, options.watch_starttime) {
        (Some(pid), Some(starttime)) => {
            Some(PidWatch::open(pid, starttime)?.ok_or_else(|| anyhow!("watched process {pid} is gone"))?)
        }
        _ => None,
    };
    let (thread_id, model, effort) = if options.resume {
        let record = store::read(&paths.record)?.ok_or_else(|| anyhow!("no stored thread for {}", options.label))?;
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
            let response = app.call(&mut app_events, "thread/resume", params, |event| backlog.push(event)).await?;
            if let Some(id) = response["thread"]["id"].as_str() {
                thread_id = Some(id.to_string());
            }
        }
        let _ = std::fs::remove_file(&paths.socket);
        let listener =
            UnixListener::bind(&paths.socket).with_context(|| format!("listen on {}", paths.socket.display()))?;
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
        signal = signals.shutdown() => bail!("{signal} during startup"),
    };
    if let Some(thread_id) = &thread_id {
        log.line(&format!("[thread] resumed {thread_id}"));
    }
    log.line(&format!("[daemon] listening {}", paths.socket.display()));
    Ok(Booted { options, paths, app, app_events, backlog, listener, watch, thread_id, model, effort })
}

fn thread_params(project: &Path, model: Option<&str>) -> Value {
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
    reply: UnboundedSender<Reply>,
    report: ReportLevel,
    /// Told which run it is waiting on.
    acked: bool,
}

impl Client {
    /// The final reply.
    fn answer(self, reply: Reply) {
        let _ = self.reply.send(reply);
    }

    fn accept(&mut self, run: &str) {
        if !self.acked {
            self.acked = true;
            let _ = self.reply.send(Reply::Accepted { run: run.to_string() });
        }
    }
}

/// A turn plus the continuation turns codex folds into it.
struct Run {
    op: u64,
    /// Every physical turn of the run, the current one last.
    turns: Vec<String>,
    acc: TurnAccumulator,
    clients: Vec<Client>,
    /// Compactions that already paid a grace hold, so a continuation that
    /// ends clean doesn't wait again.
    compactions_held: u32,
}

impl Run {
    fn current(&self) -> &str {
        self.turns.last().expect("a run has a turn")
    }
}

/// A settled run, kept for `watch --run` and for a turn/start reply that
/// arrives after its turn already ran.
struct Settled {
    op: u64,
    turns: Vec<String>,
    outcome: SettledOutcome,
}

enum SettledOutcome {
    /// With the goal as it stood when the run settled.
    Report { acc: Box<TurnAccumulator>, goal: Option<Value> },
    /// The run ended in an infrastructure failure, as its clients were told.
    Failed(String),
}

/// A client waiting for work that hasn't shown up yet.
struct Waiter {
    client: Client,
    expect: Expect,
}

enum Expect {
    /// The turn codex named in its turn/start reply: the one it started, or
    /// the running one it steered the input into. `steer_run` is the run
    /// that was current when the request went out; landing in it means the
    /// input joined work already under way.
    Turn { id: String, prompt: String, steer_run: Option<u64> },
    /// The next run codex starts, for a goal; `timer` names its timeout.
    NextRun { timer: u64 },
}

/// A request's continuation, run when its response arrives.
enum Pending {
    ThreadStart {
        then: AfterThread,
    },
    TurnStart {
        client: Client,
        prompt: String,
        steer_run: Option<u64>,
    },
    GoalSet {
        client: Client,
        action: GoalAction,
        goal_updates: u64,
        settled_count: u64,
    },
    GoalGet {
        client: Client,
        goal_updates: u64,
    },
    GoalClear {
        client: Client,
        goal_updates: u64,
    },
    /// An interrupt pausing the active goal first, so freeing the thread
    /// doesn't just start the goal's next turn.
    InterruptPause {
        client: Client,
        goal_updates: u64,
    },
    Interrupt {
        client: Option<Client>,
        turn_id: String,
        paused: bool,
    },
}

enum AfterThread {
    Send { client: Client, prompt: String },
    Goal { client: Client, action: GoalAction, objective: Option<String> },
}

/// Work that has to wait: anything needing the thread while it is being
/// created, and the turn half of an interrupt while there is nothing yet to
/// interrupt.
enum Deferred {
    Command(Request, Client),
    Interrupt { client: Client, paused: bool },
}

enum Event {
    Command {
        request: Request,
        reply: UnboundedSender<Reply>,
    },
    /// A connection taken by the refuser, counted so exit waits for its reply.
    Connected,
    Disconnected,
    GraceExpired(u64),
    GoalStartExpired(u64),
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
    /// Prefixes run ids, so a restarted daemon never mistakes an old id for
    /// one of its own.
    generation: String,

    thread_id: Option<String>,
    thread_starting: bool,
    run: Option<Run>,
    /// Generation of the grace timer while a finished turn's report is held
    /// for a continuation.
    hold: Option<u64>,
    /// Anything but an "active" goal means codex starts no more turns.
    goal: Option<Value>,
    /// Goal notifications seen, so a reply can tell it is older than them.
    goal_updates: u64,
    waiters: Vec<Waiter>,
    pending: HashMap<u64, Pending>,
    deferred: VecDeque<Deferred>,
    settled_count: u64,
    /// Recently settled runs, newest last.
    settled: VecDeque<Settled>,
    /// Every turn seen to end, retained or not: never tracked again.
    finished_turns: HashSet<String>,

    next_id: u64,
    next_run: u64,
    connections: usize,
    last_activity: Instant,
    last_rate_line: String,
    shutdown_request: Option<String>,
    /// Shutdown has begun: new commands are refused.
    stopping: bool,
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
            generation: random_hex(4),
            thread_id: booted.thread_id,
            thread_starting: false,
            run: None,
            hold: None,
            goal: None,
            goal_updates: 0,
            waiters: Vec::new(),
            pending: HashMap::new(),
            deferred: VecDeque::new(),
            settled_count: 0,
            settled: VecDeque::new(),
            finished_turns: HashSet::new(),
            next_id: 0,
            next_run: 0,
            connections: 0,
            last_activity: Instant::now(),
            last_rate_line: String::new(),
            shutdown_request: None,
            stopping: false,
            shutting_down: false,
        }
    }

    async fn serve(mut self, mut signals: Signals) {
        let mut app_events = self.app_events.take().expect("serve runs once");
        let mut events = self.event_rx.take().expect("serve runs once");
        let mut listener = self.listener.take();
        let watch = self.watch.take();
        if self.thread_id.is_some()
            && let Err(error) = self.persist()
        {
            self.log(&format!("[state] {error:#}; shutting down"));
            self.shutdown_request = Some("label not persisted".into());
        }
        for event in std::mem::take(&mut self.backlog) {
            self.on_app(event).await;
        }
        let idle = Duration::from_secs(
            std::env::var("CEPTION_IDLE_TIMEOUT_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(4 * 60 * 60),
        );

        while self.shutdown_request.is_none() {
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
                accepted = listener.as_ref().expect("held until shutdown").accept() => {
                    if let Ok((stream, _)) = accepted {
                        self.connections += 1;
                        tokio::spawn(serve_connection(stream, self.events.clone()));
                    }
                }
                _ = signals.child.recv() => reap_children(Some(&mut self.app)),
                _ = watch_exit => {
                    let pid = self.options.watch_pid.unwrap_or_default();
                    self.shutdown_request = Some(format!("watched process {pid} exited"));
                }
                _ = tokio::time::sleep_until(idle_at.unwrap_or_else(Instant::now)), if idle_at.is_some() => {
                    self.shutdown_request = Some("idle timeout".into());
                }
                _ = signals.term.recv() => self.shutdown_request = Some("SIGTERM".into()),
                _ = signals.int.recv() => self.shutdown_request = Some("SIGINT".into()),
            }
            self.resolve_waiters();
            self.drain_deferred().await;
            self.ack_run();
        }

        let reason = self.shutdown_request.take().expect("loop exits on a request");
        self.stopping = true;
        // Connections keep being answered, with Refused, until we exit.
        if let Some(listener) = listener.take() {
            tokio::spawn(refuse_connections(listener, self.events.clone()));
        }
        self.interrupt_for_shutdown(&mut app_events, &mut events).await;
        self.shutdown(&reason).await;
        self.drain_connections(&mut events).await;
    }

    /// Replies are written by connection tasks; give them a moment before
    /// the process exits under them, including whatever the refuser is
    /// still taking from the listen backlog.
    async fn drain_connections(&mut self, events: &mut UnboundedReceiver<Event>) {
        let backlog = Instant::now() + Duration::from_millis(100);
        let deadline = Instant::now() + Duration::from_secs(1);
        while self.connections > 0 || Instant::now() < backlog {
            let wake = if self.connections > 0 { deadline } else { backlog };
            tokio::select! {
                Some(event) = events.recv() => self.on_event(event).await,
                _ = tokio::time::sleep_until(wake) => {
                    if wake == deadline {
                        return;
                    }
                }
            }
        }
    }

    /// Tell the clients of the current run which run they are waiting on.
    fn ack_run(&mut self) {
        let Some(op) = self.run.as_ref().map(|run| run.op) else {
            return;
        };
        let name = self.run_name(op);
        for client in &mut self.run.as_mut().expect("checked").clients {
            client.accept(&name);
        }
    }

    fn idle_eligible(&self) -> bool {
        self.run.is_none()
            && self.hold.is_none()
            && self.connections == 0
            && self.waiters.is_empty()
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

    fn next_run(&mut self) -> u64 {
        self.next_run += 1;
        self.next_run
    }

    fn run_name(&self, op: u64) -> String {
        format!("{}.{op}", self.generation)
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

    /// A requested turn is on its way: a turn/start awaiting its reply, or
    /// a reply naming a turn not seen yet.
    fn start_in_flight(&self) -> bool {
        self.pending.values().any(|pending| matches!(pending, Pending::TurnStart { .. }))
            || self.waiters.iter().any(|waiter| matches!(waiter.expect, Expect::Turn { .. }))
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

    fn request(&mut self, method: &str, params: Value, pending: Pending) -> Result<()> {
        let id = self.app.request(method, params)?;
        self.pending.insert(id, pending);
        Ok(())
    }

    fn settle(&mut self, op: u64, turns: Vec<String>, outcome: SettledOutcome) {
        self.finished_turns.extend(turns.iter().cloned());
        self.settled.push_back(Settled { op, turns, outcome });
        if self.settled.len() > RETAINED_RUNS {
            self.settled.pop_front();
        }
        self.settled_count += 1;
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
                    Request::Send { report, .. } | Request::Goal { report, .. } | Request::Watch { report, .. } => {
                        *report
                    }
                    _ => ReportLevel::Brief,
                };
                let client = Client { reply, report, acked: false };
                if self.stopping || self.shutting_down {
                    client.answer(Reply::Refused { message: "daemon shutting down".into() });
                    return;
                }
                let item = Deferred::Command(request, client);
                if self.must_defer(&item) {
                    self.deferred.push_back(item);
                } else {
                    self.dispatch(item).await;
                }
            }
            Event::Connected => self.connections += 1,
            Event::Disconnected => self.connections = self.connections.saturating_sub(1),
            Event::GraceExpired(generation) => {
                if self.hold == Some(generation) {
                    self.hold = None;
                    self.log("[turn] no continuation arrived; settling");
                    self.finish_run();
                }
            }
            Event::GoalStartExpired(timer) => {
                let index = self
                    .waiters
                    .iter()
                    .position(|waiter| matches!(waiter.expect, Expect::NextRun { timer: t } if t == timer));
                if let Some(index) = index {
                    let waiter = self.waiters.remove(index);
                    let ms = env_duration("CEPTION_GOAL_START_MS", 30_000).as_millis();
                    self.log(&format!("[goal] no turn started within {ms}ms"));
                    let report = format!("{}\n\n(codex started no turn within {ms}ms)", self.goal_line());
                    waiter.client.answer(self.goal_reply("ok", report));
                }
            }
        }
    }

    fn must_defer(&self, item: &Deferred) -> bool {
        let creating = self.thread_starting;
        match item {
            // While a run is held, codex may yet continue it: wait to see, so
            // the input steers the continuation instead of starting new work
            // that would fold into the held run.
            Deferred::Command(Request::Send { .. }, _) => creating || self.hold.is_some(),
            Deferred::Command(Request::Interrupt, _) => creating,
            Deferred::Command(Request::Goal { action, .. }, _) => creating && *action != GoalAction::Show,
            Deferred::Command(..) => false,
            // Nothing to interrupt yet while a requested turn is on its way,
            // or while a compaction hold decides whether a continuation comes.
            Deferred::Interrupt { .. } => {
                creating
                    || (self.run.is_none() && self.start_in_flight())
                    || (self.hold.is_some() && !self.goal_active())
            }
        }
    }

    async fn drain_deferred(&mut self) {
        while !self.stopping {
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
            Deferred::Interrupt { client, paused } => self.interrupt_current(client, paused),
            Deferred::Command(request, client) => match request {
                Request::Status => {
                    client.answer(self.status_reply());
                    Ok(())
                }
                Request::Send { prompt, .. } => self.send(prompt, client),
                Request::Goal { action, objective, .. } => self.goal_command(action, objective, client),
                Request::Interrupt => self.interrupt(client),
                Request::Watch { run, .. } => {
                    self.watch(run, client);
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
        let running = self.run.is_some() || self.start_in_flight();
        Reply::Result(Outcome {
            status: "ok".into(),
            daemon: Some(DaemonStatus {
                pid: std::process::id(),
                thread_id: self.thread_id.clone(),
                turn_id: self.run.as_ref().map(|run| run.current().to_string()),
                state: if running { "active" } else { "idle" }.into(),
                goal: self.goal.clone(),
                log: self.paths.log.clone(),
            }),
            ..Default::default()
        })
    }

    fn goal_reply(&self, status: &str, report: String) -> Reply {
        Reply::Result(Outcome { status: status.into(), report, goal: self.goal.clone(), ..Default::default() })
    }

    /// Attach to the current run, or to a named run: live, or settled and
    /// retained.
    fn watch(&mut self, run: Option<String>, client: Client) {
        let Some(name) = run else {
            match &mut self.run {
                Some(run) => run.clients.push(client),
                None => client.answer(Reply::ok("idle", "no active turn")),
            }
            return;
        };
        let op = name.strip_prefix(&format!("{}.", self.generation)).and_then(|op| op.parse::<u64>().ok());
        let Some(op) = op else {
            client.answer(Reply::error(format!(
                "run {name} is not from this daemon (it restarted since); its report is gone"
            )));
            return;
        };
        if let Some(run) = self.run.as_mut().filter(|run| run.op == op) {
            run.clients.push(client);
            return;
        }
        let reply = match self.settled.iter().find(|settled| settled.op == op) {
            Some(settled) => settled_reply(&settled.outcome, client.report),
            None => Reply::error(format!("run {name} is not retained (only the last {RETAINED_RUNS} are)")),
        };
        client.answer(reply);
    }

    /// Codex decides whether the input starts a turn or steers the running
    /// one; its reply names the turn, and the client waits on that.
    fn send(&mut self, prompt: String, client: Client) -> Result<()> {
        if self.thread_id.is_none() {
            return self.start_thread(AfterThread::Send { client, prompt });
        }
        let mut params = json!({
            "threadId": self.thread_id,
            "input": [text_input(&prompt)],
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
        let steer_run = self.run.as_ref().map(|run| run.op);
        self.request("turn/start", params, Pending::TurnStart { client, prompt, steer_run })
    }

    fn start_thread(&mut self, then: AfterThread) -> Result<()> {
        self.thread_starting = true;
        let params = thread_params(&self.options.project, self.model.as_deref());
        match self.app.request("thread/start", params) {
            Ok(id) => {
                self.pending.insert(id, Pending::ThreadStart { then });
                Ok(())
            }
            Err(error) => {
                self.thread_starting = false;
                then.fail(&format!("{error:#}"));
                Err(error)
            }
        }
    }

    fn goal_command(&mut self, action: GoalAction, objective: Option<String>, client: Client) -> Result<()> {
        let Some(thread_id) = self.thread_id.clone() else {
            return match action {
                GoalAction::Show | GoalAction::Clear => {
                    client.answer(self.goal_reply("ok", self.goal_line()));
                    Ok(())
                }
                _ => self.start_thread(AfterThread::Goal { client, action, objective }),
            };
        };
        let goal_updates = self.goal_updates;
        match action {
            GoalAction::Show => {
                let pending = Pending::GoalGet { client, goal_updates };
                self.request("thread/goal/get", json!({ "threadId": thread_id }), pending)
            }
            GoalAction::Clear => {
                let pending = Pending::GoalClear { client, goal_updates };
                self.request("thread/goal/clear", json!({ "threadId": thread_id }), pending)
            }
            GoalAction::Set | GoalAction::Resume | GoalAction::Pause => {
                let status = if action == GoalAction::Pause { "paused" } else { "active" };
                let mut params = json!({ "threadId": thread_id, "status": status });
                if let Some(objective) = &objective {
                    params["objective"] = json!(objective);
                }
                let settled_count = self.settled_count;
                let pending = Pending::GoalSet { client, action, goal_updates, settled_count };
                self.request("thread/goal/set", params, pending)?;
                if let Some(objective) = &objective {
                    self.log(&format!("[goal] objective: {objective}"));
                }
                Ok(())
            }
        }
    }

    fn interrupt(&mut self, client: Client) -> Result<()> {
        // Paused even with no turn running: a held run or a goal about to
        // start its next turn must stop too.
        if self.goal_active()
            && let Some(thread_id) = self.thread_id.clone()
        {
            let params = json!({ "threadId": thread_id, "status": "paused" });
            let pending = Pending::InterruptPause { client, goal_updates: self.goal_updates };
            return self.request("thread/goal/set", params, pending);
        }
        self.interrupt_current(client, false)
    }

    /// Interrupt whatever turn is running now, which after a goal pause may
    /// be a continuation of the one the interrupt was aimed at.
    fn interrupt_current(&mut self, client: Client, paused: bool) -> Result<()> {
        let held_back = Deferred::Interrupt { client, paused };
        if self.must_defer(&held_back) {
            self.deferred.push_back(held_back);
            return Ok(());
        }
        let Deferred::Interrupt { client, paused } = held_back else { unreachable!() };
        let running = self.run.as_ref().filter(|_| self.hold.is_none()).map(|run| run.current().to_string());
        let (Some(turn_id), Some(thread_id)) = (running, self.thread_id.clone()) else {
            let report = if paused { format!("no active turn\n{}", self.goal_line()) } else { "no active turn".into() };
            client.answer(self.goal_reply("idle", report));
            return Ok(());
        };
        let params = json!({ "threadId": thread_id, "turnId": turn_id });
        let pending = Pending::Interrupt { client: Some(client), turn_id, paused };
        self.request("turn/interrupt", params, pending)
    }

    // ----- app-server ------------------------------------------------------

    async fn on_app(&mut self, event: AppEvent) {
        match event {
            AppEvent::Stderr(line) => self.log(&format!("[app-server stderr] {line}")),
            AppEvent::Closed(reason) => {
                if self.stopping {
                    return;
                }
                // Reap it first, so the report says how it ended.
                let deadline = Instant::now() + Duration::from_secs(2);
                while !self.app.has_exited() && reason.is_none() && Instant::now() < deadline {
                    reap_children(Some(&mut self.app));
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                let message = self.app.describe_exit(reason).await;
                self.log(&format!("[app-server] {message}"));
                self.hold = None;
                self.fail_everyone(&message);
                self.fail_pending(&message);
                self.shutdown_request = Some("app-server exit".into());
            }
            AppEvent::Message(message) => match Incoming::classify(message.clone()) {
                Some(Incoming::Notification { method, params }) => self.on_notification(&method, &params, &message),
                Some(Incoming::Response { id, outcome }) => {
                    if let Some(pending) = self.pending.remove(&id)
                        && let Err(error) = self.on_response(pending, outcome)
                    {
                        self.log(&format!("[error] {error:#}"));
                    }
                }
                Some(Incoming::Request { id, method }) => self.on_server_request(id, &method, &message),
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

    fn on_response(&mut self, pending: Pending, outcome: std::result::Result<Value, String>) -> Result<()> {
        match pending {
            Pending::ThreadStart { then } => {
                self.thread_starting = false;
                let thread_id = match outcome {
                    Ok(response) => response["thread"]["id"].as_str().map(str::to_string),
                    Err(error) => {
                        then.fail(&error);
                        return Ok(());
                    }
                };
                let Some(thread_id) = thread_id else {
                    then.fail("thread/start returned no thread id");
                    return Ok(());
                };
                self.thread_id = Some(thread_id.clone());
                if let Err(error) = self.persist() {
                    // Unrecorded, the thread could never be resumed or listed;
                    // fail the client and go away rather than linger.
                    self.thread_id = None;
                    let message = format!("{error:#}");
                    self.log(&format!("[state] {message}; shutting down"));
                    then.fail(&message);
                    self.shutdown_request = Some("label not persisted".into());
                    return Ok(());
                }
                self.log(&format!("[thread] started {thread_id}"));
                match then {
                    AfterThread::Send { client, prompt } => self.send(prompt, client),
                    AfterThread::Goal { client, action, objective } => self.goal_command(action, objective, client),
                }
            }
            Pending::TurnStart { client, prompt, steer_run } => {
                let turn_id = match outcome {
                    Ok(response) => response["turn"]["id"].as_str().map(str::to_string),
                    Err(error) => {
                        client.answer(Reply::error(error));
                        return Ok(());
                    }
                };
                match turn_id {
                    Some(id) => self.waiters.push(Waiter { client, expect: Expect::Turn { id, prompt, steer_run } }),
                    None => client.answer(Reply::error("turn/start returned no turn id")),
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
            Pending::GoalSet { client, action, goal_updates, settled_count } => {
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
                    self.attach_goal_client(client, settled_count);
                }
                Ok(())
            }
            Pending::InterruptPause { client, goal_updates } => match outcome {
                Ok(response) => {
                    self.take_goal_snapshot(&response, goal_updates);
                    self.log("[goal] paused so the interrupt ends the run");
                    self.interrupt_current(client, true)
                }
                Err(error) => {
                    client.answer(Reply::error(error));
                    Ok(())
                }
            },
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

    /// Where a goal client waits for the goal's work: a run that settled
    /// before the reply arrived, the current run (codex folds a goal set
    /// mid-turn into it), or the next run codex starts.
    fn attach_goal_client(&mut self, client: Client, settled_before: u64) {
        let goal_active = self.goal_active();
        if self.settled_count > settled_before
            && !goal_active
            && let Some(settled) = self.settled.back()
        {
            let reply = match &settled.outcome {
                SettledOutcome::Report { acc, .. } => turn_reply(acc, self.goal.clone(), client.report),
                SettledOutcome::Failed(message) => Reply::error(message.clone()),
            };
            self.log("[goal] the goal's turn ran and stopped while it was being set");
            client.answer(reply);
            return;
        }
        if let Some(run) = &mut self.run {
            run.clients.push(client);
            return;
        }
        if !goal_active {
            let report = format!("{}\n\n(codex started no turn)", self.goal_line());
            client.answer(self.goal_reply("ok", report));
            return;
        }
        let timer = self.next_id();
        self.waiters.push(Waiter { client, expect: Expect::NextRun { timer } });
        self.after(env_duration("CEPTION_GOAL_START_MS", 30_000), Event::GoalStartExpired(timer));
    }

    /// Settle waiters whose turn has shown up: join it if it is in the
    /// current run (or answer at once if the input steered work already
    /// under way), or answer from its outcome if it already ended.
    fn resolve_waiters(&mut self) {
        for waiter in std::mem::take(&mut self.waiters) {
            let Expect::Turn { id, steer_run, .. } = &waiter.expect else {
                self.waiters.push(waiter);
                continue;
            };
            if let Some(run) = self.run.as_mut().filter(|run| run.turns.contains(id)) {
                if *steer_run == Some(run.op) {
                    let reply = Reply::ok("steered", format!("steered active turn {id}"));
                    waiter.client.answer(reply);
                } else {
                    run.clients.push(waiter.client);
                }
            } else if let Some(settled) = self.settled.iter().find(|settled| settled.turns.contains(id)) {
                let reply = settled_reply(&settled.outcome, waiter.client.report);
                waiter.client.answer(reply);
            } else if self.finished_turns.contains(id) {
                let message = format!("turn {id} finished before codex confirmed it; its report is in the log");
                waiter.client.answer(Reply::error(message));
            } else {
                self.waiters.push(waiter);
            }
        }
    }

    fn on_notification(&mut self, method: &str, params: &Value, raw: &Value) {
        let ours = self.thread_id.is_some() && params["threadId"].as_str() == self.thread_id.as_deref();
        match method {
            "thread/tokenUsage/updated" => {
                if let Some(run) = &mut self.run
                    && ours
                    && params["turnId"].as_str() == Some(run.current())
                {
                    run.acc.handle_notification(method, params);
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
                if let Some(id) = params["turn"]["id"].as_str() {
                    self.observe_turn(id);
                }
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
                        self.finish_run();
                    }
                    // The goal stopped instead of starting the awaited run.
                    if self.run.is_none() {
                        self.release_next_run_waiters("(codex started no turn)");
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
        // Items name their turn in turnId; turn/completed in turn.id.
        let Some(id) = params["turnId"].as_str().or_else(|| params["turn"]["id"].as_str()) else {
            return;
        };
        if method == "turn/completed" && self.run.as_ref().is_some_and(|run| !run.turns.iter().any(|turn| turn == id)) {
            // The end of a turn we never saw run, while another run is
            // current (an aborted turn's start event can go undelivered).
            // Record its outcome on its own; the current run is untouched.
            if !self.finished_turns.contains(id) {
                self.settle_unseen_turn(id, params);
            }
            return;
        }
        if !self.observe_turn(id) {
            return;
        }
        let run = self.run.as_mut().expect("observed");
        let lines = run.acc.handle_notification(method, params);
        for line in lines {
            self.log(&line);
        }
        if method == "turn/completed" {
            self.complete_run();
        }
    }

    /// Account for a lifecycle event of turn `id`: the first one of an unseen
    /// turn starts a run, or continues the current one (one thread runs one
    /// turn, so a new turn means the last one is over). False for a turn that
    /// is already over.
    fn observe_turn(&mut self, id: &str) -> bool {
        if self.finished_turns.contains(id) {
            return false;
        }
        match &self.run {
            Some(run) if run.current() == id => return true,
            Some(run) if run.turns.iter().any(|turn| turn == id) => return false,
            Some(_) => self.continue_run(id),
            None => self.start_run(id),
        }
        true
    }

    fn start_run(&mut self, id: &str) {
        let thread_id = self.thread_id.clone().unwrap_or_default();
        let requested = self.waiters.iter().find_map(|waiter| match &waiter.expect {
            Expect::Turn { id: turn, prompt, .. } if turn == id => Some(prompt.clone()),
            _ => None,
        });
        let clients = self.take_next_run_waiters();
        let goal_pending = self.pending.values().any(|pending| matches!(pending, Pending::GoalSet { .. }));
        let (prompt, note) = match requested {
            Some(prompt) => (prompt, None),
            None if self.start_in_flight() => ("(turn started)".to_string(), None),
            None if !clients.is_empty() || goal_pending => {
                ("(goal turn)".to_string(), Some("[turn] codex started the goal's turn"))
            }
            None => (
                "(unattended continuation)".to_string(),
                Some("[turn] adopted an unattended continuation turn; attach with `ception watch`"),
            ),
        };
        let op = self.next_run();
        let acc = TurnAccumulator::new(self.label(), &thread_id, id, &prompt);
        self.log(&acc.header_line());
        if let Some(note) = note {
            self.log(note);
        }
        self.run = Some(Run { op, turns: vec![id.to_string()], acc, clients, compactions_held: 0 });
        self.resolve_waiters();
    }

    fn continue_run(&mut self, id: &str) {
        self.hold = None;
        let clients = self.take_next_run_waiters();
        let run = self.run.as_mut().expect("continuing a run");
        run.turns.push(id.to_string());
        run.acc.adopt_continuation(id);
        run.clients.extend(clients);
        self.log(&format!("[turn] continuing in turn {id}"));
        self.resolve_waiters();
    }

    /// A completion for a turn never seen running, while another run is
    /// current: its outcome stands alone, for whoever waits on that turn.
    fn settle_unseen_turn(&mut self, id: &str, params: &Value) {
        let thread_id = self.thread_id.clone().unwrap_or_default();
        let mut acc = TurnAccumulator::new(self.label(), &thread_id, id, "(unseen turn)");
        acc.handle_notification("turn/completed", params);
        acc.settle();
        self.log(&format!("[turn] {id} ended without being seen to start ({})", acc.status));
        let op = self.next_run();
        let outcome = SettledOutcome::Report { acc: Box::new(acc), goal: self.goal.clone() };
        self.settle(op, vec![id.to_string()], outcome);
        self.resolve_waiters();
    }

    fn take_next_run_waiters(&mut self) -> Vec<Client> {
        let (next, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.waiters)
            .into_iter()
            .partition(|waiter| matches!(waiter.expect, Expect::NextRun { .. }));
        self.waiters = rest;
        if !next.is_empty() {
            self.log(&format!("[goal] {} client(s) attached to the goal's turn", next.len()));
        }
        next.into_iter().map(|waiter| waiter.client).collect()
    }

    fn release_next_run_waiters(&mut self, note: &str) {
        let report = format!("{}\n\n{note}", self.goal_line());
        let (next, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.waiters)
            .into_iter()
            .partition(|waiter| matches!(waiter.expect, Expect::NextRun { .. }));
        self.waiters = rest;
        for waiter in next {
            waiter.client.answer(self.goal_reply("ok", report.clone()));
        }
    }

    /// A physical turn ending is not the run ending: while a goal is active,
    /// or after a compaction, codex may start another turn by itself. Hold
    /// the clients and the report until that settles; the timer is a stall
    /// safety net.
    fn complete_run(&mut self) {
        if self.hold.is_some() {
            return;
        }
        let goal_active = self.goal_active();
        let Some(run) = &mut self.run else {
            return;
        };
        let compacted = run.acc.compactions > run.compactions_held;
        if !goal_active && !compacted {
            self.finish_run();
            return;
        }
        run.compactions_held = run.acc.compactions;
        let (grace, reason) = if goal_active {
            (env_duration("CEPTION_GOAL_GRACE_MS", 30_000), "goal still active")
        } else {
            (env_duration("CEPTION_CONTINUATION_GRACE_MS", 2_000), "turn compacted")
        };
        self.log(&format!("[turn] {reason}; holding the report up to {}ms for a continuation turn", grace.as_millis()));
        let generation = self.next_id();
        self.hold = Some(generation);
        self.after(grace, Event::GraceExpired(generation));
    }

    fn finish_run(&mut self) {
        let Some(mut run) = self.run.take() else {
            return;
        };
        run.acc.settle();
        self.log(&run.acc.footer_line());
        for client in run.clients {
            let reply = turn_reply(&run.acc, self.goal.clone(), client.report);
            client.answer(reply);
        }
        let outcome = SettledOutcome::Report { acc: Box::new(run.acc), goal: self.goal.clone() };
        self.settle(run.op, run.turns, outcome);
        // With the goal stopped no further run is coming.
        if !self.goal_active() {
            self.release_next_run_waiters("(codex started no turn)");
        }
        self.last_activity = Instant::now();
        self.persist_or_log();
        self.resolve_waiters();
    }

    fn on_server_request(&mut self, id: Value, method: &str, raw: &Value) {
        let description = format!("codex sent {method} despite approvalPolicy never/danger-full-access; failing turn");
        self.log(&format!("[error] {description}: {raw}"));
        if let Err(error) = self.app.reject(id, method) {
            self.log(&format!("[error] {error:#}"));
        }
        self.hold = None;
        let turn_id = self.run.as_ref().map(|run| run.current().to_string());
        self.fail_everyone(&description);
        if let (Some(turn_id), Some(thread_id)) = (turn_id, self.thread_id.clone()) {
            let params = json!({ "threadId": thread_id, "turnId": turn_id });
            let pending = Pending::Interrupt { client: None, turn_id, paused: false };
            if let Err(error) = self.request("turn/interrupt", params, pending) {
                self.log(&format!("[error] interrupt after server request: {error:#}"));
            }
        }
    }

    /// The current run is lost and nothing awaited will arrive: tell everyone
    /// waiting, and keep the run's failure for `watch --run`, since its
    /// clients were told its id.
    fn fail_everyone(&mut self, message: &str) {
        if let Some(run) = self.run.take() {
            for client in run.clients {
                client.answer(Reply::error(message));
            }
            self.settle(run.op, run.turns, SettledOutcome::Failed(message.to_string()));
        }
        for waiter in std::mem::take(&mut self.waiters) {
            waiter.client.answer(Reply::error(message));
        }
    }

    /// Requests that will never be answered: tell their clients.
    fn fail_pending(&mut self, message: &str) {
        for (_, pending) in self.pending.drain() {
            let client = match pending {
                Pending::TurnStart { client, .. }
                | Pending::GoalSet { client, .. }
                | Pending::GoalGet { client, .. }
                | Pending::GoalClear { client, .. }
                | Pending::InterruptPause { client, .. }
                | Pending::Interrupt { client: Some(client), .. } => client,
                Pending::ThreadStart { then } => {
                    then.fail(message);
                    continue;
                }
                Pending::Interrupt { client: None, .. } => continue,
            };
            client.answer(Reply::error(message));
        }
    }

    /// Best effort and capped: a rejected or wedged interrupt must not keep
    /// the daemon alive. App events keep flowing meanwhile, so the
    /// interrupted turn can still report to its clients; new commands are
    /// refused, which sends a client back to wait for the label lock.
    async fn interrupt_for_shutdown(
        &mut self,
        app_events: &mut UnboundedReceiver<AppEvent>,
        events: &mut UnboundedReceiver<Event>,
    ) {
        let running = self.run.as_ref().map(|run| run.current().to_string());
        let (Some(turn_id), Some(thread_id)) = (running, self.thread_id.clone()) else {
            return;
        };
        let Ok(id) = self.app.request("turn/interrupt", json!({ "threadId": thread_id, "turnId": turn_id })) else {
            return;
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let event = tokio::select! {
                event = app_events.recv() => event,
                Some(event) = events.recv() => {
                    self.on_event(event).await;
                    continue;
                }
                _ = tokio::time::sleep_until(deadline) => return,
            };
            let Some(event) = event else {
                return;
            };
            if let AppEvent::Message(message) = &event
                && message.get("method").is_none()
                && message.get("id").and_then(Value::as_u64) == Some(id)
            {
                if let Some(error) = message.get("error") {
                    self.log(&format!("[interrupt] {}", error["message"].as_str().unwrap_or("failed")));
                }
                return;
            }
            let closed = matches!(event, AppEvent::Closed(_));
            self.on_app(event).await;
            if closed {
                return;
            }
        }
    }

    async fn shutdown(&mut self, reason: &str) {
        self.shutting_down = true;
        self.log(&format!("[daemon] shutting down: {reason}"));
        self.hold = None;
        let message = format!("daemon shutting down: {reason}");
        self.fail_everyone(&message);
        for item in std::mem::take(&mut self.deferred) {
            let (Deferred::Command(_, client) | Deferred::Interrupt { client, .. }) = item;
            client.answer(Reply::Refused { message: message.clone() });
        }
        self.fail_pending(&message);
        self.persist_or_log();
        let _ = std::fs::remove_file(&self.paths.socket);
        // A well-behaved server exits on stdin EOF; give it a moment before
        // tearing the tree down.
        self.app.close_stdin();
        let deadline = Instant::now() + Duration::from_millis(100);
        while !self.app.has_exited() && Instant::now() < deadline {
            reap_children(Some(&mut self.app));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        teardown(Some(&mut self.app), &mut self.log).await;
    }
}

impl AfterThread {
    fn fail(self, message: &str) {
        let (AfterThread::Send { client, .. } | AfterThread::Goal { client, .. }) = self;
        client.answer(Reply::error(message));
    }
}

fn settled_reply(outcome: &SettledOutcome, level: ReportLevel) -> Reply {
    match outcome {
        SettledOutcome::Report { acc, goal } => turn_reply(acc, goal.clone(), level),
        SettledOutcome::Failed(message) => Reply::error(message.clone()),
    }
}

fn turn_reply(acc: &TurnAccumulator, goal: Option<Value>, level: ReportLevel) -> Reply {
    Reply::Result(Outcome {
        status: acc.status.clone(),
        report: acc.build_report(level),
        goal,
        turn_id: Some(acc.turn_id.clone()),
        daemon: None,
    })
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

/// After shutdown begins: answer every new connection with Refused, so a
/// client that got in just then retries elsewhere instead of hanging.
async fn refuse_connections(listener: UnixListener, events: UnboundedSender<Event>) {
    while let Ok((stream, _)) = listener.accept().await {
        let _ = events.send(Event::Connected);
        let events = events.clone();
        tokio::spawn(async move {
            let (read, mut write) = stream.into_split();
            let _ = BufReader::new(read).lines().next_line().await;
            let refusal = Reply::Refused { message: "daemon shutting down".into() };
            if let Ok(mut line) = serde_json::to_vec(&refusal) {
                line.push(b'\n');
                let _ = write.write_all(&line).await;
            }
            let _ = events.send(Event::Disconnected);
        });
    }
}

/// One client connection: read one request, forward it, write its replies
/// until the final one. A client that hangs up while waiting is dropped; its
/// replies go nowhere.
async fn serve_connection(stream: UnixStream, events: UnboundedSender<Event>) {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let (reply_tx, mut replies) = unbounded_channel();
    match lines.next_line().await {
        Ok(Some(line)) => match serde_json::from_str::<Request>(&line) {
            Err(error) => {
                let _ = reply_tx.send(Reply::error(format!("invalid command JSON: {error}")));
            }
            Ok(request) => {
                let _ = events.send(Event::Command { request, reply: reply_tx });
            }
        },
        _ => {
            let _ = events.send(Event::Disconnected);
            return;
        }
    }
    loop {
        let reply = tokio::select! {
            reply = replies.recv() => reply.unwrap_or_else(|| Reply::error("daemon dropped the request")),
            _ = lines.next_line() => break,
        };
        let last = !matches!(reply, Reply::Accepted { .. });
        let Ok(mut line) = serde_json::to_vec(&reply) else {
            break;
        };
        line.push(b'\n');
        if write.write_all(&line).await.is_err() || last {
            let _ = write.shutdown().await;
            break;
        }
    }
    let _ = events.send(Event::Disconnected);
}
