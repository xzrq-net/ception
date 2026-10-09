//! Harness for driving the `ception` CLI as a subprocess against the fake
//! app-server. Each [`Ctx`] is a hermetic world: its own HOME (so its own state
//! root, locks and sockets), project dir and fake state file. Tests run in
//! parallel threads of one process, which every daemon watches by default.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, TryLockError};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const CEPTION: &str = env!("CARGO_BIN_EXE_ception");
pub const FAKE: &str = env!("CARGO_BIN_EXE_ception-fake-appserver");

/// The session key of [`Ctx::env`].
pub const SESSION: &str = "main";

/// Hang protection for one CLI call, not a speed assertion.
const RUN_TIMEOUT: Duration = Duration::from_secs(20);
/// Default bound for polling a precondition.
pub const WAIT: Duration = Duration::from_secs(10);

pub fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

pub fn ms(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

// ----- environment -----------------------------------------------------------

/// The complete environment of a CLI call; nothing is inherited.
#[derive(Clone, Debug, PartialEq)]
pub struct Env(BTreeMap<String, String>);

impl Env {
    pub fn set(&self, key: &str, value: impl ToString) -> Env {
        let mut vars = self.0.clone();
        vars.insert(key.to_string(), value.to_string());
        Env(vars)
    }

    pub fn unset(&self, key: &str) -> Env {
        let mut vars = self.0.clone();
        vars.remove(key);
        Env(vars)
    }
}

// ----- one world per test ------------------------------------------------------

pub struct Ctx {
    _tmp: tempfile::TempDir,
    pub root: PathBuf,
    pub home: PathBuf,
    pub project: PathBuf,
    pub fake_state: PathBuf,
    /// Session [`SESSION`], watching this test process.
    pub env: Env,
    /// Every env a call ran under, for `kill --all` at the end.
    used: Mutex<Vec<Env>>,
}

impl Ctx {
    pub fn new(behavior: &str) -> Ctx {
        let tmp = tempfile::Builder::new().prefix("ception-test-").tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let home = root.join("home");
        let project = root.join("project");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&project).unwrap();
        let fake_state = root.join("fake-state.json");
        let vars = [
            ("PATH", std::env::var("PATH").unwrap_or_default()),
            ("HOME", home.display().to_string()),
            ("CEPTION_CODEX_CMD", FAKE.to_string()),
            ("CEPTION_FAKE_STATE", fake_state.display().to_string()),
            ("CEPTION_FAKE_BEHAVIOR", behavior.to_string()),
            ("CEPTION_IDLE_TIMEOUT_SECS", "30".to_string()),
            ("CEPTION_SESSION", SESSION.to_string()),
            ("CEPTION_WATCH_PID", std::process::id().to_string()),
        ];
        let env = Env(vars.into_iter().map(|(k, v)| (k.to_string(), v)).collect());
        Ctx { _tmp: tmp, root, home, project, fake_state, env, used: Mutex::new(Vec::new()) }
    }

    /// A CLI call, by default under [`Ctx::env`] in the project dir.
    pub fn ception(&self, args: &[&str]) -> Call<'_> {
        Call {
            ctx: self,
            args: args.iter().map(|arg| arg.to_string()).collect(),
            env: self.env.clone(),
            cwd: self.project.clone(),
            stdin: None,
            timeout: RUN_TIMEOUT,
        }
    }

    fn register(&self, env: &Env) {
        let mut used = self.used.lock().unwrap();
        if !used.contains(env) {
            used.push(env.clone());
        }
    }

    // ----- on-disk layout --------------------------------------------------

    pub fn state_root(&self) -> PathBuf {
        self.home.join(".local/state/ception-rs")
    }

    pub fn run_dir(&self) -> PathBuf {
        self.state_root().join("run")
    }

    pub fn projects_dir(&self) -> PathBuf {
        self.state_root().join("projects")
    }

    /// sha256(realpath(project root))[..12], as the layout specifies.
    pub fn project_hash(&self) -> String {
        let real = fs::canonicalize(&self.project).unwrap();
        hex(&Sha256::digest(real.as_os_str().as_encoded_bytes()))[..12].to_string()
    }

    pub fn session_dir(&self, session: &str) -> PathBuf {
        self.projects_dir().join(self.project_hash()).join(session)
    }

    pub fn record_path(&self, session: &str, label: &str) -> PathBuf {
        self.session_dir(session).join(format!("{label}.json"))
    }

    pub fn record(&self, session: &str, label: &str) -> Value {
        let path = self.record_path(session, label);
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        serde_json::from_str(&text).unwrap()
    }

    pub fn log_path(&self, session: &str, label: &str) -> PathBuf {
        self.session_dir(session).join(format!("{label}.log"))
    }

    pub fn log(&self, session: &str, label: &str) -> String {
        let path = self.log_path(session, label);
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    /// `<run>/<key>.lock`, key = sha256("projhash/session/label")[..16].
    pub fn lock_path(&self, session: &str, label: &str) -> PathBuf {
        let key = format!("{}/{session}/{label}", self.project_hash());
        self.run_dir().join(format!("{}.lock", &hex(&Sha256::digest(key))[..16]))
    }

    pub fn sockets(&self) -> Vec<String> {
        dir_names(&self.run_dir()).into_iter().filter(|name| name.ends_with(".sock")).collect()
    }

    // ----- observing ---------------------------------------------------------

    /// What the fake app-server recorded; `{}` before it first wrote.
    pub fn fake_state(&self) -> Value {
        match fs::read_to_string(&self.fake_state) {
            Ok(text) => serde_json::from_str(&text).expect("parse fake state"),
            Err(error) if error.kind() == ErrorKind::NotFound => json!({}),
            Err(error) => panic!("read fake state: {error}"),
        }
    }

    pub fn turn_starts(&self) -> usize {
        self.fake_state()["lastTurnStarts"].as_array().map_or(0, Vec::len)
    }

    /// Block until the fake has started `n` turns.
    #[track_caller]
    pub fn wait_turn_starts(&self, n: usize) {
        assert!(wait_until(WAIT, || self.turn_starts() == n), "fake never reached {n} turn start(s)");
    }

    /// `list --json` under `env`.
    #[track_caller]
    pub fn list(&self, env: &Env) -> Vec<Value> {
        let out = self.ception(&["list", "--json"]).env(env).run().expect_code(0);
        serde_json::from_str(&out.stdout).unwrap_or_else(|e| panic!("list --json: {e}\n{out}"))
    }

    pub fn list_text(&self) -> String {
        self.ception(&["list"]).run().stdout
    }

    /// The status `list` shows for `label`, if listed.
    pub fn status(&self, label: &str) -> Option<String> {
        row(&self.list(&self.env), label).map(|row| row["status"].as_str().unwrap().to_string())
    }

    /// Block until `list` shows `label` with `status`.
    pub fn wait_status(&self, label: &str, status: &str, timeout: Duration) -> bool {
        wait_until(timeout, || self.status(label).as_deref() == Some(status))
    }

    /// Block until the plain `list` output contains `needle`.
    #[track_caller]
    pub fn wait_listed(&self, needle: &str) {
        assert!(wait_until(WAIT, || self.list_text().contains(needle)), "list never showed {needle:?}");
    }
}

impl Drop for Ctx {
    /// `kill --all` for every env used, then wait for the daemons to let go of
    /// their locks so none outlives the temp dir. Never panics.
    fn drop(&mut self) {
        let envs = std::mem::take(&mut *self.used.lock().unwrap_or_else(|e| e.into_inner()));
        for env in envs {
            let call = Call {
                ctx: self,
                args: vec!["kill".into(), "--all".into()],
                env,
                cwd: self.project.clone(),
                stdin: None,
                timeout: secs(5),
            };
            let _ = call.try_run();
        }
        let run = self.run_dir();
        wait_until(secs(5), || {
            dir_names(&run).iter().filter(|name| name.ends_with(".lock")).all(|name| lock_free(&run.join(name)))
        });
    }
}

pub fn row<'a>(rows: &'a [Value], label: &str) -> Option<&'a Value> {
    rows.iter().find(|row| row["label"] == label)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn dir_names(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries.flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).collect()
}

/// Whether nobody holds this flock (a missing file counts as free).
pub fn lock_free(path: &Path) -> bool {
    let Ok(file) = File::options().write(true).open(path) else {
        return true;
    };
    match file.try_lock() {
        Ok(()) => true,
        Err(TryLockError::WouldBlock) => false,
        Err(TryLockError::Error(_)) => false,
    }
}

pub fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

pub fn set_age(path: &Path, age: Duration) {
    let file = File::options().write(true).open(path).unwrap();
    file.set_modified(SystemTime::now() - age).unwrap();
}

/// The fields of /proc/<pid>/stat after the command name: state, ppid, ...
fn stat_fields(pid: u32) -> Option<Vec<String>> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 2..];
    Some(rest.split_whitespace().map(str::to_string).collect())
}

pub fn ppid(pid: u32) -> u32 {
    stat_fields(pid).expect("process exists")[1].parse().unwrap()
}

/// Running, as opposed to gone or a zombie.
pub fn alive(pid: u32) -> bool {
    stat_fields(pid).is_some_and(|fields| fields[0] != "Z")
}

/// SIGKILLs processes a failing test would otherwise leak, if they are still
/// the same processes (same start time) when the guard drops.
pub struct Reap(Vec<(u32, String)>);

impl Reap {
    pub fn new(pids: &[u32]) -> Reap {
        Reap(pids.iter().filter_map(|&pid| Some((pid, stat_fields(pid)?.get(19)?.clone()))).collect())
    }
}

impl Drop for Reap {
    fn drop(&mut self) {
        for (pid, starttime) in &self.0 {
            if stat_fields(*pid).and_then(|fields| fields.get(19).cloned()).as_ref() == Some(starttime) {
                unsafe { libc::kill(*pid as libc::pid_t, libc::SIGKILL) };
            }
        }
    }
}

// ----- waiting -------------------------------------------------------------------

pub fn wait_for<T>(timeout: Duration, mut probe: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = probe() {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(ms(50));
    }
}

pub fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    wait_for(timeout, || done().then_some(())).is_some()
}

// ----- running the CLI -------------------------------------------------------------

pub struct Call<'a> {
    ctx: &'a Ctx,
    args: Vec<String>,
    env: Env,
    cwd: PathBuf,
    stdin: Option<String>,
    timeout: Duration,
}

impl Call<'_> {
    pub fn env(mut self, env: &Env) -> Self {
        self.env = env.clone();
        self
    }

    pub fn cwd(mut self, dir: &Path) -> Self {
        self.cwd = dir.to_path_buf();
        self
    }

    pub fn stdin(mut self, text: &str) -> Self {
        self.stdin = Some(text.to_string());
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn describe(&self) -> String {
        format!("ception {}", self.args.join(" "))
    }

    /// Run to completion; panics if it outlives the timeout.
    #[track_caller]
    pub fn run(self) -> Output {
        let what = self.describe();
        let timeout = self.timeout;
        match self.try_run() {
            Ok(out) => out,
            Err(out) => panic!("{what} still running after {timeout:?}\n{out}"),
        }
    }

    /// Err carries what it printed before being killed at the timeout.
    fn try_run(self) -> Result<Output, Output> {
        let timeout = self.timeout;
        let mut bg = self.spawn();
        match bg.wait_timeout(timeout) {
            Some(out) => Ok(out),
            None => {
                let _ = bg.child.kill();
                Err(bg.wait_timeout(secs(5)).unwrap_or_default())
            }
        }
    }

    /// Start in the background with live access to stdout.
    pub fn spawn(self) -> Bg {
        self.ctx.register(&self.env);
        let mut child = Command::new(CEPTION)
            .args(&self.args)
            .current_dir(&self.cwd)
            .env_clear()
            .envs(&self.env.0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start ception");
        let mut stdin = child.stdin.take().expect("piped");
        if let Some(text) = &self.stdin {
            stdin.write_all(text.as_bytes()).expect("write stdin");
        }
        drop(stdin);
        let out = Capture::start(child.stdout.take().expect("piped"));
        let err = Capture::start(child.stderr.take().expect("piped"));
        Bg { what: self.describe(), child, out, err, done: None }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Output {
    /// None when killed by a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    #[track_caller]
    pub fn expect_code(self, code: i32) -> Self {
        assert_eq!(self.code, Some(code), "unexpected exit code\n{self}");
        self
    }

    /// Lines of stdout starting with `prefix`.
    pub fn lines_starting(&self, prefix: &str) -> Vec<&str> {
        self.stdout.lines().filter(|line| line.starts_with(prefix)).collect()
    }
}

impl fmt::Display for Output {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "exit: {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}", self.code, self.stdout, self.stderr)
    }
}

#[track_caller]
pub fn assert_has(text: &str, needle: &str) {
    assert!(text.contains(needle), "expected {needle:?} in:\n{text}");
}

#[track_caller]
pub fn assert_lacks(text: &str, needle: &str) {
    assert!(!text.contains(needle), "unexpected {needle:?} in:\n{text}");
}

/// A pipe drained by a thread into a shared buffer.
struct Capture {
    buf: Arc<Mutex<Vec<u8>>>,
    reader: Option<JoinHandle<()>>,
}

impl Capture {
    fn start(mut pipe: impl Read + Send + 'static) -> Capture {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = buf.clone();
        let reader = thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => sink.lock().unwrap().extend_from_slice(&chunk[..n]),
                }
            }
        });
        Capture { buf, reader: Some(reader) }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.buf.lock().unwrap()).into_owned()
    }

    fn finish(&mut self) -> String {
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        self.text()
    }
}

/// A CLI call running in the background. Killed on drop if still running.
pub struct Bg {
    what: String,
    child: Child,
    out: Capture,
    err: Capture,
    done: Option<Output>,
}

impl Bg {
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Everything printed to stdout so far.
    pub fn stdout(&self) -> String {
        self.out.text()
    }

    /// Block until stdout contains `needle`.
    #[track_caller]
    pub fn wait_stdout(&self, needle: &str) {
        assert!(
            wait_until(WAIT, || self.stdout().contains(needle)),
            "{} never printed {needle:?}; stdout so far:\n{}",
            self.what,
            self.stdout()
        );
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }

    /// Wait for exit; panics past the harness timeout.
    #[track_caller]
    pub fn wait(&mut self) -> Output {
        match self.wait_timeout(RUN_TIMEOUT) {
            Some(out) => out,
            None => panic!("{} still running after {RUN_TIMEOUT:?}; stdout so far:\n{}", self.what, self.stdout()),
        }
    }

    /// None if still running at the deadline.
    pub fn wait_timeout(&mut self, timeout: Duration) -> Option<Output> {
        if let Some(done) = &self.done {
            return Some(done.clone());
        }
        let deadline = Instant::now() + timeout;
        let status = loop {
            match self.child.try_wait().expect("wait for ception") {
                Some(status) => break status,
                None if Instant::now() >= deadline => return None,
                None => thread::sleep(ms(10)),
            }
        };
        let out = Output { code: status.code(), stdout: self.out.finish(), stderr: self.err.finish() };
        self.done = Some(out.clone());
        Some(out)
    }
}

impl Drop for Bg {
    fn drop(&mut self) {
        if self.done.is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// A stand-in for a session's watched process (Claude Code itself).
pub struct Dummy {
    child: Child,
}

impl Dummy {
    pub fn start() -> Dummy {
        let child = Command::new("sleep").arg("1000").spawn().expect("start sleep");
        Dummy { child }
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Kill and reap: a zombie still has its /proc starttime.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Dummy {
    fn drop(&mut self) {
        self.kill();
    }
}
