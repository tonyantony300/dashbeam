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

impl AgentService {
    /// Shares `paths` behind a one-time ticket and returns at once. The share
    /// stops after its first completed download (`Delivered`) or at expiry
    /// (`Expired`). `ttl_secs` defaults to `config.ticket_ttl_secs`.
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
                    if bytes > seen_bytes {
                        seen_bytes = bytes;
                        deadline = Instant::now() + STALL_GRACE;
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

    /// Downloads `ticket` into `receive_dir/tickets/<transfer id>/` and returns
    /// the finished record. With `max_ticket_receive_bytes` set, an oversized
    /// payload is rejected before anything is downloaded.
    pub async fn receive_ticket(&self, ticket: &str) -> Result<TransferRecord> {
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
        self.receive_into(rec.id.clone(), ticket.to_string(), out_dir)
            .await;
        // `cancel_transfer` on a not-yet-started record finalizes it itself.
        self.registry()
            .get(&rec.id)
            .context("transfer record was evicted")
    }
}
