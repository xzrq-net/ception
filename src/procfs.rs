//! Process primitives: identity via /proc starttime, death notification via
//! pidfd, finding our children, and detaching the daemon from the client's
//! process tree.

use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;

use anyhow::{Context, Result};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

/// Clock ticks since boot at which the process started. Together with the pid
/// it names one process even across pid reuse.
pub fn starttime(pid: u32) -> Result<u64> {
    let fields = stat_fields(pid).with_context(|| format!("process {pid} not found"))?;
    // Fields after comm start at 3 (state), so starttime (22) is the 20th.
    fields.get(19).context("malformed /proc stat")?.parse().context("malformed /proc stat")
}

/// The fields of /proc/<pid>/stat after "(comm) ", which may contain spaces.
fn stat_fields(pid: u32) -> Option<Vec<String>> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.get(stat.rfind(')')? + 2..)?;
    Some(rest.split_whitespace().map(str::to_string).collect())
}

/// Processes whose parent is `parent`. For the daemon (a child subreaper,
/// and the only one reaping its children) a listed pid stays its child,
/// alive or a zombie, until it reaps it, so signalling it can't hit anything
/// else.
pub fn children_of(parent: u32) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| {
            stat_fields(pid).is_some_and(|fields| fields.get(1).and_then(|ppid| ppid.parse().ok()) == Some(parent))
        })
        .collect()
}

/// Becomes ready when the watched process exits.
pub struct PidWatch {
    fd: AsyncFd<OwnedFd>,
}

impl PidWatch {
    /// `None` if the process is already gone, including when its pid now
    /// belongs to someone else.
    pub fn open(pid: u32, expected_starttime: u64) -> Result<Option<Self>> {
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        if raw < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(None);
            }
            return Err(error).context("pidfd_open");
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
        // Checked after opening: the pidfd pins this pid to whatever process
        // holds it now, so a matching starttime proves it is the right one.
        if starttime(pid).ok() != Some(expected_starttime) {
            return Ok(None);
        }
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

    #[test]
    fn children_of_finds_a_child() {
        let mut child = std::process::Command::new("sleep").arg("5").spawn().unwrap();
        assert!(children_of(std::process::id()).contains(&child.id()));
        child.kill().unwrap();
        child.wait().unwrap();
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
