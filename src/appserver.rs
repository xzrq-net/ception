//! The codex app-server child: spawning it and newline-delimited JSON-RPC
//! over its stdio. No `"jsonrpc"` field on the wire.

use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

pub enum AppEvent {
    Message(Value),
    Stderr(String),
    /// stdout ended, or carried something that isn't JSON; either way the
    /// server is gone for our purposes.
    Closed(Option<String>),
}

pub enum Incoming {
    Notification { method: String, params: Value },
    Response { id: u64, outcome: Result<Value, String> },
    /// The server asking us something (approvals); we never answer yes.
    Request { id: Value, method: String },
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
                    Some(error) => Err(error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("request failed")
                        .to_string()),
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

pub struct AppServer {
    child: Child,
    stdin: Option<ChildStdin>,
    next_id: u64,
    stderr_tail: Arc<Mutex<String>>,
}

impl AppServer {
    pub fn spawn(cwd: &Path) -> Result<(Self, UnboundedReceiver<AppEvent>)> {
        let argv = codex_command()?;
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Its own process group, so close() reaches what npx starts under it.
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
        let mut child = command
            .spawn()
            .with_context(|| format!("start {}", argv.join(" ")))?;
        let (tx, rx) = unbounded_channel();
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        tokio::spawn(read_stdout(child.stdout.take().expect("piped"), tx.clone()));
        tokio::spawn(read_stderr(child.stderr.take().expect("piped"), tx, stderr_tail.clone()));
        let stdin = child.stdin.take();
        Ok((Self { child, stdin, next_id: 1, stderr_tail }, rx))
    }

    pub async fn request(&mut self, method: &str, params: Value) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({ "id": id, "method": method, "params": params })).await?;
        Ok(id)
    }

    pub async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(json!({ "method": method, "params": params })).await
    }

    pub async fn reject(&mut self, id: Value, method: &str) -> Result<()> {
        let error = json!({ "code": -32601, "message": format!("Unsupported server request: {method}") });
        self.send(json!({ "id": id, "error": error })).await
    }

    async fn send(&mut self, message: Value) -> Result<()> {
        let stdin = self.stdin.as_mut().context("codex app-server stdin is closed")?;
        let mut line = serde_json::to_vec(&message)?;
        line.push(b'\n');
        stdin.write_all(&line).await.context("write to codex app-server")?;
        Ok(())
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
        let want = self.request(method, params).await?;
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
                Some(Incoming::Request { id, method }) => self.reject(id, &method).await?,
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
        self.notify("initialized", json!({})).await
    }

    /// Why the server went away, once its stdout has closed.
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
        let stderr = self.stderr_tail.lock().unwrap().trim().to_string();
        if stderr.is_empty() {
            format!("codex app-server exited unexpectedly ({status})")
        } else {
            format!("codex app-server exited unexpectedly ({status})\n{stderr}")
        }
    }

    /// Close stdin and give the server a moment to exit, then SIGTERM its
    /// whole process group (codex's own shells included), then SIGKILL.
    /// Returns once the direct child is reaped.
    pub async fn close(&mut self) {
        self.stdin = None;
        let Some(pgid) = self.child.id().map(|pid| pid as libc::pid_t) else {
            return;
        };
        let _ = tokio::time::timeout(Duration::from_millis(100), self.child.wait()).await;
        unsafe { libc::kill(-pgid, libc::SIGTERM) };
        if tokio::time::timeout(Duration::from_secs(2), self.child.wait()).await.is_err() {
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
            let _ = self.child.wait().await;
        }
    }
}

async fn read_stdout(stdout: tokio::process::ChildStdout, tx: UnboundedSender<AppEvent>) {
    let mut lines = BufReader::new(stdout).lines();
    loop {
        let event = match lines.next_line().await {
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
}
