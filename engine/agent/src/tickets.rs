//! Ticket shares: one-time, expiring blob tickets for agents outside the
//! paired network, and receiving such tickets.

use crate::metadata::prepare_paths;
use crate::outbound::{stop_share, SendProgress};
use crate::registry::{Direction, TransferKind, TransferRecord, TransferStatus};
use crate::service::AgentService;
use anyhow::{bail, Context, Result};
use engine::SendResult;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::Instant;

/// How often a waiting share checks `completed_peers()` and progress.
const POLL: Duration = Duration::from_millis(500);
/// Extra time past expiry for a download that is still moving bytes.
const STALL_GRACE: Duration = Duration::from_secs(30);
/// Hard stop past expiry, however long a download keeps moving.
const HARD_STOP_AFTER_EXPIRY: Duration = Duration::from_secs(600);

/// Next deadline after `now` passed `deadline` with a download that moved
/// bytes: one grace window, never past `hard_stop`. `None` means stop now.
fn extend_deadline(now: Instant, hard_stop: Instant, progressed: bool) -> Option<Instant> {
    if !progressed || now >= hard_stop {
        return None;
    }
    Some((now + STALL_GRACE).min(hard_stop))
}

impl AgentService {
    /// Shares `paths` behind a one-time ticket and returns at once. The share
    /// stops on the engine's `transfer-completed` for the first finished
    /// download (`Delivered`), with a `completed_peers()` poll as backstop, or
    /// at expiry (`Expired`). One-time is best effort: a second peer already
    /// mid-transfer when the first completes may still finish; strict gating
    /// would need an engine change. `ttl_secs` defaults to `config.ticket_ttl_secs`.
    pub async fn share_ticket(
        self: &Arc<Self>,
        paths: Vec<PathBuf>,
        cwd: PathBuf,
        ttl_secs: Option<u64>,
    ) -> Result<TransferRecord> {
        let ttl = Duration::from_secs(ttl_secs.unwrap_or(self.config().ticket_ttl_secs));
        let prepared = tokio::task::spawn_blocking(move || prepare_paths(&paths, &cwd))
            .await
            .context("prepare paths")??;
        if *self.stop_signal().borrow() {
            bail!("agent is shutting down");
        }
        let options = self.send_options()?;

        let rec = self.registry().insert(TransferRecord {
            id: String::new(),
            seq: 0,
            direction: Direction::Outbound,
            kind: TransferKind::Ticket,
            peer: None,
            file_names: prepared.file_names.clone(),
            total_bytes: prepared.total_size,
            bytes_done: 0,
            status: TransferStatus::Starting,
            created_at_ms: 0,
            updated_at_ms: 0,
            error: None,
            saved_paths: Vec::new(),
            ticket: None,
            expires_at_ms: None,
        });
        let progress = Arc::new(SendProgress::new(self.registry().clone(), rec.id.clone()));
        let app: engine::AppHandle = Some(progress.clone());
        let share = match engine::start_share_items(
            prepared.paths.clone(),
            options,
            &app,
            Some(prepared.metadata.clone()),
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                let msg = format!("could not start the share: {e:#}");
                self.registry().update(&rec.id, |r| {
                    r.status = TransferStatus::Failed;
                    r.error = Some(msg.clone());
                });
                bail!(msg);
            }
        };

        self.cancels.register(&rec.id);
        let cancel = self
            .cancels
            .start(&rec.id)
            .expect("slot was just registered");
        let expires = engine::unix_now_ms() + ttl.as_millis() as u64;
        let ticket = share.ticket.to_string();
        let live = self
            .registry()
            .update(&rec.id, |r| {
                r.status = TransferStatus::WaitingForDownload;
                r.ticket = Some(ticket);
                r.expires_at_ms = Some(expires);
            })
            .expect("record was just inserted");

        let svc = self.clone();
        let id = rec.id;
        tokio::spawn(async move {
            svc.run_ticket_share(id, share, progress, ttl, cancel).await;
        });
        Ok(live)
    }

    /// Waits for the first completed download, expiry or cancel; stops the
    /// share, then makes the single terminal update.
    async fn run_ticket_share(
        &self,
        id: String,
        share: SendResult,
        progress: Arc<SendProgress>,
        ttl: Duration,
        mut cancel: oneshot::Receiver<()>,
    ) {
        let mut stop = self.stop_signal();
        let mut deadline = Instant::now() + ttl;
        let hard_stop = deadline + HARD_STOP_AFTER_EXPIRY;
        let mut seen_bytes = 0;
        let mut tick = tokio::time::interval(POLL);
        let (status, error) = loop {
            // A finished download always wins over expiry or cancel.
            let delivered = || share.completed_peers() >= 1;
            tokio::select! {
                biased;
                _ = &mut cancel => {
                    break if delivered() {
                        (TransferStatus::Delivered, None)
                    } else {
                        (TransferStatus::Cancelled, None)
                    };
                }
                _ = stop.wait_for(|s| *s) => {
                    break if delivered() {
                        (TransferStatus::Delivered, None)
                    } else {
                        (TransferStatus::Cancelled, Some("agent shut down".to_string()))
                    };
                }
                _ = progress.completed.notified() => {
                    break (TransferStatus::Delivered, None);
                }
                _ = tick.tick() => {
                    if delivered() {
                        break (TransferStatus::Delivered, None);
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    if delivered() {
                        break (TransferStatus::Delivered, None);
                    }
                    // Don't cut off a download that is still moving.
                    let bytes = progress.bytes();
                    let progressed = bytes > seen_bytes;
                    seen_bytes = bytes;
                    if let Some(next) = extend_deadline(Instant::now(), hard_stop, progressed) {
                        deadline = next;
                        continue;
                    }
                    break (
                        TransferStatus::Expired,
                        Some(format!("ticket not downloaded within {} s", ttl.as_secs())),
                    );
                }
            }
        };
        self.cancels.finish(&id);
        stop_share(share).await;
        self.registry().update(&id, |r| {
            r.status = status;
            r.error = error;
            if status == TransferStatus::Delivered {
                r.bytes_done = r.total_bytes;
            }
        });
    }

    /// Starts downloading `ticket` into `receive_dir/tickets/<transfer id>/`
    /// and returns the `Transferring` record at once; wait on the record for
    /// the outcome. With `max_ticket_receive_bytes` set, an oversized payload
    /// is rejected (as an error) before anything is downloaded.
    pub async fn receive_ticket(self: &Arc<Self>, ticket: &str) -> Result<TransferRecord> {
        if let Some(max) = self.config().max_ticket_receive_bytes {
            let options = self.receive_options(None)?;
            let meta = engine::fetch_metadata(ticket.to_string(), options)
                .await
                .context("could not read the ticket's payload size")?;
            if meta.size > max {
                bail!(
                    "ticket payload of {} bytes exceeds max_ticket_receive_bytes ({max})",
                    meta.size
                );
            }
        }
        let rec = self.registry().insert(TransferRecord {
            id: String::new(),
            seq: 0,
            direction: Direction::Inbound,
            kind: TransferKind::Ticket,
            peer: None,
            file_names: Vec::new(),
            total_bytes: 0,
            bytes_done: 0,
            status: TransferStatus::Transferring,
            created_at_ms: 0,
            updated_at_ms: 0,
            error: None,
            saved_paths: Vec::new(),
            ticket: None,
            expires_at_ms: None,
        });
        self.cancels.register(&rec.id);
        let out_dir = self
            .config()
            .receive_dir(self.data_dir())
            .join("tickets")
            .join(&rec.id);
        let svc = self.clone();
        let (id, ticket) = (rec.id.clone(), ticket.to_string());
        tokio::spawn(async move { svc.receive_into(id, ticket, out_dir).await });
        Ok(rec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn grace_is_bounded_by_hard_stop() {
        let t0 = Instant::now();
        let hard = t0 + HARD_STOP_AFTER_EXPIRY;
        assert_eq!(extend_deadline(t0, hard, false), None);
        assert_eq!(extend_deadline(t0, hard, true), Some(t0 + STALL_GRACE));
        let near = hard - Duration::from_secs(5);
        assert_eq!(extend_deadline(near, hard, true), Some(hard));
        assert_eq!(extend_deadline(hard, hard, true), None);
    }
}
