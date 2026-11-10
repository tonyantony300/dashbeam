//! Remembers partial-receive stores staged in destination folders.
//!
//! Stores in the temp dir are found by scanning it. One staged next to the
//! files it will become can be anywhere, so its path is written here before the
//! download starts. The list is what lets the launch sweep and history delete a
//! leftover store, and it doubles as the allow-list for doing so.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context;

use crate::history::partial_store_hash;

pub const STAGED_STORES_FILE: &str = "staged-recv-stores.json";

/// Entries kept before the oldest are dropped. Only stores on a volume that is
/// never mounted again can pile up, so this is a backstop.
const MAX_ENTRIES: usize = 64;

pub struct StagedStores {
    path: PathBuf,
    file_lock: Mutex<()>,
}

impl StagedStores {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join(STAGED_STORES_FILE),
            file_lock: Mutex::new(()),
        }
    }

    fn lock_file(&self) -> std::sync::MutexGuard<'_, ()> {
        self.file_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn contains(&self, store: &Path) -> bool {
        let _guard = self.lock_file();
        self.read().iter().any(|p| p == store)
    }

    /// Records `store` before a download writes into it. Stores in `temp_dir`
    /// are skipped: the temp scan already covers them.
    pub fn remember(&self, store: &Path, temp_dir: &Path) -> anyhow::Result<()> {
        if store.parent() == Some(temp_dir) || partial_store_hash(store).is_none() {
            return Ok(());
        }
        let _guard = self.lock_file();
        let mut entries = self.read();
        entries.retain(|p| p != store);
        entries.insert(0, store.to_path_buf());
        entries.truncate(MAX_ENTRIES);
        self.write(&entries)
    }

    pub fn forget(&self, store: &Path) -> anyhow::Result<()> {
        let _guard = self.lock_file();
        let mut entries = self.read();
        let before = entries.len();
        entries.retain(|p| p != store);
        if entries.len() == before {
            return Ok(());
        }
        self.write(&entries)
    }

    /// Deletes every remembered store; returns how many were removed. An entry
    /// whose folder cannot be reached (an unplugged drive) is kept for the next
    /// launch.
    pub fn sweep(&self) -> usize {
        let _guard = self.lock_file();
        let entries = self.read();
        let mut kept = Vec::new();
        let mut removed = 0usize;
        for store in &entries {
            // The file is user-writable: never delete a name we did not generate.
            if partial_store_hash(store).is_none() {
                continue;
            }
            if store.is_dir() {
                match std::fs::remove_dir_all(store) {
                    Ok(()) => removed += 1,
                    Err(e) => {
                        tracing::warn!("failed to sweep {}: {e}", store.display());
                        kept.push(store.clone());
                    }
                }
            } else if !store.parent().is_some_and(Path::is_dir) {
                kept.push(store.clone());
            }
        }
        if kept != entries {
            if let Err(e) = self.write(&kept) {
                tracing::warn!("failed to update {}: {e:#}", self.path.display());
            }
        }
        removed
    }

    /// An unreadable list is treated as empty; the worst case is a leftover
    /// store the user deletes by hand.
    fn read(&self) -> Vec<PathBuf> {
        let Ok(raw) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        serde_json::from_str::<Vec<PathBuf>>(&raw).unwrap_or_default()
    }

    fn write(&self, entries: &[PathBuf]) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(entries)?)
            .with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("rename {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn store_in(root: &Path) -> PathBuf {
        let dir = root.join(format!(".dashbeam-recv-{HASH}"));
        std::fs::create_dir_all(dir.join("data")).expect("create store");
        std::fs::write(dir.join("data").join("blob"), b"partial").expect("write");
        dir
    }

    #[test]
    fn a_remembered_store_is_listed_until_forgotten() {
        let data = TempDir::new().expect("data dir");
        let temp = TempDir::new().expect("temp dir");
        let dest = TempDir::new().expect("destination");
        let staged = StagedStores::new(data.path());
        let store = store_in(dest.path());

        staged.remember(&store, temp.path()).expect("remember");
        assert!(staged.contains(&store));

        staged.forget(&store).expect("forget");
        assert!(!staged.contains(&store));
    }

    #[test]
    fn a_store_in_the_temp_dir_is_not_recorded() {
        let data = TempDir::new().expect("data dir");
        let temp = TempDir::new().expect("temp dir");
        let staged = StagedStores::new(data.path());
        let store = store_in(temp.path());

        staged.remember(&store, temp.path()).expect("remember");

        assert!(!staged.contains(&store));
    }

    #[test]
    fn a_foreign_directory_is_never_recorded() {
        let data = TempDir::new().expect("data dir");
        let temp = TempDir::new().expect("temp dir");
        let dest = TempDir::new().expect("destination");
        let staged = StagedStores::new(data.path());

        staged.remember(dest.path(), temp.path()).expect("remember");

        assert!(!staged.contains(dest.path()));
    }

    #[test]
    fn the_sweep_removes_a_leftover_store_and_its_entry() {
        let data = TempDir::new().expect("data dir");
        let temp = TempDir::new().expect("temp dir");
        let dest = TempDir::new().expect("destination");
        let staged = StagedStores::new(data.path());
        let store = store_in(dest.path());
        staged.remember(&store, temp.path()).expect("remember");

        assert_eq!(staged.sweep(), 1);

        assert!(!store.exists());
        assert!(dest.path().exists(), "only the store itself is removed");
        assert!(!staged.contains(&store));
    }

    #[test]
    fn the_sweep_keeps_an_entry_whose_folder_is_unreachable() {
        let data = TempDir::new().expect("data dir");
        let temp = TempDir::new().expect("temp dir");
        let staged = StagedStores::new(data.path());
        let unplugged = TempDir::new().expect("drive");
        let store = unplugged.path().join(format!(".dashbeam-recv-{HASH}"));
        staged.remember(&store, temp.path()).expect("remember");
        drop(unplugged);

        assert_eq!(staged.sweep(), 0);

        assert!(staged.contains(&store), "retried once the drive is back");
    }

    #[test]
    fn the_sweep_ignores_a_tampered_entry() {
        let data = TempDir::new().expect("data dir");
        let precious = TempDir::new().expect("user folder");
        std::fs::write(
            data.path().join(STAGED_STORES_FILE),
            serde_json::to_string(&[precious.path()]).expect("json"),
        )
        .expect("write list");
        let staged = StagedStores::new(data.path());

        assert_eq!(staged.sweep(), 0);

        assert!(precious.path().exists());
        assert!(!staged.contains(precious.path()));
    }
}
