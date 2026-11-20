//! Transfer-history recording for the Tauri shell.
//!
//! [`HistoryRecordingEmitter`] wraps the emitter the engine reports through, so
//! rows open and finalize from the same events the UI sees. The engine's
//! `let _ = handle.emit_event(...)` discards a return value, not the call, so
//! recording can't miss a terminal event.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

use engine::{
    unix_now_ms, EventEmitter, TransferDirection, TransferHistoryStore, TransferPathType,
    TransferPeer, TransferRecord, TransferStatus,
};

/// Reads `enableTransferHistory` out of `settings.json` — same envelope and
/// same raw-read reasoning as `commands::parse_persisted_minimize_to_tray`.
pub fn parse_persisted_history_enabled(raw: &str) -> Option<bool> {
    let file: serde_json::Value = serde_json::from_str(raw).ok()?;
    let envelope: serde_json::Value =
        serde_json::from_str(file.get("app_settings")?.as_str()?).ok()?;
    envelope
        .get("state")?
        .get("enableTransferHistory")?
        .as_bool()
}

/// Recording is on unless the user turned it off. A missing key means an
/// install that predates the setting, which should keep its history.
pub fn history_enabled_from_raw(raw: Option<&str>) -> bool {
    raw.and_then(parse_persisted_history_enabled)
        .unwrap_or(true)
}

/// Reads the user's recording preference from the app data dir.
pub fn history_enabled(data_dir: &Path) -> bool {
    let raw = std::fs::read_to_string(data_dir.join("settings.json")).ok();
    history_enabled_from_raw(raw.as_deref())
}

/// Announced whenever a row reaches a terminal state or has its destination
/// corrected, so a history list already on screen can re-read. Matched by name
/// in `frontend/src/routes/history.tsx`.
pub const HISTORY_UPDATED_EVENT: &str = "transfer-history-updated";

/// What a `transfer-completed` / `receive-completed` payload tells us.
#[derive(Debug, Default, PartialEq)]
pub struct CompletionFacts {
    /// Time on the wire, excluding connection setup and disk export.
    pub duration_ms: Option<u64>,
    /// Receiver only: time spent writing the files out to disk.
    pub export_ms: Option<u64>,
    /// Payload bytes, already excluding hash-seq root and collection metadata.
    pub bytes: Option<u64>,
}

impl CompletionFacts {
    pub fn parse(payload: &str) -> Self {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
            return Self::default();
        };
        Self {
            duration_ms: value.get("durationMs").and_then(|v| v.as_u64()),
            export_ms: value.get("exportMs").and_then(|v| v.as_u64()),
            bytes: value.get("bytes").and_then(|v| v.as_u64()),
        }
    }
}

/// Bytes per second over the engine's wire time — wall-clock would also cover
/// connection setup and disk export.
pub fn average_speed_bps(payload_bytes: u64, duration_ms: Option<u64>) -> Option<f64> {
    match duration_ms {
        Some(ms) if ms > 0 => Some(payload_bytes as f64 / (ms as f64 / 1000.0)),
        _ => None,
    }
}

/// What a receive contained, from `receive-file-names` — which arrives just
/// before completion, unlike the ticket preview the UI fetched earlier.
#[derive(Debug, Default, PartialEq)]
pub struct ReceivedShape {
    /// Raw, never localized — the UI formats "3 items" so rows re-localize.
    pub root_name: String,
    pub item_count: u32,
    pub path_type: Option<TransferPathType>,
}

impl ReceivedShape {
    pub fn from_file_names(file_names: &[String]) -> Self {
        let mut top_level: Vec<&str> = Vec::new();
        for name in file_names {
            let head = name.split('/').next().unwrap_or(name);
            if !top_level.contains(&head) {
                top_level.push(head);
            }
        }

        match top_level.as_slice() {
            [] => Self::default(),
            [only] => {
                let is_dir = file_names.iter().any(|n| n.contains('/'));
                Self {
                    root_name: (*only).to_string(),
                    item_count: 1,
                    path_type: Some(if is_dir {
                        TransferPathType::Directory
                    } else {
                        TransferPathType::File
                    }),
                }
            }
            many => Self {
                root_name: String::new(),
                item_count: many.len() as u32,
                path_type: None,
            },
        }
    }
}

/// Everything known about a transfer before its first byte moves.
#[derive(Debug, Clone, Default)]
pub struct TransferContext {
    pub root_name: String,
    pub payload_bytes: u64,
    pub item_count: u32,
    pub path_type: Option<TransferPathType>,
    pub blob_hash: Option<String>,
    pub save_path: Option<String>,
    /// Receive only, set at open so a crash still leaves a reclaimable pointer.
    pub resumable_store_path: Option<String>,
    pub peer: Option<TransferPeer>,
}

/// Mutable state a transfer accumulates between opening and finalizing.
#[derive(Debug, Default)]
struct OpenRow {
    id: Option<String>,
    /// First terminal state wins. `stop_sharing` fires after a completed
    /// broadcast share, and must not rewrite it as cancelled.
    finalized: bool,
    /// Last progress seen, so a cancel can say how far it got. Not written
    /// through per tick — that's a file rewrite per megabyte.
    bytes_transferred: u64,
    file_names: Vec<String>,
    peer_count: u32,
}

pub struct HistoryRecordingEmitter {
    inner: Arc<dyn EventEmitter>,
    store: Arc<TransferHistoryStore>,
    direction: TransferDirection,
    context: Mutex<TransferContext>,
    row: Mutex<OpenRow>,
}

impl HistoryRecordingEmitter {
    pub fn new(
        inner: Arc<dyn EventEmitter>,
        store: Arc<TransferHistoryStore>,
        direction: TransferDirection,
        context: TransferContext,
    ) -> Self {
        Self {
            inner,
            store,
            direction,
            context: Mutex::new(context),
            row: Mutex::new(OpenRow::default()),
        }
    }

    fn lock_row(&self) -> std::sync::MutexGuard<'_, OpenRow> {
        self.row
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_context(&self) -> std::sync::MutexGuard<'_, TransferContext> {
        self.context
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Fills in context learned after the emitter was built — the blob hash is
    /// only known once the share session has started.
    pub fn update_context(&self, f: impl FnOnce(&mut TransferContext)) {
        f(&mut self.lock_context());
    }

    /// Records the device a share was invited to — known before the connection
    /// exists, and the only synchronous source of a Nearby device's name.
    pub fn note_invited_peer(&self, peer: TransferPeer) {
        self.lock_context().peer = Some(peer);
    }

    /// Tells the UI a row changed on disk. Emitted through `inner` rather than
    /// `self`: routing it back through `note_event` would be re-entrant, and
    /// the recorder has nothing to record about its own announcement.
    fn notify_changed(&self) {
        let _ = self.inner.emit_event(HISTORY_UPDATED_EVENT);
    }

    fn open_row(&self) {
        let mut row = self.lock_row();
        if row.id.is_some() {
            return;
        }
        let context = self.lock_context().clone();
        let mut record = TransferRecord::new(
            self.direction,
            context.root_name.clone(),
            context.payload_bytes,
        );
        record.item_count = context.item_count;
        record.path_type = context.path_type;
        record.blob_hash = context.blob_hash.clone();
        record.save_path = context.save_path.clone();
        record.resumable_store_path = context.resumable_store_path.clone();
        record.peer = context.peer.clone();

        match self.store.open(record) {
            Ok(id) => row.id = Some(id),
            Err(e) => tracing::warn!("failed to open transfer history row: {e}"),
        }
    }

    /// Writes the terminal state of the row, if one was opened.
    pub fn finalize(&self, status: TransferStatus, facts: CompletionFacts, error: Option<String>) {
        let (id, bytes_transferred, file_names, peer_count) = {
            let mut row = self.lock_row();
            let Some(id) = row.id.clone() else {
                return;
            };
            if row.finalized {
                return;
            }
            row.finalized = true;
            (
                id,
                row.bytes_transferred,
                row.file_names.clone(),
                row.peer_count,
            )
        };
        let shape = ReceivedShape::from_file_names(&file_names);
        let is_receive = self.direction == TransferDirection::Receive;
        let completed = status == TransferStatus::Completed;

        let result = self.store.update(&id, |record| {
            record.status = status;
            record.ended_at = Some(unix_now_ms());
            record.duration_ms = facts.duration_ms;
            record.export_ms = facts.export_ms;
            if let Some(bytes) = facts.bytes {
                record.payload_bytes = bytes;
                record.bytes_transferred = bytes;
            } else {
                record.bytes_transferred = bytes_transferred;
            }
            record.avg_speed_bps = average_speed_bps(record.payload_bytes, facts.duration_ms);
            if !file_names.is_empty() {
                record.set_file_names(file_names);
            }
            // A send knows its own shape up front; a receive only learns it
            // from the collection it just pulled.
            if is_receive && shape.item_count > 0 {
                record.root_name = shape.root_name;
                record.item_count = shape.item_count;
                record.path_type = shape.path_type;
            }
            record.peer_count = peer_count.max(u32::from(record.peer.is_some()));
            // A broadcast serves several peers, so keep the count, not a peer.
            if record.peer_count > 1 {
                record.peer = None;
            }
            if completed {
                // The cleanup guard removed the partial store on success.
                record.resumable_store_path = None;
            }
            record.error = error;
        });

        match result {
            Ok(Some(_)) => self.notify_changed(),
            // `None` means the row is gone — cleared from the history page
            // mid-transfer — so there is nothing for the list to re-read.
            Ok(None) => {}
            Err(e) => tracing::warn!("failed to finalize transfer history row: {e}"),
        }
    }

    /// Corrects the destination after the row is closed. Android finishes a
    /// receive into app-private staging and only then copies the files out to
    /// the picked folder or `Download/DashBeam`, so the path known at open is
    /// never where they come to rest.
    // Only Android exports after the fact; desktop writes to its destination.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn note_destination(&self, path: String, uri: Option<String>) {
        let Some(id) = self.lock_row().id.clone() else {
            return;
        };
        let result = self.store.update(&id, |record| {
            record.save_path = Some(path);
            record.save_uri = uri;
        });
        match result {
            Ok(Some(_)) => self.notify_changed(),
            Ok(None) => {}
            Err(e) => tracing::warn!("failed to record transfer destination: {e}"),
        }
    }

    fn note_event(&self, event_name: &str, payload: Option<&str>) {
        match event_name {
            "transfer-started" | "receive-started" => self.open_row(),
            "transfer-completed" | "receive-completed" => {
                let facts = payload.map(CompletionFacts::parse).unwrap_or_default();
                self.finalize(TransferStatus::Completed, facts, None);
            }
            "transfer-failed" => {
                self.finalize(TransferStatus::Failed, CompletionFacts::default(), None);
            }
            "transfer-progress" | "receive-progress" => {
                if let Some(bytes) = payload.and_then(progress_bytes) {
                    self.lock_row().bytes_transferred = bytes;
                }
            }
            "receive-file-names" => {
                if let Some(names) = payload.and_then(parse_file_names) {
                    self.lock_row().file_names = names;
                }
            }
            "share-peer-connected" => {
                let mut row = self.lock_row();
                row.peer_count = row.peer_count.saturating_add(1);
            }
            _ => {}
        }
    }
}

impl EventEmitter for HistoryRecordingEmitter {
    fn emit_event(&self, event_name: &str) -> Result<(), String> {
        self.note_event(event_name, None);
        self.inner.emit_event(event_name)
    }

    fn emit_event_with_payload(&self, event_name: &str, payload: &str) -> Result<(), String> {
        self.note_event(event_name, Some(payload));
        self.inner.emit_event_with_payload(event_name, payload)
    }
}

/// `<bytes>:<total>:<speed x1000>` — the shared progress payload shape.
fn progress_bytes(payload: &str) -> Option<u64> {
    payload.split(':').next()?.parse::<u64>().ok()
}

fn parse_file_names(payload: &str) -> Option<Vec<String>> {
    serde_json::from_str::<Vec<String>>(payload).ok()
}

/// Where a partial receive for `hash` bound for `output_dir` is staged.
pub fn partial_store_path_for(hash: &str, output_dir: &std::path::Path) -> PathBuf {
    engine::storage::recv_store_dir(hash, output_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::EventEmitter as _;
    use tempfile::TempDir;

    /// Stands in for `TauriEventEmitter` so the recorder needs no Tauri app.
    struct SinkEmitter;

    impl EventEmitter for SinkEmitter {
        fn emit_event(&self, _event_name: &str) -> Result<(), String> {
            Ok(())
        }
        fn emit_event_with_payload(&self, _n: &str, _p: &str) -> Result<(), String> {
            Ok(())
        }
    }

    /// Records what the recorder forwards, so tests can assert on the change
    /// notification rather than only on the rows it writes.
    #[derive(Default)]
    struct CapturingEmitter {
        events: Mutex<Vec<String>>,
    }

    impl CapturingEmitter {
        fn times_seen(&self, event_name: &str) -> usize {
            self.events
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .filter(|seen| *seen == event_name)
                .count()
        }

        fn note(&self, event_name: &str) {
            self.events
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(event_name.to_string());
        }
    }

    impl EventEmitter for CapturingEmitter {
        fn emit_event(&self, event_name: &str) -> Result<(), String> {
            self.note(event_name);
            Ok(())
        }
        fn emit_event_with_payload(&self, event_name: &str, _p: &str) -> Result<(), String> {
            self.note(event_name);
            Ok(())
        }
    }

    /// A recorder whose forwarded events are inspectable.
    fn watched_recorder(
        dir: &TempDir,
        direction: TransferDirection,
        context: TransferContext,
    ) -> (Arc<CapturingEmitter>, HistoryRecordingEmitter) {
        let store = Arc::new(TransferHistoryStore::new(dir.path()));
        let events = Arc::new(CapturingEmitter::default());
        let emitter = HistoryRecordingEmitter::new(events.clone(), store, direction, context);
        (events, emitter)
    }

    fn recorder(
        dir: &TempDir,
        direction: TransferDirection,
        context: TransferContext,
    ) -> (Arc<TransferHistoryStore>, HistoryRecordingEmitter) {
        let store = Arc::new(TransferHistoryStore::new(dir.path()));
        let emitter =
            HistoryRecordingEmitter::new(Arc::new(SinkEmitter), store.clone(), direction, context);
        (store, emitter)
    }

    fn send_context() -> TransferContext {
        TransferContext {
            root_name: "report.pdf".to_string(),
            payload_bytes: 2_048,
            item_count: 1,
            path_type: Some(TransferPathType::File),
            blob_hash: Some("abc".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn a_share_nobody_downloads_leaves_no_row() {
        let dir = TempDir::new().expect("tempdir");
        let (store, emitter) = recorder(&dir, TransferDirection::Send, send_context());

        // The share started, but no peer ever pulled: no `transfer-started`.
        emitter.emit_event("share-started").expect("emit");

        assert!(store.list().expect("list").is_empty());
    }

    #[test]
    fn a_completed_send_records_duration_speed_and_peer() {
        let dir = TempDir::new().expect("tempdir");
        let mut context = send_context();
        context.peer = Some(TransferPeer {
            endpoint_id: "abc123".to_string(),
            display_name: Some("Tony's Laptop".to_string()),
            device_type: Some("laptop".to_string()),
        });
        let (store, emitter) = recorder(&dir, TransferDirection::Send, context);

        emitter.emit_event("transfer-started").expect("emit");
        emitter
            .emit_event_with_payload("transfer-progress", "1024:2048:1000000")
            .expect("emit");
        emitter
            .emit_event_with_payload(
                "transfer-completed",
                r#"{"durationMs":2000,"bytes":2048,"totalBytes":2048}"#,
            )
            .expect("emit");

        let records = store.list().expect("list");
        assert_eq!(records.len(), 1);
        let row = &records[0];
        assert_eq!(row.status, TransferStatus::Completed);
        assert_eq!(row.payload_bytes, 2_048);
        assert_eq!(row.duration_ms, Some(2_000));
        assert_eq!(row.avg_speed_bps, Some(1_024.0));
        assert_eq!(
            row.peer.as_ref().map(|p| p.display_name.as_deref()),
            Some(Some("Tony's Laptop"))
        );
    }

    #[test]
    fn a_failed_send_records_the_failure_not_a_success() {
        let dir = TempDir::new().expect("tempdir");
        let (store, emitter) = recorder(&dir, TransferDirection::Send, send_context());

        emitter.emit_event("transfer-started").expect("emit");
        emitter.emit_event("transfer-failed").expect("emit");

        let records = store.list().expect("list");
        assert_eq!(records[0].status, TransferStatus::Failed);
    }

    #[test]
    fn stopping_a_finished_share_does_not_rewrite_it_as_cancelled() {
        let dir = TempDir::new().expect("tempdir");
        let (store, emitter) = recorder(&dir, TransferDirection::Send, send_context());

        emitter.emit_event("transfer-started").expect("emit");
        emitter
            .emit_event_with_payload("transfer-completed", r#"{"durationMs":10,"bytes":2048}"#)
            .expect("emit");
        // `stop_sharing` always fires after a share ends.
        emitter.finalize(TransferStatus::Cancelled, CompletionFacts::default(), None);

        assert_eq!(
            store.list().expect("list")[0].status,
            TransferStatus::Completed
        );
    }

    #[test]
    fn a_broadcast_share_counts_devices_instead_of_naming_one() {
        let dir = TempDir::new().expect("tempdir");
        let mut context = send_context();
        context.peer = Some(TransferPeer {
            endpoint_id: "first".to_string(),
            display_name: Some("First Device".to_string()),
            device_type: None,
        });
        let (store, emitter) = recorder(&dir, TransferDirection::Send, context);

        emitter.emit_event("transfer-started").expect("emit");
        for _ in 0..3 {
            emitter
                .emit_event_with_payload("share-peer-connected", r#"{"endpoint_id":"x"}"#)
                .expect("emit");
        }
        emitter.finalize(TransferStatus::Cancelled, CompletionFacts::default(), None);

        let row = &store.list().expect("list")[0];
        assert_eq!(row.peer_count, 3);
        assert!(
            row.peer.is_none(),
            "one peer's identity must not stand in for three"
        );
    }

    fn receive_context(partial: &str) -> TransferContext {
        TransferContext {
            blob_hash: Some("hash".to_string()),
            save_path: Some("/home/tony/Downloads".to_string()),
            resumable_store_path: Some(partial.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn a_completed_receive_learns_its_shape_and_drops_the_partial_pointer() {
        let dir = TempDir::new().expect("tempdir");
        let (store, emitter) = recorder(
            &dir,
            TransferDirection::Receive,
            receive_context("/tmp/.dashbeam-recv-hash"),
        );

        emitter.emit_event("receive-started").expect("emit");
        emitter
            .emit_event_with_payload("receive-file-names", r#"["Photos/a.jpg","Photos/b.jpg"]"#)
            .expect("emit");
        emitter
            .emit_event_with_payload(
                "receive-completed",
                r#"{"durationMs":4000,"exportMs":250,"bytes":8192}"#,
            )
            .expect("emit");

        let row = &store.list().expect("list")[0];
        assert_eq!(row.status, TransferStatus::Completed);
        assert_eq!(row.root_name, "Photos");
        assert_eq!(row.item_count, 1);
        assert_eq!(row.path_type, Some(TransferPathType::Directory));
        assert_eq!(row.payload_bytes, 8_192);
        assert_eq!(row.export_ms, Some(250));
        assert_eq!(row.save_path.as_deref(), Some("/home/tony/Downloads"));
        assert!(
            row.resumable_store_path.is_none(),
            "success deletes the partial store, so the pointer must go too"
        );
    }

    #[test]
    fn a_receive_killed_mid_transfer_stays_reclaimable() {
        let dir = TempDir::new().expect("tempdir");
        let (store, emitter) = recorder(
            &dir,
            TransferDirection::Receive,
            receive_context("/tmp/.dashbeam-recv-hash"),
        );

        emitter.emit_event("receive-started").expect("emit");
        emitter
            .emit_event_with_payload("receive-progress", "4096:8192:500000")
            .expect("emit");
        // The process dies here: no terminal event ever arrives.
        drop(emitter);

        let row = &store.list().expect("list")[0];
        assert_eq!(row.status, TransferStatus::InProgress);
        assert_eq!(
            row.resumable_store_path.as_deref(),
            Some("/tmp/.dashbeam-recv-hash"),
            "the pointer written at open is what makes the partial reclaimable"
        );

        // Next launch reconciles it.
        assert_eq!(store.mark_interrupted().expect("sweep"), 1);
        let row = &store.list().expect("list")[0];
        assert_eq!(row.status, TransferStatus::Interrupted);
        assert!(row.resumable_store_path.is_some());
    }

    #[test]
    fn a_cancelled_receive_reports_how_far_it_got() {
        let dir = TempDir::new().expect("tempdir");
        let (store, emitter) = recorder(
            &dir,
            TransferDirection::Receive,
            receive_context("/tmp/.dashbeam-recv-hash"),
        );

        emitter.emit_event("receive-started").expect("emit");
        emitter
            .emit_event_with_payload("receive-progress", "4096:8192:500000")
            .expect("emit");
        emitter.finalize(TransferStatus::Cancelled, CompletionFacts::default(), None);

        let row = &store.list().expect("list")[0];
        assert_eq!(row.status, TransferStatus::Cancelled);
        assert_eq!(row.bytes_transferred, 4_096);
        assert!(row.resumable_store_path.is_some());
    }

    #[test]
    fn recording_never_swallows_an_event_the_ui_needs() {
        let dir = TempDir::new().expect("tempdir");
        let (_store, emitter) = recorder(&dir, TransferDirection::Send, send_context());

        // Every engine event must still reach the wrapped emitter.
        assert!(emitter.emit_event("transfer-started").is_ok());
        assert!(emitter
            .emit_event_with_payload("transfer-progress", "1:2:3")
            .is_ok());
        assert!(emitter.emit_event("some-unrelated-event").is_ok());
    }

    #[test]
    fn reads_the_persisted_history_toggle() {
        let raw = r#"{"app_settings":"{\"state\":{\"enableTransferHistory\":false}}"}"#;
        assert_eq!(parse_persisted_history_enabled(raw), Some(false));
    }

    #[test]
    fn a_missing_history_toggle_is_absent_rather_than_false() {
        let raw = r#"{"app_settings":"{\"state\":{\"minimizeToTray\":true}}"}"#;
        assert_eq!(parse_persisted_history_enabled(raw), None);
    }

    #[test]
    fn malformed_settings_do_not_panic() {
        assert_eq!(parse_persisted_history_enabled("not json"), None);
        assert_eq!(parse_persisted_history_enabled("{}"), None);
    }

    #[test]
    fn history_recording_defaults_to_on_when_unset() {
        assert!(history_enabled_from_raw(None));
        assert!(history_enabled_from_raw(Some(r#"{}"#)));
    }

    #[test]
    fn history_recording_respects_an_explicit_opt_out() {
        let raw = r#"{"app_settings":"{\"state\":{\"enableTransferHistory\":false}}"}"#;
        assert!(!history_enabled_from_raw(Some(raw)));
    }

    #[test]
    fn completion_facts_come_from_the_engine_payload() {
        let facts = CompletionFacts::parse(
            r#"{"durationMs":2500,"exportMs":300,"bytes":1048576,"totalBytes":1048576}"#,
        );
        assert_eq!(facts.duration_ms, Some(2500));
        assert_eq!(facts.export_ms, Some(300));
        assert_eq!(facts.bytes, Some(1_048_576));
    }

    #[test]
    fn a_completion_payload_without_export_time_is_still_usable() {
        let facts = CompletionFacts::parse(r#"{"durationMs":1000,"bytes":50}"#);
        assert_eq!(facts.duration_ms, Some(1000));
        assert_eq!(facts.export_ms, None);
        assert_eq!(facts.bytes, Some(50));
    }

    #[test]
    fn a_malformed_completion_payload_yields_no_facts() {
        let facts = CompletionFacts::parse("nonsense");
        assert_eq!(facts.duration_ms, None);
        assert_eq!(facts.bytes, None);
    }

    #[test]
    fn average_speed_is_payload_over_wire_time() {
        assert_eq!(average_speed_bps(1_000_000, Some(2_000)), Some(500_000.0));
    }

    #[test]
    fn average_speed_is_absent_without_a_measurable_duration() {
        assert_eq!(average_speed_bps(1_000_000, Some(0)), None);
        assert_eq!(average_speed_bps(1_000_000, None), None);
    }

    #[test]
    fn a_single_flat_file_names_itself() {
        let shape = ReceivedShape::from_file_names(&["report.pdf".to_string()]);
        assert_eq!(shape.root_name, "report.pdf");
        assert_eq!(shape.item_count, 1);
        assert_eq!(shape.path_type, Some(TransferPathType::File));
    }

    #[test]
    fn files_under_one_folder_name_the_folder() {
        let shape = ReceivedShape::from_file_names(&[
            "Photos/a.jpg".to_string(),
            "Photos/b.jpg".to_string(),
            "Photos/nested/c.jpg".to_string(),
        ]);
        assert_eq!(shape.root_name, "Photos");
        assert_eq!(shape.item_count, 1);
        assert_eq!(shape.path_type, Some(TransferPathType::Directory));
    }

    #[test]
    fn several_top_level_items_are_counted_not_named() {
        let shape = ReceivedShape::from_file_names(&[
            "a.txt".to_string(),
            "b.txt".to_string(),
            "Docs/c.txt".to_string(),
        ]);
        assert_eq!(shape.item_count, 3);
        assert_eq!(shape.path_type, None);
    }

    #[test]
    fn an_empty_file_list_yields_nothing_to_name() {
        let shape = ReceivedShape::from_file_names(&[]);
        assert_eq!(shape.root_name, "");
        assert_eq!(shape.item_count, 0);
    }

    #[test]
    fn an_exported_receive_records_where_the_files_actually_landed() {
        let dir = TempDir::new().expect("tempdir");
        let (store, emitter) = recorder(
            &dir,
            TransferDirection::Receive,
            receive_context("/tmp/.dashbeam-recv-hash"),
        );

        emitter.emit_event("receive-started").expect("emit");
        emitter
            .emit_event_with_payload("receive-completed", r#"{"bytes":8192}"#)
            .expect("emit");
        // Android exports out of staging after the row is closed.
        emitter.note_destination(
            "Download/Work".to_string(),
            Some("content://tree/work".to_string()),
        );

        let row = &store.list().expect("list")[0];
        assert_eq!(row.save_path.as_deref(), Some("Download/Work"));
        assert_eq!(
            row.save_uri.as_deref(),
            Some("content://tree/work"),
            "Open needs the tree the files went to, not the one settings holds now"
        );
    }

    #[test]
    fn a_destination_is_only_recorded_for_a_row_that_exists() {
        let dir = TempDir::new().expect("tempdir");
        let (store, emitter) = recorder(
            &dir,
            TransferDirection::Receive,
            receive_context("/tmp/.dashbeam-recv-hash"),
        );

        // No `receive-started`, so nothing was ever opened.
        emitter.note_destination("Download/DashBeam".to_string(), None);

        assert!(store.list().expect("list").is_empty());
    }
    #[test]
    fn a_finished_transfer_tells_an_open_history_list_to_re_read() {
        let dir = TempDir::new().expect("tempdir");
        let (events, emitter) = watched_recorder(&dir, TransferDirection::Send, send_context());

        emitter.emit_event("transfer-started").expect("emit");
        emitter
            .emit_event_with_payload("transfer-completed", r#"{"durationMs":10,"bytes":2048}"#)
            .expect("emit");

        assert_eq!(events.times_seen(HISTORY_UPDATED_EVENT), 1);
    }

    #[test]
    fn a_failed_transfer_tells_an_open_history_list_to_re_read() {
        let dir = TempDir::new().expect("tempdir");
        let (events, emitter) = watched_recorder(&dir, TransferDirection::Send, send_context());

        emitter.emit_event("transfer-started").expect("emit");
        emitter.emit_event("transfer-failed").expect("emit");

        assert_eq!(
            events.times_seen(HISTORY_UPDATED_EVENT),
            1,
            "a failure fires no other event the list could listen for"
        );
    }

    #[test]
    fn a_cancelled_receive_tells_an_open_history_list_to_re_read() {
        let dir = TempDir::new().expect("tempdir");
        let (events, emitter) = watched_recorder(
            &dir,
            TransferDirection::Receive,
            receive_context("/tmp/.dashbeam-recv-hash"),
        );

        emitter.emit_event("receive-started").expect("emit");
        // The receive command finalizes cancels itself; no engine event fires.
        emitter.finalize(TransferStatus::Cancelled, CompletionFacts::default(), None);

        assert_eq!(events.times_seen(HISTORY_UPDATED_EVENT), 1);
    }

    #[test]
    fn a_transfer_still_running_leaves_the_history_list_alone() {
        let dir = TempDir::new().expect("tempdir");
        let (events, emitter) = watched_recorder(&dir, TransferDirection::Send, send_context());

        emitter.emit_event("transfer-started").expect("emit");
        for bytes in ["512:2048:1000", "1024:2048:1000", "1536:2048:1000"] {
            emitter
                .emit_event_with_payload("transfer-progress", bytes)
                .expect("emit");
        }

        assert_eq!(
            events.times_seen(HISTORY_UPDATED_EVENT),
            0,
            "rows only reach the list once they are terminal"
        );
    }

    #[test]
    fn a_second_finalize_does_not_tell_the_list_twice() {
        let dir = TempDir::new().expect("tempdir");
        let (events, emitter) = watched_recorder(&dir, TransferDirection::Send, send_context());

        emitter.emit_event("transfer-started").expect("emit");
        emitter
            .emit_event_with_payload("transfer-completed", r#"{"durationMs":10,"bytes":2048}"#)
            .expect("emit");
        // `stop_sharing` always fires after a share ends.
        emitter.finalize(TransferStatus::Cancelled, CompletionFacts::default(), None);

        assert_eq!(events.times_seen(HISTORY_UPDATED_EVENT), 1);
    }

    #[test]
    fn correcting_the_destination_tells_an_open_history_list_to_re_read() {
        let dir = TempDir::new().expect("tempdir");
        let (events, emitter) = watched_recorder(
            &dir,
            TransferDirection::Receive,
            receive_context("/tmp/.dashbeam-recv-hash"),
        );

        emitter.emit_event("receive-started").expect("emit");
        emitter
            .emit_event_with_payload("receive-completed", r#"{"durationMs":10,"bytes":2048}"#)
            .expect("emit");
        emitter.note_destination("Download/Work".to_string(), None);

        assert_eq!(
            events.times_seen(HISTORY_UPDATED_EVENT),
            2,
            "Android moves the files after the row closes, changing it again"
        );
    }

    #[test]
    fn a_destination_for_a_row_that_never_opened_tells_the_list_nothing() {
        let dir = TempDir::new().expect("tempdir");
        let (events, emitter) = watched_recorder(
            &dir,
            TransferDirection::Receive,
            receive_context("/tmp/.dashbeam-recv-hash"),
        );

        emitter.note_destination("Download/DashBeam".to_string(), None);

        assert_eq!(events.times_seen(HISTORY_UPDATED_EVENT), 0);
    }
}
