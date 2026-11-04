//! In-memory record of every transfer the agent node makes.

use serde::Serialize;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferStatus {
    Queued,
    Starting,
    WaitingForAccept,
    WaitingForDownload,
    Transferring,
    Delivered,
    Received,
    Declined,
    Expired,
    Unreachable,
    Failed,
    Cancelled,
}

impl TransferStatus {
    pub fn is_terminal(&self) -> bool {
        !matches!(
            self,
            Self::Queued
                | Self::Starting
                | Self::WaitingForAccept
                | Self::WaitingForDownload
                | Self::Transferring
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Outbound,
    Inbound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferKind {
    Paired,
    Ticket,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Peer {
    pub name: Option<String>,
    pub endpoint_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TransferRecord {
    pub id: String,
    pub seq: u64,
    pub direction: Direction,
    pub kind: TransferKind,
    pub peer: Option<Peer>,
    pub file_names: Vec<String>,
    pub total_bytes: u64,
    pub bytes_done: u64,
    pub status: TransferStatus,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub error: Option<String>,
    pub saved_paths: Vec<PathBuf>,
    pub ticket: Option<String>,
    pub expires_at_ms: Option<u64>,
}

struct State {
    // Insertion order, oldest first.
    records: VecDeque<TransferRecord>,
    next_seq: u64,
}

struct Inner {
    cap: usize,
    state: Mutex<State>,
    seq_tx: watch::Sender<u64>,
}

/// Cloneable handle to the shared registry.
#[derive(Clone)]
pub struct Registry {
    inner: Arc<Inner>,
}

impl Registry {
    pub fn new(cap: usize) -> Self {
        let (seq_tx, _) = watch::channel(0);
        Self {
            inner: Arc::new(Inner {
                cap,
                state: Mutex::new(State {
                    records: VecDeque::new(),
                    next_seq: 1,
                }),
                seq_tx,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn bump(&self, st: &mut State) -> u64 {
        let seq = st.next_seq;
        st.next_seq += 1;
        self.inner.seq_tx.send_replace(seq);
        seq
    }

    pub fn insert(&self, mut record: TransferRecord) -> TransferRecord {
        let mut st = self.lock();
        let now = engine::unix_now_ms();
        record.id = uuid::Uuid::new_v4().to_string();
        record.created_at_ms = now;
        record.updated_at_ms = now;
        record.seq = self.bump(&mut st);
        st.records.push_back(record.clone());
        // Evict oldest terminal records; live ones are never dropped.
        while st.records.len() > self.inner.cap {
            match st.records.iter().position(|r| r.status.is_terminal()) {
                Some(i) => {
                    st.records.remove(i);
                }
                None => break,
            }
        }
        record
    }

    pub fn update(&self, id: &str, f: impl FnOnce(&mut TransferRecord)) -> Option<TransferRecord> {
        let mut st = self.lock();
        let i = st.records.iter().position(|r| r.id == id)?;
        let seq = self.bump(&mut st);
        let r = &mut st.records[i];
        f(r);
        r.seq = seq;
        r.updated_at_ms = engine::unix_now_ms();
        Some(r.clone())
    }

    /// Like [`update`](Self::update), but a no-op (no seq bump) once the
    /// record is terminal: for progress and status steps that may race the
    /// single terminal update.
    pub fn update_live(
        &self,
        id: &str,
        f: impl FnOnce(&mut TransferRecord),
    ) -> Option<TransferRecord> {
        let mut st = self.lock();
        let i = st.records.iter().position(|r| r.id == id)?;
        if st.records[i].status.is_terminal() {
            return None;
        }
        let seq = self.bump(&mut st);
        let r = &mut st.records[i];
        f(r);
        r.seq = seq;
        r.updated_at_ms = engine::unix_now_ms();
        Some(r.clone())
    }

    pub fn get(&self, id: &str) -> Option<TransferRecord> {
        self.lock().records.iter().find(|r| r.id == id).cloned()
    }

    pub fn list(
        &self,
        after_seq: Option<u64>,
        direction: Option<Direction>,
    ) -> Vec<TransferRecord> {
        self.lock()
            .records
            .iter()
            .filter(|r| after_seq.is_none_or(|s| r.seq > s))
            .filter(|r| direction.is_none_or(|d| r.direction == d))
            .cloned()
            .collect()
    }

    /// `None` only for an unknown id; the bool is `still_running` (timed out).
    pub async fn wait_terminal(
        &self,
        id: &str,
        timeout: Duration,
    ) -> Option<(TransferRecord, bool)> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut rx = self.inner.seq_tx.subscribe();
        loop {
            let rec = self.get(id)?;
            if rec.status.is_terminal() {
                return Some((rec, false));
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => {}
                _ => return Some((rec, true)),
            }
        }
    }

    /// Received inbound records with `seq > after_seq` plus the next cursor.
    pub async fn wait_inbound_completed(
        &self,
        after_seq: u64,
        timeout: Duration,
    ) -> (Vec<TransferRecord>, u64) {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut rx = self.inner.seq_tx.subscribe();
        loop {
            let found: Vec<_> = self
                .list(Some(after_seq), Some(Direction::Inbound))
                .into_iter()
                .filter(|r| r.status == TransferStatus::Received)
                .collect();
            if !found.is_empty() {
                let next = found.iter().map(|r| r.seq).max().unwrap_or(after_seq);
                return (found, next);
            }
            if !matches!(
                tokio::time::timeout_at(deadline, rx.changed()).await,
                Ok(Ok(()))
            ) {
                return (Vec::new(), after_seq);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(direction: Direction, status: TransferStatus) -> TransferRecord {
        TransferRecord {
            id: String::new(),
            seq: 0,
            direction,
            kind: TransferKind::Ticket,
            peer: None,
            file_names: vec!["a.txt".into()],
            total_bytes: 10,
            bytes_done: 0,
            status,
            created_at_ms: 0,
            updated_at_ms: 0,
            error: None,
            saved_paths: vec![],
            ticket: None,
            expires_at_ms: None,
        }
    }

    #[test]
    fn insert_assigns_unique_ids_and_increasing_seq() {
        let r = Registry::new(10);
        let a = r.insert(rec(Direction::Outbound, TransferStatus::Queued));
        let b = r.insert(rec(Direction::Outbound, TransferStatus::Queued));
        assert_ne!(a.id, b.id);
        assert!(b.seq > a.seq);
        assert!(a.created_at_ms > 0);
        let up = r.update(&a.id, |t| t.bytes_done = 5).unwrap();
        assert!(up.seq > b.seq);
        assert_eq!(r.get(&a.id).unwrap().bytes_done, 5);
        assert!(r.update("nope", |_| {}).is_none());
    }

    #[test]
    fn update_live_skips_terminal_records() {
        let r = Registry::new(10);
        let a = r.insert(rec(Direction::Outbound, TransferStatus::Transferring));
        let live = r.update_live(&a.id, |t| t.bytes_done = 3).unwrap();
        assert_eq!(live.bytes_done, 3);
        let done = r
            .update(&a.id, |t| t.status = TransferStatus::Delivered)
            .unwrap();
        assert!(r.update_live(&a.id, |t| t.bytes_done = 9).is_none());
        let after = r.get(&a.id).unwrap();
        assert_eq!((after.seq, after.bytes_done), (done.seq, 3));
    }

    #[tokio::test]
    async fn wait_terminal_returns_still_running_on_timeout() {
        let r = Registry::new(10);
        let a = r.insert(rec(Direction::Outbound, TransferStatus::Transferring));
        let (t, running) = r
            .wait_terminal(&a.id, Duration::from_millis(50))
            .await
            .unwrap();
        assert!(running);
        assert_eq!(t.status, TransferStatus::Transferring);
        assert!(r
            .wait_terminal("nope", Duration::from_millis(10))
            .await
            .is_none());
    }

    #[tokio::test]
    async fn wait_terminal_wakes_on_terminal_update() {
        let r = Registry::new(10);
        let a = r.insert(rec(Direction::Outbound, TransferStatus::Transferring));
        let r2 = r.clone();
        let id = a.id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            r2.update(&id, |t| t.status = TransferStatus::Delivered);
        });
        let (t, running) = r
            .wait_terminal(&a.id, Duration::from_secs(5))
            .await
            .unwrap();
        assert!(!running);
        assert_eq!(t.status, TransferStatus::Delivered);
    }

    #[tokio::test]
    async fn wait_inbound_completed_returns_only_received_after_cursor() {
        let r = Registry::new(10);
        r.insert(rec(Direction::Inbound, TransferStatus::Transferring));
        r.insert(rec(Direction::Outbound, TransferStatus::Received));
        let c = r.insert(rec(Direction::Inbound, TransferStatus::Received));
        let (v, next) = r.wait_inbound_completed(0, Duration::from_millis(10)).await;
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].id, c.id);
        assert_eq!(next, c.seq);
        let (v, next2) = r
            .wait_inbound_completed(next, Duration::from_millis(30))
            .await;
        assert!(v.is_empty());
        assert_eq!(next2, next);
    }

    #[tokio::test]
    async fn wait_inbound_completed_wakes_on_new_received() {
        let r = Registry::new(10);
        let a = r.insert(rec(Direction::Inbound, TransferStatus::Transferring));
        let r2 = r.clone();
        let id = a.id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            r2.update(&id, |t| t.status = TransferStatus::Received);
        });
        let (v, _) = r.wait_inbound_completed(0, Duration::from_secs(5)).await;
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn list_filters_by_seq_and_direction() {
        let r = Registry::new(10);
        let a = r.insert(rec(Direction::Outbound, TransferStatus::Queued));
        r.insert(rec(Direction::Inbound, TransferStatus::Queued));
        assert_eq!(r.list(None, None).len(), 2);
        assert_eq!(r.list(Some(a.seq), None).len(), 1);
        assert_eq!(r.list(None, Some(Direction::Outbound)).len(), 1);
    }

    #[test]
    fn cap_evicts_oldest_terminal_first() {
        let r = Registry::new(3);
        let live = r.insert(rec(Direction::Outbound, TransferStatus::Transferring));
        let t1 = r.insert(rec(Direction::Outbound, TransferStatus::Delivered));
        let t2 = r.insert(rec(Direction::Outbound, TransferStatus::Failed));
        let t3 = r.insert(rec(Direction::Outbound, TransferStatus::Delivered));
        assert!(r.get(&live.id).is_some());
        assert!(r.get(&t1.id).is_none());
        assert!(r.get(&t2.id).is_some());
        assert!(r.get(&t3.id).is_some());
        assert_eq!(r.list(None, None).len(), 3);
    }
}
