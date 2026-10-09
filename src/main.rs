//! ception: run Codex as a named, long-lived subagent. Each label is a daemon
//! owning a codex app-server and one thread; CLI calls talk to it over a unix
//! socket and block until the turn settles.

mod appserver;
mod client;
mod daemon;
mod paths;
mod procfs;
mod proto;
mod quota;
mod render;
mod session;
mod store;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use crate::render::ReportLevel;

/// Run OpenAI Codex as a named background subagent.
///
/// Labels are scoped to the project root (nearest .jj/.git/.hg above the
/// invocation directory, or --cwd) and to the calling session.
#[derive(Parser)]
#[command(name = "ception", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start a fresh Codex thread under LABEL and run its first turn.
    Spawn {
        label: String,
        /// Model for this label's thread; default from the codex config.
        #[arg(long)]
        model: Option<String>,
        /// Reasoning effort for this label's turns; default from the codex config.
        #[arg(long)]
        effort: Option<String>,
        #[command(flatten)]
        wait: WaitArgs,
        /// The prompt; `-` or nothing reads stdin.
        #[arg(trailing_var_arg = true)]
        prompt: Vec<String>,
    },
    /// Follow up on LABEL's thread: steers the live turn, else starts one.
    Send {
        label: String,
        #[command(flatten)]
        wait: WaitArgs,
        #[arg(trailing_var_arg = true)]
        prompt: Vec<String>,
    },
    /// Set LABEL's thread goal (codex then drives its own turns), or manage it.
    Goal {
        label: String,
        /// Restart a stopped goal.
        #[arg(long, group = "goal_action")]
        resume: bool,
        /// Stop codex from starting further turns.
        #[arg(long, group = "goal_action")]
        pause: bool,
        /// Print the objective and status.
        #[arg(long, group = "goal_action")]
        show: bool,
        #[arg(long, group = "goal_action")]
        clear: bool,
        #[command(flatten)]
        wait: WaitArgs,
        /// The objective; `-` or nothing reads stdin.
        #[arg(trailing_var_arg = true)]
        objective: Vec<String>,
    },
    /// Interrupt LABEL's live turn (pausing an active goal first).
    Interrupt {
        label: String,
        #[command(flatten)]
        at: AtArgs,
    },
    /// Shut down LABEL's daemon, or all of this session's in the project.
    Kill {
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        label: Option<String>,
        #[arg(long)]
        all: bool,
        #[command(flatten)]
        at: AtArgs,
    },
    /// Show this project's labels, all sessions.
    List {
        /// Every project.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        at: AtArgs,
    },
    /// The account's rate-limit windows.
    Quota {
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        at: AtArgs,
    },
    /// Block until LABEL's live turn settles and print its report.
    Watch {
        label: String,
        /// Tail the log instead, indefinitely.
        #[arg(long, conflicts_with = "run")]
        follow: bool,
        /// A run reported by `--timeout`: block on it, or print its retained
        /// report if it already settled.
        #[arg(long)]
        run: Option<u64>,
        #[command(flatten)]
        wait: WaitArgs,
    },
    /// Print the operating guide for the agent driving ception.
    Skill,
    #[command(hide = true)]
    Daemon(daemon::Options),
}

#[derive(Args, Clone)]
struct AtArgs {
    /// Resolve the project from this directory instead of the current one.
    #[arg(long)]
    cwd: Option<PathBuf>,
}

#[derive(Args, Clone)]
struct WaitArgs {
    #[command(flatten)]
    at: AtArgs,
    #[arg(long, value_enum, default_value_t)]
    report: ReportLevel,
    /// Once the turn has started, stop waiting after SECS and exit 5; the
    /// turn keeps running (reattach with `watch --run`).
    #[arg(long, value_name = "SECS")]
    timeout: Option<u64>,
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            return if error.use_stderr() { ExitCode::from(4) } else { ExitCode::SUCCESS };
        }
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    match runtime.block_on(client::run(cli.command)) {
        Ok(code) => ExitCode::from(code),
        // Usage and infrastructure errors; turn outcomes are exit codes.
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::from(4)
        }
    }
}
