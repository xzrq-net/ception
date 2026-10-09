//! The codex app-server child: spawning it, newline-delimited JSON-RPC over
//! its stdio (no `"jsonrpc"` field on the wire), and owning its process group.

use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;
use tokio::time::Instant;

use crate::procfs::PidWatch;

pub enum AppEvent {
    Message(Value),
    Stderr(String),
    /// Stdout ended, carried something that isn't JSON, or the server exited
    /// while something else kept the pipe open; either way it is gone.
    Closed(Option<String>),
}

pub enum Incoming {
    Notification {
        method: String,
        params: Value,
    },
    Response {
        id: u64,
        outcome: Result<Value, String>,
    },
    /// The server asking us something (approvals); we never answer yes.
    Request {
        id: Value,
        method: String,
    },
}

impl Incoming {
    pub fn classify(message: Value) -> Option<Self> {
        let Value::Object(mut map) = message else {
            return None;
        };
        let method = map.get("method").and_then(Value::as_str).map(str::to_string);
        match (map.remove("id"), method) {
            (Some(id), Some(method)) => Some(Self::Request { id, method }),
            (Some(id), None) => {
                let outcome = match map.remove("error") {
                    Some(error) => {
                        Err(error.get("message").and_then(Value::as_str).unwrap_or("request failed").to_string())
                    }
                    None => Ok(map.remove("result").unwrap_or(Value::Null)),
                };
                Some(Self::Response { id: id.as_u64()?, outcome })
            }
            (None, Some(method)) => {
                let params = map.remove("params").unwrap_or(Value::Null);
                Some(Self::Notification { method, params })
            }
            (None, None) => None,
        }
    }
}

pub fn codex_command() -> Result<Vec<String>> {
    match std::env::var("CEPTION_CODEX_CMD") {
        Ok(line) if !line.is_empty() => {
            let argv = shell_words::split(&line).context("CEPTION_CODEX_CMD")?;
            if argv.is_empty() {
                bail!("CEPTION_CODEX_CMD is empty");
            }
            Ok(argv)
        }
        _ => Ok(["npx", "-y", "@openai/codex", "app-server"].map(String::from).to_vec()),
    }
}

const STDERR_TAIL: usize = 4000;

/// After the server exits, how long to keep reading output it already wrote
/// when a descendant holds the pipe open.
const EXIT_DRAIN: Duration = Duration::from_millis(500);

pub struct AppServer {
    child: Child,
    /// The server's process group, which outlives the child handle: codex's
    /// shells and npx's children live in it.
    pgid: libc::pid_t,
    /// Lines for the writer task. Writes never block the caller, so a server
    /// that stops reading can't wedge the daemon.
    writer: Option<UnboundedSender<Vec<u8>>>,
    next_id: u64,
    stderr_tail: Arc<Mutex<String>>,
    /// Fires when stderr hits EOF, so an exit description includes the
    /// server's last words.
    stderr_done: Option<oneshot::Receiver<()>>,
    closed: bool,
}

impl AppServer {
    /// `owner`, if given, marks the server and everything it starts (see
    /// [`crate::procfs::OWNER_VAR`]).
    pub fn spawn(cwd: &Path, owner: Option<&str>) -> Result<(Self, UnboundedReceiver<AppEvent>)> {
        let argv = codex_command()?;
        let mut command = Command::new(&argv[0]);
        if let Some(owner) = owner {
            command.env(crate::procfs::OWNER_VAR, owner);
        }
        command
            .args(&argv[1..])
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Its own process group, so close() reaches everything under it.
        // PDEATHSIG makes it die with us even on SIGKILL; the "parent" is the
        // spawning thread, and ception runs current_thread runtimes, so that
        // is the main thread. If we died before prctl took effect, the child
        // is already reparented: check, and bail.
        let parent = std::process::id() as libc::pid_t;
        unsafe {
            command.pre_exec(move || {
                libc::setpgid(0, 0);
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                if libc::getppid() != parent {
                    libc::_exit(1);
                }
                Ok(())
            });
        }
        let mut child = command.spawn().with_context(|| format!("start {}", argv.join(" ")))?;
        let pid = child.id().context("app-server pid")?;
        let exit = PidWatch::child(pid)?;
        let (tx, rx) = unbounded_channel();
        let tx_writer = tx.clone();
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        tokio::spawn(read_stdout(child.stdout.take().expect("piped"), tx.clone(), exit));
        let (stderr_eof, stderr_done) = oneshot::channel();
        tokio::spawn(read_stderr(child.stderr.take().expect("piped"), tx, stderr_tail.clone(), stderr_eof));
        let (writer, lines) = unbounded_channel();
        tokio::spawn(write_stdin(child.stdin.take().expect("piped"), lines, tx_writer));
        let server = Self {
            child,
            pgid: pid as libc::pid_t,
            writer: Some(writer),
            next_id: 1,
            stderr_tail,
            stderr_done: Some(stderr_done),
            closed: false,
        };
        Ok((server, rx))
    }

    pub fn request(&mut self, method: &str, params: Value) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({ "id": id, "method": method, "params": params }))?;
        Ok(id)
    }

    pub fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(json!({ "method": method, "params": params }))
    }

    pub fn reject(&mut self, id: Value, method: &str) -> Result<()> {
        let error = json!({ "code": -32601, "message": format!("Unsupported server request: {method}") });
        self.send(json!({ "id": id, "error": error }))
    }

    fn send(&mut self, message: Value) -> Result<()> {
        let mut line = serde_json::to_vec(&message)?;
        line.push(b'\n');
        self.writer.as_ref().and_then(|writer| writer.send(line).ok()).context("codex app-server stdin is closed")
    }

    /// Send a request and wait for its response, handing everything else to
    /// `other`. Only for startup and one-shot clients, where no turn runs.
    pub async fn call(
        &mut self,
        events: &mut UnboundedReceiver<AppEvent>,
        method: &str,
        params: Value,
        mut other: impl FnMut(AppEvent),
    ) -> Result<Value> {
        let want = self.request(method, params)?;
        loop {
            let Some(event) = events.recv().await else {
                bail!("codex app-server closed");
            };
            let message = match event {
                AppEvent::Message(message) => message,
                AppEvent::Closed(reason) => bail!(self.exit_description(reason).await),
                other_event => {
                    other(other_event);
                    continue;
                }
            };
            match Incoming::classify(message.clone()) {
                Some(Incoming::Response { id, outcome }) if id == want => {
                    return outcome.map_err(|error| anyhow!(error));
                }
                Some(Incoming::Request { id, method }) => self.reject(id, &method)?,
                _ => other(AppEvent::Message(message)),
            }
        }
    }

    pub async fn initialize(
        &mut self,
        events: &mut UnboundedReceiver<AppEvent>,
        other: impl FnMut(AppEvent),
    ) -> Result<()> {
        let params = json!({
            "clientInfo": { "name": "ception", "title": "ception", "version": env!("CARGO_PKG_VERSION") },
            "capabilities": { "experimentalApi": false },
        });
        self.call(events, "initialize", params, other).await?;
        self.notify("initialized", json!({}))
    }

    /// Why the server went away, once [`AppEvent::Closed`] has arrived.
    pub async fn exit_description(&mut self, reason: Option<String>) -> String {
        if let Some(reason) = reason {
            return reason;
        }
        let status = match tokio::time::timeout(Duration::from_secs(2), self.child.wait()).await {
            Ok(Ok(status)) => match (status.code(), std::os::unix::process::ExitStatusExt::signal(&status)) {
                (Some(code), _) => format!("exit {code}"),
                (None, Some(signal)) => format!("signal {signal}"),
                _ => "unknown status".to_string(),
            },
            _ => "stdout closed".to_string(),
        };
        if let Some(done) = self.stderr_done.take() {
            let _ = tokio::time::timeout(Duration::from_millis(500), done).await;
        }
        let stderr = self.stderr_tail.lock().unwrap().trim().to_string();
        if stderr.is_empty() {
            format!("codex app-server exited unexpectedly ({status})")
        } else {
            format!("codex app-server exited unexpectedly ({status})\n{stderr}")
        }
    }

    /// Close stdin and give the server a moment to exit, then SIGTERM its
    /// process group, then SIGKILL. Returns once the group is empty (or the
    /// SIGKILL grace ran out), so a successor never overlaps old tools.
    pub async fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.writer = None;
        let _ = tokio::time::timeout(Duration::from_millis(100), self.child.wait()).await;
        unsafe { libc::kill(-self.pgid, libc::SIGTERM) };
        if !self.group_gone_within(Duration::from_secs(2)).await {
            unsafe { libc::kill(-self.pgid, libc::SIGKILL) };
            self.group_gone_within(Duration::from_secs(1)).await;
        }
    }

    async fn group_gone_within(&mut self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            // Reap the leader: a zombie still counts as a group member.
            let _ = self.child.try_wait();
            if unsafe { libc::kill(-self.pgid, 0) } != 0 {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

impl Drop for AppServer {
    /// Abandoned without close(), e.g. a startup cut short: take the whole
    /// group down, not just the direct child.
    fn drop(&mut self) {
        if !self.closed {
            unsafe { libc::kill(-self.pgid, libc::SIGKILL) };
        }
    }
}

/// A failed write means the server stopped reading for good: report it as
/// gone, or requests already sent would wait forever.
async fn write_stdin(mut stdin: ChildStdin, mut lines: UnboundedReceiver<Vec<u8>>, events: UnboundedSender<AppEvent>) {
    while let Some(line) = lines.recv().await {
        if let Err(error) = stdin.write_all(&line).await {
            let _ = events.send(AppEvent::Closed(Some(format!("write to codex app-server: {error}"))));
            return;
        }
    }
}

async fn read_stdout(stdout: ChildStdout, tx: UnboundedSender<AppEvent>, exit: Option<PidWatch>) {
    let mut lines = BufReader::new(stdout).lines();
    let mut drain_until: Option<Instant> = None;
    loop {
        let next = tokio::select! {
            line = lines.next_line() => line,
            // The server exited but stdout is still open (a descendant has
            // it): read what is already there, then report it gone.
            _ = async { exit.as_ref().expect("guarded").exited().await },
                if exit.is_some() && drain_until.is_none() =>
            {
                drain_until = Some(Instant::now() + EXIT_DRAIN);
                continue;
            }
            _ = tokio::time::sleep_until(drain_until.unwrap_or_else(Instant::now)), if drain_until.is_some() => Ok(None),
        };
        let event = match next {
            Ok(Some(line)) if line.trim().is_empty() => continue,
            Ok(Some(line)) => match serde_json::from_str(&line) {
                Ok(message) => AppEvent::Message(message),
                Err(error) => AppEvent::Closed(Some(format!("app-server emitted invalid JSON: {error}"))),
            },
            Ok(None) => AppEvent::Closed(None),
            Err(error) => AppEvent::Closed(Some(format!("app-server stdout: {error}"))),
        };
        let closed = matches!(event, AppEvent::Closed(_));
        if tx.send(event).is_err() || closed {
            return;
        }
    }
}

async fn read_stderr(
    stderr: tokio::process::ChildStderr,
    tx: UnboundedSender<AppEvent>,
    tail: Arc<Mutex<String>>,
    eof: oneshot::Sender<()>,
) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        {
            let mut tail = tail.lock().unwrap();
            tail.push_str(&line);
            tail.push('\n');
            if tail.len() > STDERR_TAIL {
                let cut = tail.len() - STDERR_TAIL;
                let cut = (cut..tail.len()).find(|&i| tail.is_char_boundary(i)).unwrap_or(0);
                tail.drain(..cut);
            }
        }
        let _ = tx.send(AppEvent::Stderr(line));
    }
    let _ = eof.send(());
}
