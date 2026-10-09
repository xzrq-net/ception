//! Who is asking: the session that owns labels, and the process whose death
//! ends that session's daemons.
//!
//! Claude Code exports its session id and its own pid to every Bash call. A
//! nested `claude` exports its own, and `--resume` keeps the id under a new
//! pid, so a resumed session finds its labels again. Other harnesses get one
//! `default` session per project whose daemons only end on idle timeout,
//! unless they set the CEPTION_ variables.

use anyhow::{Context, Result};

use crate::paths::validate_name;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub key: String,
    pub watch_pid: Option<u32>,
}

pub fn from_env() -> Result<Session> {
    resolve(|name| std::env::var(name).ok())
}

pub fn resolve(env: impl Fn(&str) -> Option<String>) -> Result<Session> {
    let var = |name: &str| env(name).filter(|value| !value.is_empty());
    let key = var("CEPTION_SESSION")
        .or_else(|| var("CLAUDE_CODE_SESSION_ID"))
        .unwrap_or_else(|| "default".to_string());
    validate_name("session", &key)?;
    let watch_pid = match var("CEPTION_WATCH_PID").or_else(|| var("CLAUDE_PID")) {
        Some(pid) => Some(pid.parse().with_context(|| format!("invalid watch pid {pid:?}"))?),
        None => None,
    };
    Ok(Session { key, watch_pid })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(vars: &[(&str, &str)]) -> Result<Session> {
        resolve(|name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        })
    }

    #[test]
    fn claude_code_session() {
        let s = session(&[("CLAUDE_CODE_SESSION_ID", "abc-123"), ("CLAUDE_PID", "42")]).unwrap();
        assert_eq!(s, Session { key: "abc-123".into(), watch_pid: Some(42) });
    }

    #[test]
    fn other_harness_gets_default_without_watch() {
        assert_eq!(session(&[]).unwrap(), Session { key: "default".into(), watch_pid: None });
    }

    #[test]
    fn explicit_overrides_win_independently() {
        let vars = [
            ("CLAUDE_CODE_SESSION_ID", "abc"),
            ("CLAUDE_PID", "42"),
            ("CEPTION_SESSION", "mine"),
        ];
        assert_eq!(session(&vars).unwrap(), Session { key: "mine".into(), watch_pid: Some(42) });
        let vars = [("CLAUDE_PID", "42"), ("CEPTION_WATCH_PID", "7")];
        assert_eq!(session(&vars).unwrap(), Session { key: "default".into(), watch_pid: Some(7) });
    }

    #[test]
    fn empty_values_are_unset_and_garbage_fails() {
        let s = session(&[("CEPTION_SESSION", ""), ("CLAUDE_PID", "")]).unwrap();
        assert_eq!(s, Session { key: "default".into(), watch_pid: None });
        assert!(session(&[("CEPTION_SESSION", "../x")]).is_err());
        assert!(session(&[("CLAUDE_PID", "nope")]).is_err());
    }
}
