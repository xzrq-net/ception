//! Label records and locks. One JSON file per label, written only by that
//! label's daemon while it holds the label lock, so nothing needs a
//! project-wide lock.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::{LabelPaths, projects_root};

/// What it takes to bring a label's daemon back: the thread and how it was
/// spawned. "Last used" is the file's mtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub cwd: PathBuf,
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

pub fn read(path: &Path) -> Result<Option<Record>> {
    match fs::read(path) {
        Ok(bytes) => {
            let record = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse {}", path.display()))?;
            Ok(Some(record))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

/// Atomic replace. The fixed temp name is safe because only the lock holder
/// writes; a crash leaves it for [`gc`].
pub fn write(path: &Path, record: &Record) -> Result<()> {
    let dir = path.parent().expect("record paths have parents");
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let temp = path.with_extension("json.tmp");
    let mut body = serde_json::to_vec_pretty(record)?;
    body.push(b'\n');
    fs::write(&temp, body).with_context(|| format!("write {}", temp.display()))?;
    fs::rename(&temp, path).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Take a label lock without waiting; `None` if it is held.
pub fn try_lock(path: &Path) -> Result<Option<File>> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => Err(error).with_context(|| format!("lock {}", path.display())),
    }
}

/// Whether a daemon (live or starting) holds this label lock.
pub fn lock_held(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    matches!(try_lock(path), Ok(None))
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub hash: String,
    pub session: String,
    pub label: String,
    pub record: Record,
    pub modified: SystemTime,
}

impl Entry {
    pub fn paths(&self) -> Result<LabelPaths> {
        LabelPaths::new(&self.hash, &self.session, &self.label)
    }
}

pub fn list_project(hash: &str) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for (session, dir) in subdirs(&projects_root()?.join(hash)) {
        for file in files(&dir) {
            let Some(label) = file.name.strip_suffix(".json") else {
                continue;
            };
            // Unreadable records are skipped here and aged out by gc.
            if let Ok(Some(record)) = read(&file.path) {
                entries.push(Entry {
                    hash: hash.to_string(),
                    session: session.clone(),
                    label: label.to_string(),
                    record,
                    modified: file.modified,
                });
            }
        }
    }
    entries.sort_by(|a, b| (&a.session, &a.label).cmp(&(&b.session, &b.label)));
    Ok(entries)
}

pub fn list_all() -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for (hash, _) in subdirs(&projects_root()?) {
        entries.extend(list_project(&hash)?);
    }
    Ok(entries)
}

const TEMP_MAX_AGE: Duration = Duration::from_secs(3600);

/// Forget other sessions' labels in this project that have been idle longer
/// than `max_age` and have no daemon: record and log both. Also sweeps temp
/// files a crash left behind. The calling session's labels are kept for as
/// long as it lives.
pub fn gc(hash: &str, my_session: &str, max_age: Duration) -> Result<()> {
    let now = SystemTime::now();
    let age = |modified: SystemTime| now.duration_since(modified).unwrap_or_default();
    for (session, dir) in subdirs(&projects_root()?.join(hash)) {
        let mut newest: BTreeMap<String, SystemTime> = BTreeMap::new();
        for file in files(&dir) {
            if let Some(label) = file.name.strip_suffix(".json.tmp") {
                // A daemon may be rewriting this very file; only its lock
                // holder could be, so take the lock and look again.
                if age(file.modified) > TEMP_MAX_AGE {
                    let paths = LabelPaths::new(hash, &session, label)?;
                    if let Ok(Some(_lock)) = try_lock(&paths.lock) {
                        let modified = fs::metadata(&file.path).and_then(|meta| meta.modified());
                        if modified.is_ok_and(|modified| age(modified) > TEMP_MAX_AGE) {
                            let _ = fs::remove_file(&file.path);
                        }
                    }
                }
                continue;
            }
            let Some(label) = file.name.strip_suffix(".json").or(file.name.strip_suffix(".log")) else {
                continue;
            };
            let seen = newest.entry(label.to_string()).or_insert(file.modified);
            *seen = (*seen).max(file.modified);
        }
        if session == my_session {
            continue;
        }
        for (label, modified) in newest {
            if age(modified) <= max_age {
                continue;
            }
            // Deleted under the label lock, rechecking age: a daemon starting
            // for this label meanwhile either holds the lock or has already
            // refreshed the files.
            let paths = LabelPaths::new(hash, &session, &label)?;
            let Ok(Some(_lock)) = try_lock(&paths.lock) else {
                continue;
            };
            let still_stale = [&paths.record, &paths.log].iter().all(|file| {
                fs::metadata(file)
                    .and_then(|meta| meta.modified())
                    .map_or(true, |modified| age(modified) > max_age)
            });
            if still_stale {
                let _ = fs::remove_file(&paths.record);
                let _ = fs::remove_file(&paths.log);
            }
        }
        // Succeeds only once the session has nothing left.
        let _ = fs::remove_dir(&dir);
    }
    Ok(())
}

struct DirFile {
    name: String,
    path: PathBuf,
    modified: SystemTime,
}

fn files(dir: &Path) -> Vec<DirFile> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            meta.is_file().then(|| DirFile {
                name: entry.file_name().to_string_lossy().into_owned(),
                path: entry.path(),
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            })
        })
        .collect()
}

fn subdirs(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| (entry.file_name().to_string_lossy().into_owned(), entry.path()))
        .collect()
}
