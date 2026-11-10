//! Persistent transfer history for desktop and Android.
//!
//! Rows are written in two phases: opened when a transfer starts, finalized on
//! every exit path. A row still `InProgress` at the next launch was interrupted
//! by a crash or force-quit and is swept to `Interrupted`.
//!
//! Storage mirrors `PairedDeviceStore`: a mutex serializing read-modify-write
//! cycles, a tmp file, and an atomic rename.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::staged_stores::StagedStores;

/// Rows kept before the oldest are dropped.
pub const MAX_RECORDS: usize = 500;

/// File names stored per record — for showing what moved, not rebuilding a tree.
pub const MAX_FILE_NAMES: usize = 20;

/// Prefix `storage::create_recv_store` gives every partial-receive directory.
const PARTIAL_STORE_PREFIX: &str = crate::storage::RECV_DIR_PREFIX;

/// BLAKE3 hash, hex encoded.
const PARTIAL_STORE_HASH_LEN: usize = 64;


pub const HISTORY_FILE: &str = "transfer-history.json";

/// An unparseable history file is moved aside under this name rather than
/// silently replaced, so a serialization bug can't destroy a user's records.
pub const CORRUPT_HISTORY_FILE: &str = "transfer-history.corrupt.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransferDirection {
    Send,
    Receive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TransferStatus {
    InProgress,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransferPathType {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferPeer {
    pub endpoint_id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub device_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferRecord {
    pub id: String,
    pub direction: TransferDirection,
    pub status: TransferStatus,
    pub started_at: u64,
    #[serde(default)]
    pub ended_at: Option<u64>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub export_ms: Option<u64>,
    pub payload_bytes: u64,
    #[serde(default)]
    pub bytes_transferred: u64,
    #[serde(default)]
    pub avg_speed_bps: Option<f64>,
    #[serde(default)]
    pub item_count: u32,
    #[serde(default)]
    pub path_type: Option<TransferPathType>,
    pub root_name: String,
    #[serde(default)]
    pub file_names: Vec<String>,
    #[serde(default)]
    pub file_names_truncated: bool,
    #[serde(default)]
    pub blob_hash: Option<String>,
    #[serde(default)]
    pub peer: Option<TransferPeer>,
    #[serde(default)]
    pub peer_count: u32,
    #[serde(default)]
    pub save_path: Option<String>,
    /// Android only: the SAF tree the files were exported into. `save_path` is
    /// readable but not addressable, so opening the folder later needs this.
    #[serde(default)]
    pub save_uri: Option<String>,
    #[serde(default)]
    pub conflict_count: u32,
    #[serde(default)]
    pub resumable_store_path: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

impl TransferRecord {
    pub fn new(direction: TransferDirection, root_name: String, payload_bytes: u64) -> Self {
        Self {
            id: uuid::Uuid::new_v4().simple().to_string(),
            direction,
            status: TransferStatus::InProgress,
            started_at: protocol::identity::unix_now_ms(),
            ended_at: None,
            duration_ms: None,
            export_ms: None,
            payload_bytes,
            bytes_transferred: 0,
            avg_speed_bps: None,
            item_count: 0,
            path_type: None,
            root_name,
            file_names: Vec::new(),
            file_names_truncated: false,
            blob_hash: None,
            peer: None,
            peer_count: 0,
            save_path: None,
            save_uri: None,
            conflict_count: 0,
            resumable_store_path: None,
            error: None,
        }
    }

    /// Stores at most `MAX_FILE_NAMES` names, flagging when more were dropped.
    pub fn set_file_names(&mut self, names: Vec<String>) {
        self.file_names_truncated = names.len() > MAX_FILE_NAMES;
        self.file_names = names.into_iter().take(MAX_FILE_NAMES).collect();
    }
}

/// The BLAKE3 hash a partial-receive directory holds, if the path names one.
/// Doubles as the name guard for deletion — `transfer-history.json` is
/// user-writable, so a tampered path must not become an arbitrary-delete.
pub fn partial_store_hash(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let hash = name.strip_prefix(PARTIAL_STORE_PREFIX)?;
    if hash.len() != PARTIAL_STORE_HASH_LEN
        || !hash.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    Some(hash.to_string())
}

/// Whether `path` is a partial-receive store this app may delete: the name must
/// be one we generate *and* the directory must either sit directly in the temp
/// dir or be a store we recorded staging in a destination folder.
pub fn is_reclaimable_partial(path: &Path, temp_dir: &Path, staged: &StagedStores) -> bool {
    if partial_store_hash(path).is_none() {
        return false;
    }
    if staged.contains(path) {
        return true;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    match (parent.canonicalize(), temp_dir.canonicalize()) {
        (Ok(parent), Ok(temp)) => parent == temp,
        _ => false,
    }
}

/// Deletes a record's partial store, if it has one and it's safe to delete.
/// Called when a row is deleted, which is the only pointer to that disk space.
pub fn reclaim_partial(record: &TransferRecord, temp_dir: &Path, staged: &StagedStores) -> bool {
    let Some(raw) = record.resumable_store_path.as_deref() else {
        return false;
    };
    let path = PathBuf::from(raw);
    if !is_reclaimable_partial(&path, temp_dir, staged) {
        tracing::warn!("refusing to reclaim non-partial path {}", path.display());
        return false;
    }
    match std::fs::remove_dir_all(&path) {
        Ok(()) => {
            if let Err(e) = staged.forget(&path) {
                tracing::warn!("failed to forget {}: {e:#}", path.display());
            }
            true
        }
        Err(e) => {
            tracing::warn!("failed to reclaim {}: {e}", path.display());
            false
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct TransferHistoryFile {
    version: u32,
    records: Vec<TransferRecord>,
}

impl Default for TransferHistoryFile {
    fn default() -> Self {
        Self {
            version: 1,
            records: Vec::new(),
        }
    }
}

pub struct TransferHistoryStore {
    path: PathBuf,
    /// Serializes read-modify-write cycles so concurrent callers don't race on
    /// the shared `.tmp` rename path.
    file_lock: Mutex<()>,
}

impl TransferHistoryStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join(HISTORY_FILE),
            file_lock: Mutex::new(()),
        }
    }

    /// Guards no in-memory state, so poisoning would only make it unusable.
    fn lock_file(&self) -> std::sync::MutexGuard<'_, ()> {
        self.file_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Newest first.
    pub fn list(&self) -> anyhow::Result<Vec<TransferRecord>> {
        Ok(self.read_file()?.records)
    }

    /// Writes a new row and returns its id.
    pub fn open(&self, record: TransferRecord) -> anyhow::Result<String> {
        let _guard = self.lock_file();
        let mut file = self.read_file()?;
        let id = record.id.clone();
        file.records.insert(0, record);
        file.records.truncate(MAX_RECORDS);
        self.write_file(&file)?;
        Ok(id)
    }

    /// Applies `f` to the row with `id` and persists the result.
    ///
    /// `None` when no such row exists — a finalize after the row was trimmed or
    /// deleted is expected, not an error.
    pub fn update<F>(&self, id: &str, f: F) -> anyhow::Result<Option<TransferRecord>>
    where
        F: FnOnce(&mut TransferRecord),
    {
        let _guard = self.lock_file();
        let mut file = self.read_file()?;
        let Some(existing) = file.records.iter_mut().find(|r| r.id == id) else {
            return Ok(None);
        };
        f(existing);
        let updated = existing.clone();
        self.write_file(&file)?;
        Ok(Some(updated))
    }

    /// Removes one row, returning it so the caller can reclaim its temp data.
    pub fn delete(&self, id: &str) -> anyhow::Result<Option<TransferRecord>> {
        let _guard = self.lock_file();
        let mut file = self.read_file()?;
        let Some(index) = file.records.iter().position(|r| r.id == id) else {
            return Ok(None);
        };
        let removed = file.records.remove(index);
        self.write_file(&file)?;
        Ok(Some(removed))
    }

    /// Removes every row, returning them so callers can reclaim temp data —
    /// the rows are the only pointers to those partial stores.
    pub fn clear(&self) -> anyhow::Result<Vec<TransferRecord>> {
        let _guard = self.lock_file();
        let mut file = self.read_file()?;
        let removed = std::mem::take(&mut file.records);
        self.write_file(&file)?;
        Ok(removed)
    }

    /// Reconciles rows left open by a crash or force-quit. Run once at startup,
    /// before any command can open a new row.
    pub fn mark_interrupted(&self) -> anyhow::Result<usize> {
        let _guard = self.lock_file();
        let mut file = self.read_file()?;
        let now = protocol::identity::unix_now_ms();
        let mut swept = 0usize;
        for record in &mut file.records {
            if record.status == TransferStatus::InProgress {
                record.status = TransferStatus::Interrupted;
                record.ended_at = Some(now);
                swept += 1;
            }
        }
        if swept > 0 {
            self.write_file(&file)?;
        }
        Ok(swept)
    }

    /// Drops pointers to partial stores that are no longer on disk. Called at
    /// launch, after the temp sweep: a row that keeps pointing at a deleted
    /// store offers to free space that is already free.
    pub fn forget_missing_partial_stores(&self) -> anyhow::Result<usize> {
        let _guard = self.lock_file();
        let mut file = self.read_file()?;
        let mut forgotten = 0usize;
        for record in &mut file.records {
            let missing = record
                .resumable_store_path
                .as_deref()
                .is_some_and(|path| !Path::new(path).is_dir());
            if missing {
                record.resumable_store_path = None;
                forgotten += 1;
            }
        }
        if forgotten > 0 {
            self.write_file(&file)?;
        }
        Ok(forgotten)
    }

    fn read_file(&self) -> anyhow::Result<TransferHistoryFile> {
        if !self.path.exists() {
            return Ok(TransferHistoryFile::default());
        }
        let raw = std::fs::read_to_string(&self.path)
            .with_context(|| format!("read {}", self.path.display()))?;
        match serde_json::from_str(&raw) {
            Ok(file) => Ok(file),
            Err(error) => {
                let quarantine = self.path.with_file_name(CORRUPT_HISTORY_FILE);
                tracing::warn!(
                    "transfer history unreadable ({error}); moving it to {}",
                    quarantine.display()
                );
                if let Err(e) = std::fs::rename(&self.path, &quarantine) {
                    tracing::warn!("failed to quarantine corrupt transfer history: {e}");
                }
                Ok(TransferHistoryFile::default())
            }
        }
    }

    fn write_file(&self, file: &TransferHistoryFile) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        // fsync before the rename: on some filesystems `write` + `rename` can
        // surface as a zero-length file after power loss, losing everything.
        {
            use std::io::Write;
            let mut handle = std::fs::File::create(&tmp)?;
            handle.write_all(serde_json::to_string_pretty(file)?.as_bytes())?;
            handle.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn record(name: &str) -> TransferRecord {
        TransferRecord::new(TransferDirection::Send, name.to_string(), 1_000)
    }

    #[test]
    fn opened_record_is_listed_back() {
        let dir = TempDir::new().expect("tempdir");
        let store = TransferHistoryStore::new(dir.path());

        let id = store
            .open(TransferRecord::new(
                TransferDirection::Send,
                "report.pdf".to_string(),
                1_000,
            ))
            .expect("open");

        let records = store.list().expect("list");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, id);
        assert_eq!(records[0].root_name, "report.pdf");
        assert_eq!(records[0].payload_bytes, 1_000);
        assert_eq!(records[0].status, TransferStatus::InProgress);
    }

    #[test]
    fn newest_record_is_listed_first() {
        let dir = TempDir::new().expect("tempdir");
        let store = TransferHistoryStore::new(dir.path());

        store.open(record("first.pdf")).expect("open first");
        store.open(record("second.pdf")).expect("open second");

        let records = store.list().expect("list");
        assert_eq!(records[0].root_name, "second.pdf");
        assert_eq!(records[1].root_name, "first.pdf");
    }

    #[test]
    fn oldest_records_are_dropped_past_the_cap() {
        let dir = TempDir::new().expect("tempdir");
        let store = TransferHistoryStore::new(dir.path());

        for i in 0..MAX_RECORDS + 5 {
            store.open(record(&format!("file-{i}.pdf"))).expect("open");
        }

        let records = store.list().expect("list");
        assert_eq!(records.len(), MAX_RECORDS);
        assert_eq!(records[0].root_name, format!("file-{}.pdf", MAX_RECORDS + 4));
        assert_eq!(records[MAX_RECORDS - 1].root_name, "file-5.pdf");
    }

    #[test]
    fn update_mutates_the_named_record_only() {
        let dir = TempDir::new().expect("tempdir");
        let store = TransferHistoryStore::new(dir.path());
        let target = store.open(record("target.pdf")).expect("open");
        store.open(record("other.pdf")).expect("open other");

        let updated = store
            .update(&target, |r| {
                r.status = TransferStatus::Completed;
                r.duration_ms = Some(2_000);
            })
            .expect("update")
            .expect("record present");

        assert_eq!(updated.status, TransferStatus::Completed);
        let records = store.list().expect("list");
        let other = records
            .iter()
            .find(|r| r.root_name == "other.pdf")
            .expect("other present");
        assert_eq!(other.status, TransferStatus::InProgress);
    }

    #[test]
    fn update_of_unknown_id_reports_absence() {
        let dir = TempDir::new().expect("tempdir");
        let store = TransferHistoryStore::new(dir.path());

        let updated = store
            .update("nope", |r| r.status = TransferStatus::Completed)
            .expect("update");

        assert!(updated.is_none());
    }

    #[test]
    fn delete_returns_the_removed_record_so_temp_data_can_be_reclaimed() {
        let dir = TempDir::new().expect("tempdir");
        let store = TransferHistoryStore::new(dir.path());
        let id = store.open(record("gone.pdf")).expect("open");

        let removed = store.delete(&id).expect("delete").expect("was present");

        assert_eq!(removed.root_name, "gone.pdf");
        assert!(store.list().expect("list").is_empty());
    }

    #[test]
    fn clear_returns_every_removed_record() {
        let dir = TempDir::new().expect("tempdir");
        let store = TransferHistoryStore::new(dir.path());
        store.open(record("a.pdf")).expect("open a");
        store.open(record("b.pdf")).expect("open b");

        let removed = store.clear().expect("clear");

        assert_eq!(removed.len(), 2);
        assert!(store.list().expect("list").is_empty());
    }

    #[test]
    fn in_progress_rows_are_swept_to_interrupted() {
        let dir = TempDir::new().expect("tempdir");
        let store = TransferHistoryStore::new(dir.path());
        let stale = store.open(record("stale.pdf")).expect("open");
        let done = store.open(record("done.pdf")).expect("open");
        store
            .update(&done, |r| r.status = TransferStatus::Completed)
            .expect("finalize");

        let swept = store.mark_interrupted().expect("sweep");

        assert_eq!(swept, 1);
        let records = store.list().expect("list");
        let stale = records.iter().find(|r| r.id == stale).expect("stale");
        assert_eq!(stale.status, TransferStatus::Interrupted);
        assert!(stale.ended_at.is_some());
    }

    #[test]
    fn a_corrupt_file_is_quarantined_rather_than_silently_dropped() {
        let dir = TempDir::new().expect("tempdir");
        let store = TransferHistoryStore::new(dir.path());
        store.open(record("before.pdf")).expect("open");
        std::fs::write(dir.path().join(HISTORY_FILE), "{ not json").expect("corrupt");

        let records = store.list().expect("list survives corruption");

        assert!(records.is_empty());
        let quarantined = std::fs::read_to_string(dir.path().join(CORRUPT_HISTORY_FILE))
            .expect("corrupt file preserved");
        assert_eq!(quarantined, "{ not json");
    }

    #[test]
    fn recording_recovers_after_a_corrupt_file() {
        let dir = TempDir::new().expect("tempdir");
        let store = TransferHistoryStore::new(dir.path());
        std::fs::write(dir.path().join(HISTORY_FILE), "{ not json").expect("corrupt");

        store.open(record("after.pdf")).expect("open after corruption");

        let records = store.list().expect("list");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].root_name, "after.pdf");
    }

    const HASH_A: &str =
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HASH_B: &str =
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn partial_dir(root: &Path, hash: &str) -> PathBuf {
        let dir = root.join(format!(".dashbeam-recv-{hash}"));
        std::fs::create_dir_all(&dir).expect("create partial");
        dir
    }

    #[test]
    fn file_names_past_the_cap_are_truncated_and_flagged() {
        let mut record = record("photos");
        let names: Vec<String> = (0..MAX_FILE_NAMES + 3)
            .map(|i| format!("photo-{i}.jpg"))
            .collect();

        record.set_file_names(names);

        assert_eq!(record.file_names.len(), MAX_FILE_NAMES);
        assert!(record.file_names_truncated);
        assert_eq!(record.file_names[0], "photo-0.jpg");
    }

    #[test]
    fn a_short_file_list_is_kept_whole_and_unflagged() {
        let mut record = record("pair");

        record.set_file_names(vec!["a.txt".to_string(), "b.txt".to_string()]);

        assert_eq!(record.file_names.len(), 2);
        assert!(!record.file_names_truncated);
    }

    #[test]
    fn partial_store_hash_reads_a_well_formed_dir_name() {
        let path = PathBuf::from(format!("/tmp/.dashbeam-recv-{HASH_A}"));
        assert_eq!(partial_store_hash(&path).as_deref(), Some(HASH_A));
    }

    #[test]
    fn partial_store_hash_rejects_a_foreign_dir_name() {
        assert!(partial_store_hash(Path::new("/tmp/important-documents")).is_none());
        assert!(partial_store_hash(Path::new("/tmp/.dashbeam-recv-short")).is_none());
        assert!(partial_store_hash(Path::new("/tmp/.dashbeam-send-abc")).is_none());
    }

    #[test]
    fn a_tampered_path_outside_temp_is_not_reclaimable() {
        let temp = TempDir::new().expect("tempdir");
        let elsewhere = TempDir::new().expect("other tempdir");
        let outside = partial_dir(elsewhere.path(), HASH_A);

        let staged = StagedStores::new(temp.path());

        assert!(!is_reclaimable_partial(&outside, temp.path(), &staged));
    }

    #[test]
    fn a_partial_inside_temp_is_reclaimable() {
        let temp = TempDir::new().expect("tempdir");
        let inside = partial_dir(temp.path(), HASH_A);

        let staged = StagedStores::new(temp.path());

        assert!(is_reclaimable_partial(&inside, temp.path(), &staged));
    }





    #[test]
    fn reclaiming_a_record_removes_its_partial_store() {
        let temp = TempDir::new().expect("tempdir");
        let partial = partial_dir(temp.path(), HASH_A);
        let mut record = record("interrupted");
        record.resumable_store_path = Some(partial.to_string_lossy().to_string());

        let staged = StagedStores::new(temp.path());

        assert!(reclaim_partial(&record, temp.path(), &staged));
        assert!(!partial.exists());
    }

    #[test]
    fn reclaiming_removes_a_store_staged_in_a_destination_folder() {
        let data = TempDir::new().expect("data dir");
        let temp = TempDir::new().expect("tempdir");
        let dest = TempDir::new().expect("destination");
        let partial = partial_dir(dest.path(), HASH_A);
        let staged = StagedStores::new(data.path());
        staged.remember(&partial, temp.path()).expect("remember");
        let mut record = record("interrupted");
        record.resumable_store_path = Some(partial.to_string_lossy().to_string());

        assert!(reclaim_partial(&record, temp.path(), &staged));

        assert!(!partial.exists());
        assert!(dest.path().exists(), "the destination folder is untouched");
        assert!(!staged.contains(&partial));
    }

    #[test]
    fn reclaiming_refuses_a_record_pointing_outside_temp() {
        let temp = TempDir::new().expect("tempdir");
        let elsewhere = TempDir::new().expect("other tempdir");
        let outside = partial_dir(elsewhere.path(), HASH_A);
        let mut record = record("tampered");
        record.resumable_store_path = Some(outside.to_string_lossy().to_string());

        let staged = StagedStores::new(temp.path());

        assert!(!reclaim_partial(&record, temp.path(), &staged));
        assert!(outside.exists());
    }

    #[test]
    fn a_partial_store_the_launch_sweep_removed_is_forgotten() {
        let data = TempDir::new().expect("data dir");
        let temp = TempDir::new().expect("temp dir");
        let store = TransferHistoryStore::new(data.path());
        let surviving = partial_dir(temp.path(), HASH_A);
        let swept = temp.path().join(format!(".dashbeam-recv-{HASH_B}"));

        let kept = store.open(record("kept")).expect("open kept");
        let stale = store.open(record("stale")).expect("open stale");
        for (id, path) in [(&kept, &surviving), (&stale, &swept)] {
            store
                .update(id, |r| {
                    r.resumable_store_path = Some(path.to_string_lossy().to_string())
                })
                .expect("point at a store");
        }

        let forgotten = store.forget_missing_partial_stores().expect("forget");

        assert_eq!(forgotten, 1);
        let records = store.list().expect("list");
        let kept = records.iter().find(|r| r.id == kept).expect("kept row");
        let stale = records.iter().find(|r| r.id == stale).expect("stale row");
        assert!(
            kept.resumable_store_path.is_some(),
            "a store still on disk stays resumable"
        );
        assert_eq!(stale.resumable_store_path, None);
    }
}
