//! Process primitives: identity via /proc starttime, death notification via
//! pidfd, and detaching the daemon from the client's process tree.

use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;

use anyhow::{Context, Result};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

/// Clock ticks since boot at which the process started. Together with the pid
/// it names one process even across pid reuse.
pub fn starttime(pid: u32) -> Result<u64> {
    let stat =
        std::fs::read_to_string(format!("/proc/{pid}/stat")).with_context(|| format!("process {pid} not found"))?;
    // comm (field 2) is parenthesised and may contain spaces; fields after it
    // start at 3, so starttime (22) is the 20th.
    let rest = &stat[stat.rfind(')').context("malformed /proc stat")? + 2..];
    let field = rest.split_whitespace().nth(19).context("malformed /proc stat")?;
    Ok(field.parse()?)
}

/// The variable that marks an app-server and everything it starts, so the
/// lot can be found again by value, whatever process group or parent they
/// end up with.
pub const OWNER_VAR: &str = "CEPTION_OWNER";

/// Live processes (other than us) whose environment carries
/// `CEPTION_OWNER=<token>`, with their start times.
pub fn owned_processes(token: &str) -> Vec<(u32, u64)> {
    let marker = format!("{OWNER_VAR}={token}");
    let me = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| pid != me)
        .filter(|&pid| {
            std::fs::read(format!("/proc/{pid}/environ"))
                .is_ok_and(|environ| environ.split(|&b| b == 0).any(|var| var == marker.as_bytes()))
        })
        .filter_map(|pid| Some((pid, starttime(pid).ok()?)))
        .collect()
}

/// SIGTERM the processes carrying `token`, then SIGKILL what is left. Each
/// signal goes only to a pid whose start time still matches, so a reused
/// pid is never hit. Returns how many there were.
pub async fn kill_owned(token: &str) -> usize {
    let found = owned_processes(token);
    let alive = |&(pid, start): &(u32, u64)| starttime(pid).ok() == Some(start);
    for (signal, wait_ms) in [(libc::SIGTERM, 2000), (libc::SIGKILL, 1000)] {
        let live: Vec<_> = found.iter().copied().filter(alive).collect();
        if live.is_empty() {
            break;
        }
        for (pid, _) in &live {
            unsafe { libc::kill(*pid as libc::pid_t, signal) };
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(wait_ms);
        while live.iter().any(alive) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }
    found.len()
}

/// Becomes ready when the watched process exits.
pub struct PidWatch {
    fd: AsyncFd<OwnedFd>,
}

impl PidWatch {
    /// `None` if the process is already gone, including when its pid now
    /// belongs to someone else.
    pub fn open(pid: u32, expected_starttime: u64) -> Result<Option<Self>> {
        let Some(watch) = Self::pidfd(pid)? else {
            return Ok(None);
        };
        // Checked after opening: the pidfd pins this pid to whatever process
        // holds it now, so a matching starttime proves it is the right one.
        if starttime(pid).ok() != Some(expected_starttime) {
            return Ok(None);
        }
        Ok(Some(watch))
    }

    /// For our own unreaped child, whose pid cannot have been reused.
    pub fn child(pid: u32) -> Result<Option<Self>> {
        Self::pidfd(pid)
    }

    fn pidfd(pid: u32) -> Result<Option<Self>> {
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        if raw < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(None);
            }
            return Err(error).context("pidfd_open");
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
        Ok(Some(Self { fd: AsyncFd::with_interest(fd, Interest::READABLE)? }))
    }

    pub async fn exited(&self) {
        let _ = self.fd.readable().await;
    }
}

/// Run the child in its own session and reparent it at once: the intermediate
/// forked here exits before exec. Claude Code stops a background shell by
/// killing its process tree, walking ppids, and setsid alone does not escape
/// that. std's exec-error pipe still works: the intermediate's copy closes on
/// `_exit`, the daemon's on exec.
pub fn detach(command: &mut std::process::Command) {
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            match libc::fork() {
                -1 => Err(io::Error::last_os_error()),
                0 => Ok(()),
                _ => libc::_exit(0),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starttime_identifies_self() {
        let pid = std::process::id();
        assert_eq!(starttime(pid).unwrap(), starttime(pid).unwrap());
        assert!(starttime(u32::MAX - 1).is_err());
    }

    #[tokio::test]
    async fn pid_watch_fires_on_exit_and_rejects_reuse() {
        let mut child = std::process::Command::new("sleep").arg("0.2").spawn().unwrap();
        let pid = child.id();
        let start = starttime(pid).unwrap();
        assert!(PidWatch::open(pid, start + 1).unwrap().is_none());
        let watch = PidWatch::open(pid, start).unwrap().expect("live process");
        let reaper = std::thread::spawn(move || child.wait());
        tokio::time::timeout(std::time::Duration::from_secs(5), watch.exited())
            .await
            .expect("pidfd never became ready");
        reaper.join().unwrap().unwrap();
    }
}
