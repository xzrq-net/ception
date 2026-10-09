//! Where things live: project identity, state and runtime roots, per-label paths.

use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

/// Directory name under the state and runtime roots. `ception-rs` while the JS
/// version is still in use; flips to `ception` at switchover.
pub const APP_DIR: &str = "ception-rs";

const VCS_MARKERS: [&str; 3] = [".jj", ".git", ".hg"];

/// sockaddr_un.sun_path, including the terminating NUL.
const SUN_PATH_MAX: usize = 108;

/// Labels and session keys become path components.
pub fn validate_name(kind: &str, name: &str) -> Result<()> {
    let charset = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if name.is_empty() || !charset || name.chars().all(|c| c == '.') {
        bail!("{kind} must contain only letters, numbers, dot, underscore, and dash");
    }
    Ok(())
}

pub fn state_root() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home()?.join(".local").join("state"),
    };
    Ok(base.join(APP_DIR))
}

/// Label locks and sockets. Under the state root rather than
/// $XDG_RUNTIME_DIR: containers on one kernel share the state root but not
/// their runtime dirs, and a lock or socket only works if both see one file.
pub fn run_root() -> Result<PathBuf> {
    Ok(state_root()?.join("run"))
}

pub fn projects_root() -> Result<PathBuf> {
    Ok(state_root()?.join("projects"))
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

/// The project a directory belongs to: the nearest ancestor holding a VCS
/// marker, else the directory itself. Labels are scoped to it, so any
/// subdirectory reaches the same labels.
pub fn project_root(cwd: &Path) -> Result<PathBuf> {
    let real = std::fs::canonicalize(cwd).with_context(|| format!("resolve {}", cwd.display()))?;
    for dir in real.ancestors() {
        if VCS_MARKERS.iter().any(|marker| dir.join(marker).exists()) {
            return Ok(dir.to_path_buf());
        }
    }
    Ok(real)
}

pub fn project_hash(root: &Path) -> String {
    hex(&Sha256::digest(root.as_os_str().as_encoded_bytes()))[..12].to_string()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Everything one label touches on disk.
#[derive(Debug, Clone)]
pub struct LabelPaths {
    /// Thread id and spawn options, written only by the label's daemon.
    pub record: PathBuf,
    pub log: PathBuf,
    /// Held by the label's daemon for its whole life.
    pub lock: PathBuf,
    pub socket: PathBuf,
}

impl LabelPaths {
    pub fn new(hash: &str, session: &str, label: &str) -> Result<Self> {
        let dir = session_dir(hash, session)?;
        let run = run_root()?;
        // Hashed so socket paths stay short whatever the session id and label.
        let key = &hex(&Sha256::digest(format!("{hash}/{session}/{label}")))[..16];
        let socket = run.join(format!("{key}.sock"));
        if socket.as_os_str().len() >= SUN_PATH_MAX {
            bail!("socket path too long for a unix socket: {}", socket.display());
        }
        Ok(Self {
            record: dir.join(format!("{label}.json")),
            log: dir.join(format!("{label}.log")),
            lock: run.join(format!("{key}.lock")),
            socket,
        })
    }

    /// Create the record/log directory and the runtime directory.
    pub fn ensure_dirs(&self) -> Result<()> {
        for file in [&self.record, &self.lock] {
            let dir = file.parent().expect("label paths have parents");
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .with_context(|| format!("create {}", dir.display()))?;
        }
        Ok(())
    }
}

pub fn session_dir(hash: &str, session: &str) -> Result<PathBuf> {
    Ok(projects_root()?.join(hash).join(session))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_reject_path_tricks() {
        for bad in ["", ".", "..", "a/b", "a b", "ü"] {
            assert!(validate_name("label", bad).is_err(), "{bad:?} accepted");
        }
        for good in ["impl", "a.b_c-1", ".hidden", "0b1d8f0e-46c2-4f5c-9b1e-1f2d3c4b5a69"] {
            assert!(validate_name("label", good).is_ok(), "{good:?} rejected");
        }
    }

    #[test]
    fn project_root_is_nearest_vcs_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        let deep = root.join("src").join("deep");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(project_root(&deep).unwrap(), deep.canonicalize().unwrap());
        std::fs::create_dir(root.join(".jj")).unwrap();
        assert_eq!(project_root(&deep).unwrap(), root.canonicalize().unwrap());
    }
}
