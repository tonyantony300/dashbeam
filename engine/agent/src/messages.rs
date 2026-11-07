//! Text messages exchanged with paired devices, kept in memory like transfers.
//!
//! Inbound messages carry a `read` flag: [`Messages::wait_unread`] hands each
//! one out once, whichever client (MCP session or CLI) asks first.

use crate::registry::{Direction, Peer};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub seq: u64,
    pub direction: Direction,
    /// The paired device it came from or went to.
    pub peer: Peer,
    /// Name the sender claimed in the message (self-reported).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_name: Option<String>,
    pub text: String,
    pub at_ms: u64,
    /// Inbound only: handed out by a wait already. Outbound is always true.
    pub read: bool,
}

struct State {
    messages: VecDeque<Message>,
    next_seq: u64,
}

struct Inner {
    cap: usize,
    state: Mutex<State>,
    seq_tx: watch::Sender<u64>,
}

/// Cloneable handle to the shared message log.
#[derive(Clone)]
pub struct Messages {
    inner: Arc<Inner>,
}

impl Messages {
    pub fn new(cap: usize) -> Self {
        let (seq_tx, _) = watch::channel(0);
        Self {
            inner: Arc::new(Inner {
                cap,
                state: Mutex::new(State {
                    messages: VecDeque::new(),
                    next_seq: 1,
                }),
                seq_tx,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Appends a message, assigning `id`, `seq`, `at_ms` and `read`. Evicts
    /// the oldest read message past the cap, then the oldest of any kind.
    pub fn push(
        &self,
        direction: Direction,
        peer: Peer,
        sender_name: Option<String>,
        text: String,
    ) -> Message {
        let mut st = self.lock();
        let seq = st.next_seq;
        st.next_seq += 1;
        let msg = Message {
            id: uuid::Uuid::new_v4().to_string(),
            seq,
            direction,
            peer,
            sender_name,
            text,
            at_ms: engine::unix_now_ms(),
            read: direction == Direction::Outbound,
        };
        st.messages.push_back(msg.clone());
        while st.messages.len() > self.inner.cap {
            let i = st.messages.iter().position(|m| m.read).unwrap_or(0);
            st.messages.remove(i);
        }
        drop(st);
        self.inner.seq_tx.send_replace(seq);
        msg
    }

    /// Messages with `seq > after_seq`, oldest first, without marking them read.
    pub fn list(&self, after_seq: Option<u64>) -> Vec<Message> {
        let after = after_seq.unwrap_or(0);
        self.lock()
            .messages
            .iter()
            .filter(|m| m.seq > after)
            .cloned()
            .collect()
    }

    pub fn unread_count(&self) -> usize {
        self.lock().messages.iter().filter(|m| !m.read).count()
    }

    fn take_unread(&self) -> Vec<Message> {
        let mut st = self.lock();
        let mut out = Vec::new();
        for m in st.messages.iter_mut().filter(|m| !m.read) {
            m.read = true;
            out.push(m.clone());
        }
        out
    }

    /// Waits up to `timeout` for unread inbound messages and returns them all,
    /// marked read. Empty when none arrived in time.
    pub async fn wait_unread(&self, timeout: Duration) -> Vec<Message> {
        let mut rx = self.inner.seq_tx.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let got = self.take_unread();
            if !got.is_empty() {
                return got;
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => continue,
                _ => return Vec::new(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: &str) -> Peer {
        Peer {
            name: Some(format!("dev-{id}")),
            endpoint_id: Some(id.to_string()),
        }
    }

    #[test]
    fn outbound_is_read_inbound_is_not() {
        let m = Messages::new(10);
        let out = m.push(Direction::Outbound, peer("a"), None, "hi".into());
        let inb = m.push(Direction::Inbound, peer("a"), Some("A".into()), "yo".into());
        assert!(out.read);
        assert!(!inb.read);
        assert_eq!((out.seq, inb.seq), (1, 2));
        assert_eq!(m.unread_count(), 1);
    }

    #[tokio::test]
    async fn wait_returns_each_unread_once() {
        let m = Messages::new(10);
        m.push(Direction::Inbound, peer("a"), None, "one".into());
        m.push(Direction::Inbound, peer("a"), None, "two".into());
        let got = m.wait_unread(Duration::from_millis(10)).await;
        assert_eq!(
            got.iter().map(|g| g.text.as_str()).collect::<Vec<_>>(),
            ["one", "two"]
        );
        assert!(got.iter().all(|g| g.read));
        assert!(m.wait_unread(Duration::from_millis(10)).await.is_empty());
        assert_eq!(m.list(None).len(), 2, "history keeps read messages");
    }

    #[tokio::test]
    async fn wait_wakes_on_new_message() {
        let m = Messages::new(10);
        let m2 = m.clone();
        let waiter = tokio::spawn(async move { m2.wait_unread(Duration::from_secs(5)).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        m.push(Direction::Outbound, peer("a"), None, "mine".into());
        m.push(Direction::Inbound, peer("a"), None, "theirs".into());
        let got = waiter.await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "theirs");
    }

    #[test]
    fn cap_evicts_read_before_unread() {
        let m = Messages::new(2);
        m.push(Direction::Inbound, peer("a"), None, "unread-1".into());
        m.push(Direction::Outbound, peer("a"), None, "sent".into());
        m.push(Direction::Inbound, peer("a"), None, "unread-2".into());
        let texts: Vec<_> = m.list(None).into_iter().map(|x| x.text).collect();
        assert_eq!(texts, ["unread-1", "unread-2"]);
    }

    #[test]
    fn list_filters_by_seq() {
        let m = Messages::new(10);
        m.push(Direction::Inbound, peer("a"), None, "1".into());
        m.push(Direction::Inbound, peer("a"), None, "2".into());
        assert_eq!(m.list(Some(1)).len(), 1);
    }
}
