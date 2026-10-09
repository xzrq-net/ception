//! Client <-> daemon messages: one JSON request line from the client, one
//! JSON reply line from the daemon. Both ends are this binary, so the format
//! is not a compatibility surface.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::render::ReportLevel;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    /// Steer the live turn if there is one, else start a turn.
    Send { prompt: String, report: ReportLevel },
    Goal { action: GoalAction, objective: Option<String>, report: ReportLevel },
    Interrupt,
    /// Block until the live turn settles; answers at once when idle. With a
    /// run id: that run, live or retained.
    Watch { report: ReportLevel, run: Option<String> },
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalAction {
    Set,
    Resume,
    Pause,
    Show,
    Clear,
}

/// Zero or more `Accepted`, then exactly one `Result`, `Error` or `Refused`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    /// The request's turn is running as run `run`; the final reply follows
    /// when it settles.
    Accepted { run: String },
    Result(Outcome),
    Error { message: String },
    /// Not acted on (the daemon is shutting down); safe to retry elsewhere.
    Refused { message: String },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Outcome {
    /// A codex turn status (completed, failed, interrupted) or one of ok,
    /// steered, idle; maps to the exit code.
    pub status: String,
    pub report: String,
    pub goal: Option<Value>,
    /// Set when the report is a turn report, which then ends with the goal line.
    pub turn_id: Option<String>,
    pub daemon: Option<DaemonStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub pid: u32,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    /// "active" while a turn runs, else "idle".
    pub state: String,
    pub goal: Option<Value>,
    pub log: PathBuf,
}

impl Reply {
    pub fn error(message: impl Into<String>) -> Self {
        Self::Error { message: message.into() }
    }

    pub fn ok(status: &str, report: impl Into<String>) -> Self {
        Self::Result(Outcome { status: status.to_string(), report: report.into(), ..Default::default() })
    }
}
