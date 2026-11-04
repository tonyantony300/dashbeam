//! Per-transfer cancel slots for `cancel_transfer`, shared by outbound sends
//! and inbound downloads. A slot exists from registration until the transfer
//! finishes; `cancel` removing it is what marks the transfer as cancelled.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use tokio::sync::oneshot;

enum Slot {
    /// Registered, not started: whoever cancels it writes the terminal record.
    Pending,
    /// Running: the task owning the receiver writes the terminal record.
    Running(oneshot::Sender<()>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cancelled {
    /// Never started; the caller must finalize the record.
    Pending,
    /// Signalled; the running task finalizes the record.
    Running,
    /// No slot: unknown, already finishing, or not cancellable.
    NotTracked,
}

#[derive(Default)]
pub(crate) struct Cancels {
    slots: Mutex<HashMap<String, Slot>>,
}

impl Cancels {
    fn slots(&self) -> MutexGuard<'_, HashMap<String, Slot>> {
        self.slots.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn register(&self, id: &str) {
        self.slots().insert(id.to_string(), Slot::Pending);
    }

    /// Pending → Running. `None` if it was cancelled before it started. The
    /// receiver fires (or errors, if the slot is dropped) on cancel only.
    pub(crate) fn start(&self, id: &str) -> Option<oneshot::Receiver<()>> {
        let mut slots = self.slots();
        match slots.get(id) {
            Some(Slot::Pending) => {
                let (tx, rx) = oneshot::channel();
                slots.insert(id.to_string(), Slot::Running(tx));
                Some(rx)
            }
            _ => None,
        }
    }

    /// Drops the slot; `false` means `cancel` got there first.
    pub(crate) fn finish(&self, id: &str) -> bool {
        self.slots().remove(id).is_some()
    }

    pub(crate) fn cancel(&self, id: &str) -> Cancelled {
        match self.slots().remove(id) {
            Some(Slot::Pending) => Cancelled::Pending,
            Some(Slot::Running(tx)) => {
                let _ = tx.send(());
                Cancelled::Running
            }
            None => Cancelled::NotTracked,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_cancel_prevents_start() {
        let c = Cancels::default();
        c.register("a");
        assert_eq!(c.cancel("a"), Cancelled::Pending);
        assert!(c.start("a").is_none());
        assert!(!c.finish("a"));
        assert_eq!(c.cancel("a"), Cancelled::NotTracked);
    }

    #[test]
    fn running_cancel_signals_receiver() {
        let c = Cancels::default();
        c.register("a");
        let mut rx = c.start("a").unwrap();
        assert!(rx.try_recv().is_err());
        assert_eq!(c.cancel("a"), Cancelled::Running);
        assert!(rx.try_recv().is_ok());
        assert!(!c.finish("a"));
    }

    #[test]
    fn finish_without_cancel() {
        let c = Cancels::default();
        c.register("a");
        let _rx = c.start("a").unwrap();
        assert!(c.start("a").is_none());
        assert!(c.finish("a"));
        assert_eq!(c.cancel("a"), Cancelled::NotTracked);
        assert!(c.start("unknown").is_none());
    }
}
