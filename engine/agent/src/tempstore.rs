//! Housekeeping for the node's blob stores under [`crate::paths::temp_path`].
//!
//! Partial receive stores are kept after a failed or cancelled download so a
//! re-send of the same content resumes. The desktop app clears them at launch;
//! a long-running agent needs the same at startup plus an age-based prune.

use std::path::Path;
use std::time::{Duration, SystemTime};

/// Partial stores untouched this long are pruned.
pub const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
/// How often the pruner runs.
pub const PRUNE_EVERY: Duration = Duration::from_secs(60 * 60);

fn is_store(name: &str) -> bool {
    name.starts_with(engine::storage::SEND_DIR_PREFIX)
        || name.starts_with(engine::storage::RECV_DIR_PREFIX)
}

fn remove(path: &Path) -> bool {
    match std::fs::remove_dir_all(path) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("could not remove blob store {}: {e}", path.display());
            false
        }
    }
}

/// Removes every blob store in `dir`. Only safe while no transfer runs, i.e.
/// at startup under the node lock. Returns how many were removed.
pub fn sweep_all(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(is_store) && e.path().is_dir())
        .filter(|e| remove(&e.path()))
        .count()
}

/// Newest modification time of any file under `path`; the dir's own mtime
/// when it holds no files.
fn newest_mtime(path: &Path) -> Option<SystemTime> {
    let mut newest: Option<SystemTime> = None;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() {
                stack.push(e.path());
            } else if let Ok(m) = meta.modified() {
                newest = Some(newest.map_or(m, |n| n.max(m)));
            }
        }
    }
    newest.or_else(|| std::fs::metadata(path).and_then(|m| m.modified()).ok())
}

/// Removes partial receive stores in `dir` with nothing written for `max_age`.
/// A download in progress keeps writing, so it is never old enough.
pub fn prune_stale(dir: &Path, max_age: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let now = SystemTime::now();
    entries
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with(engine::storage::RECV_DIR_PREFIX))
        })
        .filter(|e| {
            newest_mtime(&e.path())
                .and_then(|m| now.duration_since(m).ok())
                .is_some_and(|age| age >= max_age)
        })
        .filter(|e| remove(&e.path()))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn store(root: &Path, name: &str) -> std::path::PathBuf {
        let d = root.join(name).join("data");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("x.data"), b"partial").unwrap();
        root.join(name)
    }

    fn age(path: &Path, by: Duration) {
        fs::File::options()
            .write(true)
            .open(path.join("data").join("x.data"))
            .unwrap()
            .set_modified(SystemTime::now() - by)
            .unwrap();
    }

    #[test]
    fn sweep_removes_only_blob_stores() {
        let root = tempfile::tempdir().unwrap();
        store(root.path(), ".dashbeam-recv-aa");
        store(root.path(), ".dashbeam-send-bb");
        fs::create_dir(root.path().join("keep-me")).unwrap();
        assert_eq!(sweep_all(root.path()), 2);
        let left: Vec<_> = fs::read_dir(root.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(left, ["keep-me"]);
    }

    #[test]
    fn prune_keeps_recent_and_send_stores() {
        let root = tempfile::tempdir().unwrap();
        let old = store(root.path(), ".dashbeam-recv-old");
        store(root.path(), ".dashbeam-recv-new");
        let old_send = store(root.path(), ".dashbeam-send-old");
        age(&old, Duration::from_secs(48 * 3600));
        age(&old_send, Duration::from_secs(48 * 3600));
        assert_eq!(prune_stale(root.path(), STALE_AFTER), 1);
        assert!(!old.exists());
        assert!(root.path().join(".dashbeam-recv-new").exists());
        assert!(old_send.exists(), "send stores are cleaned by their share");
    }

    #[test]
    fn missing_dir_is_fine() {
        let root = tempfile::tempdir().unwrap();
        let gone = root.path().join("nope");
        assert_eq!(sweep_all(&gone), 0);
        assert_eq!(prune_stale(&gone, STALE_AFTER), 0);
    }
}
