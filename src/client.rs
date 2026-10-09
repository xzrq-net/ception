//! CLI commands: resolve the label's scope, reach (or start) its daemon,
//! relay the reply.

use std::io::{IsTerminal, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::Instant;

use crate::daemon::{self, spawn_timeout};
use crate::paths::{self, LabelPaths, project_hash, validate_name};
use crate::proto::{DaemonStatus, GoalAction, Reply, Request};
use crate::render::{format_goal, status_exit_code};
use crate::session::{self, Session};
use crate::store;
use crate::{AtArgs, Command, WaitArgs, procfs, quota};

pub async fn run(command: Command) -> Result<u8> {
    match command {
        Command::Spawn { label, model, effort, wait, prompt } => spawn(&label, model, effort, &wait, prompt).await,
        Command::Send { label, wait, prompt } => send(&label, &wait, prompt).await,
        Command::Goal { label, resume, pause, show, clear, wait, objective } => {
            let action = match (resume, pause, show, clear) {
                (true, ..) => GoalAction::Resume,
                (_, true, ..) => GoalAction::Pause,
                (_, _, true, _) => GoalAction::Show,
                (.., true) => GoalAction::Clear,
                _ => GoalAction::Set,
            };
            goal(&label, action, &wait, objective).await
        }
        Command::Interrupt { label, at } => interrupt(&label, &at).await,
        Command::Kill { label, all, at } => kill(label.as_deref(), all, &at).await,
        Command::List { all, json, at } => list(all, json, &at).await,
        Command::Quota { json, at } => {
            quota::run(&at.dir()?, json).await?;
            Ok(0)
        }
        Command::Watch { label, follow, run, wait } => watch(&label, follow, run, &wait).await,
        Command::Skill => {
            print!("{}", include_str!("../SKILL.md"));
            Ok(0)
        }
        Command::Daemon(options) => {
            daemon::run(options).await?;
            Ok(0)
        }
    }
}

impl WaitArgs {
    /// Fixed when the command starts, so daemon startup counts against it.
    fn deadline(&self) -> Option<Instant> {
        self.timeout.map(|secs| Instant::now() + Duration::from_secs(secs))
    }
}

impl AtArgs {
    fn dir(&self) -> Result<PathBuf> {
        match &self.cwd {
            Some(dir) => Ok(dir.clone()),
            None => std::env::current_dir().context("current directory"),
        }
    }
}

/// The project and session a command runs in.
struct Scope {
    project: PathBuf,
    hash: String,
    session: Session,
}

impl Scope {
    fn resolve(at: &AtArgs) -> Result<Self> {
        let project = paths::project_root(&at.dir()?)?;
        let hash = project_hash(&project);
        Ok(Self { project, hash, session: session::from_env()? })
    }

    fn label(&self, label: &str) -> Result<LabelPaths> {
        validate_name("label", label)?;
        let paths = LabelPaths::new(&self.hash, &self.session.key, label)?;
        paths.ensure_dirs()?;
        Ok(paths)
    }

    fn gc(&self) {
        let days = std::env::var("CEPTION_GC_DAYS").ok().and_then(|v| v.parse().ok()).unwrap_or(7.0_f64);
        let _ = store::gc(&self.hash, &self.session.key, Duration::from_secs_f64(days * 86_400.0));
    }

    /// The process this session's daemons watch, with its start time.
    fn watch_identity(&self) -> Result<Option<(u32, u64)>> {
        let Some(pid) = self.session.watch_pid else {
            return Ok(None);
        };
        let starttime = procfs::starttime(pid).with_context(|| format!("watched process {pid} is gone"))?;
        Ok(Some((pid, starttime)))
    }

    /// The error for a label this session has no thread for, naming other
    /// sessions that do.
    fn missing(&self, label: &str) -> anyhow::Error {
        let others: Vec<String> = store::list_project(&self.hash)
            .unwrap_or_default()
            .into_iter()
            .filter(|entry| entry.label == label && entry.session != self.session.key)
            .map(|entry| entry.session)
            .collect();
        if others.is_empty() {
            anyhow!(
                "no live daemon or stored thread for {label} in project {} (session {}); labels resolve from the \
                 invocation cwd — pass --cwd, or run `ception list --all` to see every project",
                self.project.display(),
                self.session.key
            )
        } else {
            anyhow!(
                "{label} belongs to another session ({}); labels are per session. To take it over deliberately, \
                 run with CEPTION_SESSION=<that session>",
                others.join(", ")
            )
        }
    }
}

// ----- talking to a daemon -------------------------------------------------

/// One request, one reply. The write half stays open until the reply: the
/// daemon takes EOF as the client hanging up.
async fn call(socket: &Path, request: &Request) -> Result<Reply> {
    let stream = UnixStream::connect(socket).await?;
    let (read, mut write) = stream.into_split();
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    write.write_all(&line).await?;
    let mut reply = String::new();
    BufReader::new(read).read_line(&mut reply).await?;
    if reply.is_empty() {
        bail!("daemon connection closed before replying");
    }
    serde_json::from_str(&reply).context("daemon reply")
}

const STATUS_TIMEOUT: Duration = Duration::from_secs(5);

async fn status(paths: &LabelPaths) -> Option<DaemonStatus> {
    match tokio::time::timeout(STATUS_TIMEOUT, call(&paths.socket, &Request::Status)).await {
        Ok(Ok(Reply::Result(outcome))) => outcome.daemon,
        _ => None,
    }
}

/// How a daemon gets started: fresh with spawn options, or reviving the
/// label's recorded thread (which the daemon reads itself, under its lock).
enum Revival {
    Fresh { model: Option<String>, effort: Option<String> },
    Resume,
}

enum Spawned {
    Ready,
    /// Another daemon holds the label.
    Busy,
}

async fn spawn_daemon(scope: &Scope, label: &str, paths: &LabelPaths, revival: &Revival) -> Result<Spawned> {
    let mut command = tokio::process::Command::new(std::env::current_exe().context("locate ception")?);
    command
        .arg("daemon")
        .arg("--project")
        .arg(&scope.project)
        .args(["--session", &scope.session.key, "--label", label]);
    match revival {
        Revival::Resume => {
            command.arg("--resume");
        }
        Revival::Fresh { model, effort } => {
            if let Some(model) = model {
                command.args(["--model", model]);
            }
            if let Some(effort) = effort {
                command.args(["--effort", effort]);
            }
        }
    }
    if let Some((pid, starttime)) = scope.watch_identity()? {
        command.args(["--watch-pid", &pid.to_string(), "--watch-starttime", &starttime.to_string()]);
    }
    // The daemon's stdout is our readiness pipe; it points its own stderr at
    // the log once it holds the label.
    command
        .current_dir(&scope.project)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    procfs::detach(command.as_std_mut());
    let mut child = command.spawn().context("start daemon")?;
    let stdout = child.stdout.take().expect("piped");
    let mut line = String::new();
    // A little past the daemon's own startup bound, so its error arrives.
    let wait = spawn_timeout() + Duration::from_secs(5);
    let read = tokio::time::timeout(wait, BufReader::new(stdout).read_line(&mut line)).await;
    // The intermediate exits as soon as it has forked the daemon.
    let _ = child.wait().await;
    let log = paths.log.display();
    match read {
        Err(_) => bail!("daemon for {label} did not start within {}s; see {log}", wait.as_secs()),
        Ok(Err(error)) => return Err(error).context("daemon readiness"),
        Ok(Ok(_)) => {}
    }
    match line.trim() {
        "ready" => Ok(Spawned::Ready),
        "busy" => Ok(Spawned::Busy),
        other => match other.strip_prefix("error: ") {
            Some(error) => bail!("daemon for {label} failed to start: {error}; see {log}"),
            None => bail!("daemon for {label} exited during startup; see {log}"),
        },
    }
}

const POLL: Duration = Duration::from_millis(50);

/// Reach the label's daemon, starting one if needed, under one deadline. A
/// daemon on its way out (say, its resumed Claude session's previous process
/// died) refuses status; we then wait for it to release the label and start
/// a successor.
async fn ensure_daemon(scope: &Scope, label: &str, paths: &LabelPaths, revival: &Revival) -> Result<DaemonStatus> {
    let deadline = Instant::now() + spawn_timeout() + Duration::from_secs(10);
    loop {
        if Instant::now() >= deadline {
            bail!("daemon for {label} did not become ready; see {}", paths.log.display());
        }
        if let Some(daemon) = status(paths).await {
            return Ok(daemon);
        }
        match spawn_daemon(scope, label, paths, revival).await? {
            Spawned::Ready => continue,
            // Starting elsewhere: wait for its socket, or for the lock to come
            // free if that start failed, then go round again.
            Spawned::Busy => {
                wait_until(deadline, || paths.socket.exists() || !store::lock_held(&paths.lock)).await;
                tokio::time::sleep(POLL).await;
            }
        }
    }
}

async fn wait_until(deadline: Instant, done: impl Fn() -> bool) {
    while Instant::now() < deadline && !done() {
        tokio::time::sleep(POLL).await;
    }
}

/// A live daemon for a label this session has a thread for, revived if
/// needed.
async fn revive(scope: &Scope, label: &str, paths: &LabelPaths) -> Result<DaemonStatus> {
    if let Some(daemon) = status(paths).await {
        return Ok(daemon);
    }
    if !paths.record.exists() {
        return Err(scope.missing(label));
    }
    ensure_daemon(scope, label, paths, &Revival::Resume).await
}

enum Conversation {
    Done(u8),
    /// The daemon didn't take the request (it is going away): wait for the
    /// label to come free and try again.
    Refused,
}

/// Send a turn-bearing request and relay its outcome. With a deadline, stop
/// waiting once it has passed and the turn is known to be running.
async fn converse(socket: &Path, request: &Request, label: &str, deadline: Option<Instant>) -> Result<Conversation> {
    let Ok(stream) = UnixStream::connect(socket).await else {
        return Ok(Conversation::Refused);
    };
    let (read, mut write) = stream.into_split();
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    write.write_all(&line).await?;
    let mut lines = BufReader::new(read).lines();
    let mut run: Option<String> = None;
    loop {
        let next = match (deadline, &run) {
            (Some(deadline), Some(run)) => match tokio::time::timeout_at(deadline, lines.next_line()).await {
                Ok(next) => next?,
                Err(_) => return Ok(Conversation::Done(still_running(label, run))),
            },
            _ => lines.next_line().await?,
        };
        let Some(next) = next else {
            bail!("daemon connection closed before replying");
        };
        match serde_json::from_str(&next).context("daemon reply")? {
            Reply::Accepted { run: accepted } => {
                if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    return Ok(Conversation::Done(still_running(label, &accepted)));
                }
                run = Some(accepted);
            }
            Reply::Refused { .. } => return Ok(Conversation::Refused),
            reply => return finish(reply, label).map(Conversation::Done),
        }
    }
}

/// Converse, reviving the label's daemon first and again whenever a daemon
/// on its way out refuses the request.
async fn converse_reviving(
    scope: &Scope,
    label: &str,
    paths: &LabelPaths,
    request: &Request,
    deadline: Option<Instant>,
    revival: impl Fn() -> Option<Revival>,
) -> Result<u8> {
    let mut printed_log = false;
    for _ in 0..3 {
        let daemon = match revival() {
            Some(revival) => ensure_daemon(scope, label, paths, &revival).await?,
            None => revive(scope, label, paths).await?,
        };
        if !printed_log {
            println!("log: {}", daemon.log.display());
            printed_log = true;
        }
        match converse(&paths.socket, request, label, deadline).await? {
            Conversation::Done(code) => return Ok(code),
            Conversation::Refused => {
                let free_by = Instant::now() + spawn_timeout();
                wait_until(free_by, || !store::lock_held(&paths.lock)).await;
            }
        }
    }
    bail!("the daemon for {label} kept refusing work while shutting down")
}

fn still_running(label: &str, run: &str) -> u8 {
    println!("still running: run {run}; reattach with `ception watch {label} --run {run}`");
    5
}

fn finish(reply: Reply, label: &str) -> Result<u8> {
    let outcome = match reply {
        Reply::Error { message } | Reply::Refused { message } => bail!(message),
        Reply::Accepted { .. } => bail!("daemon sent no final reply"),
        Reply::Result(outcome) => outcome,
    };
    if !outcome.report.is_empty() {
        println!("{}", outcome.report.trim_end());
    }
    // A turn report says what happened in the turn; the goal says whether
    // codex is going to keep going. Both are needed to know if the run is over.
    if outcome.turn_id.is_some() {
        if let Some(goal) = &outcome.goal {
            println!("\n{}", format_goal(Some(goal), Some(label)));
        }
    }
    Ok(status_exit_code(&outcome.status) as u8)
}

/// Prompt words joined, or stdin for a lone `-`.
fn read_text(words: Vec<String>, what: &str, command: &str) -> Result<String> {
    let text = if words == ["-"] {
        if std::io::stdin().is_terminal() {
            bail!("{command}: `-` reads the {what} from stdin, which is a terminal");
        }
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text).context("read stdin")?;
        text
    } else {
        words.join(" ")
    };
    if text.trim().is_empty() {
        bail!("{command} requires a {what} (`-` reads it from stdin)");
    }
    Ok(text)
}

// ----- commands ------------------------------------------------------------

async fn spawn(
    label: &str,
    model: Option<String>,
    effort: Option<String>,
    wait: &WaitArgs,
    prompt: Vec<String>,
) -> Result<u8> {
    let prompt = read_text(prompt, "prompt", "spawn")?;
    let deadline = wait.deadline();
    let scope = Scope::resolve(&wait.at)?;
    let paths = scope.label(label)?;
    scope.gc();
    if status(&paths).await.is_some() {
        bail!("daemon already live for {label}");
    }
    match spawn_daemon(&scope, label, &paths, &Revival::Fresh { model, effort }).await? {
        Spawned::Busy => bail!("daemon already live for {label}"),
        Spawned::Ready => {}
    }
    println!("log: {}", paths.log.display());
    let request = Request::Send { prompt, report: wait.report };
    match converse(&paths.socket, &request, label, deadline).await? {
        Conversation::Done(code) => Ok(code),
        Conversation::Refused => bail!("the new daemon for {label} refused the turn; see {}", paths.log.display()),
    }
}

async fn send(label: &str, wait: &WaitArgs, prompt: Vec<String>) -> Result<u8> {
    let prompt = read_text(prompt, "prompt", "send")?;
    let deadline = wait.deadline();
    let scope = Scope::resolve(&wait.at)?;
    let paths = scope.label(label)?;
    scope.gc();
    // The daemon decides atomically: steer if a turn is live, else start one.
    let request = Request::Send { prompt, report: wait.report };
    converse_reviving(&scope, label, &paths, &request, deadline, || None).await
}

/// Setting or resuming a goal blocks on the run codex starts for it; pause,
/// show and clear answer at once. Setting one on a label with no thread yet
/// starts a fresh daemon: the objective alone is enough to start a run.
async fn goal(label: &str, action: GoalAction, wait: &WaitArgs, objective: Vec<String>) -> Result<u8> {
    let objective = match action {
        GoalAction::Set => Some(read_text(objective, "objective", "goal")?),
        _ if !objective.is_empty() => bail!("goal: an objective only goes with setting a goal"),
        _ => None,
    };
    let deadline = wait.deadline();
    let scope = Scope::resolve(&wait.at)?;
    let paths = scope.label(label)?;
    scope.gc();
    let request = Request::Goal { action, objective, report: wait.report };
    // A label with no thread yet gets a fresh daemon.
    let revival = || {
        (action == GoalAction::Set && !paths.record.exists()).then_some(Revival::Fresh { model: None, effort: None })
    };
    if matches!(action, GoalAction::Set | GoalAction::Resume) {
        return converse_reviving(&scope, label, &paths, &request, deadline, revival).await;
    }
    // Pause, show and clear answer at once and print no log line.
    match revival() {
        Some(revival) => ensure_daemon(&scope, label, &paths, &revival).await?,
        None => revive(&scope, label, &paths).await?,
    };
    match converse(&paths.socket, &request, label, None).await? {
        Conversation::Done(code) => Ok(code),
        Conversation::Refused => bail!("the daemon for {label} is shutting down; try again"),
    }
}

async fn interrupt(label: &str, at: &AtArgs) -> Result<u8> {
    let scope = Scope::resolve(at)?;
    let paths = scope.label(label)?;
    if status(&paths).await.is_none() {
        println!("no active turn");
        return Ok(0);
    }
    finish(call(&paths.socket, &Request::Interrupt).await?, label)
}

async fn kill(label: Option<&str>, all: bool, at: &AtArgs) -> Result<u8> {
    let scope = Scope::resolve(at)?;
    if all {
        let mut killed = 0;
        for entry in store::list_project(&scope.hash)? {
            if entry.session != scope.session.key {
                continue;
            }
            let paths = entry.paths()?;
            if status(&paths).await.is_some() {
                let _ = call(&paths.socket, &Request::Shutdown).await;
                killed += 1;
            }
        }
        println!("killed {killed} daemon(s)");
        return Ok(0);
    }
    let label = label.expect("clap requires a label without --all");
    let paths = scope.label(label)?;
    if status(&paths).await.is_none() {
        println!("no live daemon");
        return Ok(0);
    }
    finish(call(&paths.socket, &Request::Shutdown).await?, label)
}

async fn list(all: bool, json_output: bool, at: &AtArgs) -> Result<u8> {
    let scope = Scope::resolve(at)?;
    scope.gc();
    let entries = if all { store::list_all()? } else { store::list_project(&scope.hash)? };
    let mut rows = Vec::new();
    for entry in entries {
        let paths = entry.paths()?;
        let daemon = status(&paths).await;
        let goal = daemon.as_ref().and_then(|d| d.goal.as_ref()).and_then(|g| g["status"].as_str().map(str::to_string));
        rows.push(json!({
            "label": entry.label,
            "cwd": entry.record.cwd,
            "hash": entry.hash,
            "session": if entry.session == scope.session.key { "mine".to_string() } else { entry.session.clone() },
            "status": daemon.as_ref().map_or("dead".to_string(), |d| d.state.clone()),
            "goal": goal,
            "threadId": daemon.as_ref().and_then(|d| d.thread_id.clone()).unwrap_or(entry.record.thread_id.clone()),
            "lastUsed": timestamp(entry.modified),
            "logPath": paths.log,
        }));
    }
    if json_output {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(0);
    }
    let text = |value: &Value| value.as_str().map_or("-".to_string(), str::to_string);
    for row in &rows {
        println!(
            "{}\t{}\t{}\tgoal={}\t{}\t{}\t{}",
            text(&row["label"]),
            text(&row["session"]),
            text(&row["status"]),
            text(&row["goal"]),
            text(&row["threadId"]),
            text(&row["lastUsed"]),
            text(&row["logPath"]),
        );
    }
    Ok(0)
}

fn timestamp(time: SystemTime) -> String {
    jiff::Timestamp::try_from(time)
        .map(|ts| ts.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
        .unwrap_or_else(|_| "-".into())
}

async fn watch(label: &str, follow: bool, run: Option<String>, wait: &WaitArgs) -> Result<u8> {
    let deadline = wait.deadline();
    let scope = Scope::resolve(&wait.at)?;
    let paths = scope.label(label)?;
    if follow {
        // A read-only tail may peek at another session's label.
        let log = if paths.log.exists() {
            paths.log.clone()
        } else {
            store::list_project(&scope.hash)?
                .into_iter()
                .filter(|entry| entry.label == label)
                .max_by_key(|entry| entry.modified)
                .and_then(|entry| entry.paths().ok())
                .map_or(paths.log.clone(), |paths| paths.log)
        };
        let error = std::process::Command::new("tail").arg("-F").arg(&log).exec();
        return Err(error).context("run tail");
    }
    // Attach to the live turn and block until it settles; the report arrives
    // exactly as it would have on the client that started it.
    let Some(daemon) = status(&paths).await else {
        bail!("no live daemon for {label}; `ception send` respawns and resumes a stored thread");
    };
    println!("log: {}", daemon.log.display());
    let request = Request::Watch { report: wait.report, run };
    match converse(&paths.socket, &request, label, deadline).await? {
        Conversation::Done(code) => Ok(code),
        Conversation::Refused => bail!("no live daemon for {label} (it is shutting down)"),
    }
}
