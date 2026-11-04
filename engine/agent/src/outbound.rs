//! Outbound paired sends. Invite responses carry no invite id on today's wire,
//! so each target device gets one worker that runs its sends one at a time
//! (one outstanding invite per device); different devices run concurrently.

use crate::devices::{resolve_device, ResolveError};
use crate::inbound::parse_progress;
use crate::metadata::{prepare_paths, Prepared};
use crate::registry::{Direction, Peer, Registry, TransferKind, TransferRecord, TransferStatus};
use crate::service::AgentService;
use anyhow::{anyhow, bail, Context, Result};
use engine::{AddrInfoOptions, EventEmitter, PairedDeviceInfo, SendOptions, SendResult};
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;

/// How often a waiting send checks `completed_peers()` and progress.
const POLL: Duration = Duration::from_millis(500);

struct Job {
    id: String,
    endpoint_id: String,
    name: String,
    prepared: Prepared,
}

/// One target device's worker.
struct Lane {
    jobs: mpsc::UnboundedSender<Job>,
    /// Set while a send to this device waits on its invite's answer.
    responses: Option<mpsc::UnboundedSender<bool>>,
}

#[derive(Default)]
pub(crate) struct Outbound {
    /// Keyed by lowercase endpoint id.
    lanes: Mutex<HashMap<String, Lane>>,
}

impl Outbound {
    fn lanes(&self) -> MutexGuard<'_, HashMap<String, Lane>> {
        self.lanes.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_responses(&self, key: &str, tx: Option<mpsc::UnboundedSender<bool>>) {
        if let Some(lane) = self.lanes().get_mut(key) {
            lane.responses = tx;
        }
    }
}

/// `paired-invite-response` payload.
#[derive(Deserialize)]
struct InviteResponsePayload {
    endpoint_id: String,
    response: String,
}

/// A send's terminal status and reason.
struct End {
    status: TransferStatus,
    error: Option<String>,
}

impl End {
    fn ok(status: TransferStatus) -> Self {
        Self {
            status,
            error: None,
        }
    }

    fn err(status: TransferStatus, error: impl Into<String>) -> Self {
        Self {
            status,
            error: Some(error.into()),
        }
    }
}

impl AgentService {
    /// Validates the device and paths, records the send (`Queued` behind an
    /// earlier send to the same device, else `Starting`) and returns at once.
    /// Relative paths resolve against `cwd`, the caller's working directory.
    pub async fn send_to_device(
        self: &Arc<Self>,
        device_query: &str,
        paths: Vec<PathBuf>,
        cwd: PathBuf,
    ) -> Result<TransferRecord> {
        let devices = self.list_devices()?;
        let device = resolve_device(&devices, device_query).map_err(|e| unresolved(e, &devices))?;
        if !device.pairing_status.is_active() {
            bail!(
                "{} ({}) is no longer actively paired ({:?}); pair again before sending",
                device.display_name,
                device.endpoint_id,
                device.pairing_status
            );
        }
        let prepared = tokio::task::spawn_blocking(move || prepare_paths(&paths, &cwd))
            .await
            .context("prepare paths")??;
        if *self.stop_signal().borrow() {
            bail!("agent is shutting down");
        }

        let key = device.endpoint_id.to_lowercase();
        let mut lanes = self.outbound.lanes();
        // Live sends to this device finalize only after their share stops, so
        // this is exactly "the device's worker is busy".
        let busy = self
            .registry()
            .list(None, Some(Direction::Outbound))
            .iter()
            .any(|r| {
                r.kind == TransferKind::Paired
                    && !r.status.is_terminal()
                    && r.peer
                        .as_ref()
                        .and_then(|p| p.endpoint_id.as_deref())
                        .is_some_and(|id| id.eq_ignore_ascii_case(&key))
            });
        let rec = self.registry().insert(TransferRecord {
            id: String::new(),
            seq: 0,
            direction: Direction::Outbound,
            kind: TransferKind::Paired,
            peer: Some(Peer {
                name: Some(device.display_name.clone()),
                endpoint_id: Some(device.endpoint_id.clone()),
            }),
            file_names: prepared.file_names.clone(),
            total_bytes: prepared.total_size,
            bytes_done: 0,
            status: if busy {
                TransferStatus::Queued
            } else {
                TransferStatus::Starting
            },
            created_at_ms: 0,
            updated_at_ms: 0,
            error: None,
            saved_paths: Vec::new(),
            ticket: None,
            expires_at_ms: None,
        });
        self.cancels.register(&rec.id);
        let lane = lanes
            .entry(key.clone())
            .or_insert_with(|| self.spawn_lane(key));
        let job = Job {
            id: rec.id.clone(),
            endpoint_id: device.endpoint_id,
            name: device.display_name,
            prepared,
        };
        if lane.jobs.send(job).is_err() {
            drop(lanes);
            self.cancels.finish(&rec.id);
            self.registry().update_live(&rec.id, |r| {
                r.status = TransferStatus::Failed;
                r.error = Some("agent is shutting down".into());
            });
            bail!("agent is shutting down");
        }
        Ok(rec)
    }

    fn spawn_lane(self: &Arc<Self>, key: String) -> Lane {
        let (jobs, rx) = mpsc::unbounded_channel();
        tokio::spawn(run_lane(Arc::downgrade(self), key, rx, self.stop_signal()));
        Lane {
            jobs,
            responses: None,
        }
    }

    /// `paired-invite-response`: handed to the send waiting on that device, if any.
    pub(crate) fn on_invite_response(&self, payload: Option<&str>) {
        let Some(payload) = payload else { return };
        let resp = match serde_json::from_str::<InviteResponsePayload>(payload) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("malformed paired-invite-response payload: {e}");
                return;
            }
        };
        let accepted = match resp.response.as_str() {
            "accepted" => true,
            "declined" => false,
            other => {
                tracing::debug!(response = other, "unknown invite response; ignoring");
                return;
            }
        };
        let lanes = self.outbound.lanes();
        match lanes
            .get(&resp.endpoint_id.to_lowercase())
            .and_then(|l| l.responses.as_ref())
        {
            Some(tx) => {
                let _ = tx.send(accepted);
            }
            // E.g. the receiver's own expiry answering an invite we already expired.
            None => tracing::debug!(
                peer = %resp.endpoint_id,
                accepted,
                "invite response with no send waiting; ignoring"
            ),
        }
    }

    /// Relay/discovery from config; full dial info in the ticket, as the app does.
    pub(crate) fn send_options(&self) -> Result<SendOptions> {
        let relay_mode = engine::build_relay_mode(self.config().relay.clone())
            .map_err(|e| anyhow!("invalid relay config: {e}"))?;
        let discovery_mode = engine::build_discovery_mode(self.config().discovery.clone())
            .map_err(|e| anyhow!("invalid discovery config: {e}"))?;
        Ok(SendOptions {
            relay_mode,
            discovery_mode,
            ticket_type: AddrInfoOptions::RelayAndAddresses,
            magic_ipv4_addr: None,
            magic_ipv6_addr: None,
        })
    }

    /// One send, start to the single terminal update.
    async fn run_send(&self, key: &str, job: Job) {
        // `cancel_transfer` already finalized it while it was queued.
        let Some(mut cancel) = self.cancels.start(&job.id) else {
            return;
        };
        if self.registry().get(&job.id).map(|r| r.status) == Some(TransferStatus::Queued) {
            self.registry()
                .update_live(&job.id, |r| r.status = TransferStatus::Starting);
        }
        let end = self.drive_send(key, &job, &mut cancel).await;
        self.cancels.finish(&job.id);
        if let Some(e) = &end.error {
            tracing::info!(peer = %job.endpoint_id, status = ?end.status, "send ended: {e}");
        }
        self.registry().update_live(&job.id, |r| {
            r.status = end.status;
            r.error = end.error;
            if end.status == TransferStatus::Delivered {
                r.bytes_done = r.total_bytes;
            }
        });
    }

    /// Shares, invites, waits; the share is stopped before this returns.
    async fn drive_send(&self, key: &str, job: &Job, cancel: &mut oneshot::Receiver<()>) -> End {
        let options = match self.send_options() {
            Ok(o) => o,
            Err(e) => return End::err(TransferStatus::Failed, format!("{e:#}")),
        };
        let progress = Arc::new(SendProgress::new(self.registry().clone(), job.id.clone()));
        let app: engine::AppHandle = Some(progress.clone());
        // Not raced with cancel: dropping a half-built share leaks its temp store.
        let share = match engine::start_share_items(
            job.prepared.paths.clone(),
            options,
            &app,
            Some(job.prepared.metadata.clone()),
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                return End::err(
                    TransferStatus::Failed,
                    format!("could not start the share: {e:#}"),
                )
            }
        };

        // Registered before inviting: a trusted receiver can answer first.
        let (tx, mut responses) = mpsc::unbounded_channel();
        self.outbound.set_responses(key, Some(tx));
        let end = self
            .invite_and_wait(job, &share, &progress, &mut responses, cancel)
            .await;
        self.outbound.set_responses(key, None);
        stop_share(share).await;
        end
    }

    async fn invite_and_wait(
        &self,
        job: &Job,
        share: &SendResult,
        progress: &SendProgress,
        responses: &mut mpsc::UnboundedReceiver<bool>,
        cancel: &mut oneshot::Receiver<()>,
    ) -> End {
        let mut stop = self.stop_signal();
        let invite = self.node().invite_paired_device(
            &job.endpoint_id,
            &share.ticket,
            job.prepared.file_count,
            job.prepared.total_size,
        );
        let delivered = tokio::select! {
            biased;
            _ = &mut *cancel => return End::ok(TransferStatus::Cancelled),
            _ = stop.wait_for(|s| *s) => {
                return End::err(TransferStatus::Cancelled, "agent shut down")
            }
            r = invite => r,
        };
        match delivered {
            Ok(true) => {}
            Ok(false) => {
                return End::err(
                    TransferStatus::Unreachable,
                    format!(
                        "invite not delivered: {} is offline or unreachable",
                        job.name
                    ),
                )
            }
            Err(e) => {
                return End::err(
                    TransferStatus::Unreachable,
                    format!("invite not delivered: {e:#}"),
                )
            }
        }

        // Our trust in them is only a hint that they auto-accept us; an answer
        // or moving bytes settle it either way.
        let trusted = self
            .paired_device(&job.endpoint_id)
            .ok()
            .flatten()
            .is_some_and(|d| d.trusted);
        let timeout = Duration::from_secs(self.config().invite_timeout_secs);
        let mut deadline = Instant::now() + timeout;
        let mut status = if trusted {
            TransferStatus::Transferring
        } else {
            TransferStatus::WaitingForAccept
        };
        let expires = engine::unix_now_ms() + timeout.as_millis() as u64;
        self.registry().update_live(&job.id, |r| {
            r.status = status;
            r.expires_at_ms = Some(expires);
        });

        // An accept or any data disarms expiry; after that the same window
        // bounds a stall instead, so a dead receiver can't hold the queue.
        let mut engaged = false;
        let mut seen_bytes = 0;
        let mut tick = tokio::time::interval(POLL);
        loop {
            let mut activity = false;
            tokio::select! {
                biased;
                _ = &mut *cancel => return End::ok(TransferStatus::Cancelled),
                _ = stop.wait_for(|s| *s) => {
                    return End::err(TransferStatus::Cancelled, "agent shut down")
                }
                Some(accepted) = responses.recv() => {
                    if !accepted {
                        return End::ok(TransferStatus::Declined);
                    }
                    activity = true;
                }
                _ = tokio::time::sleep_until(deadline) => {
                    return if engaged {
                        End::err(
                            TransferStatus::Failed,
                            format!("{} accepted but no data moved for {} s", job.name, timeout.as_secs()),
                        )
                    } else {
                        End::err(
                            TransferStatus::Expired,
                            format!("no answer from {} within {} s", job.name, timeout.as_secs()),
                        )
                    };
                }
                _ = tick.tick() => {
                    if share.completed_peers() >= 1 {
                        return End::ok(TransferStatus::Delivered);
                    }
                    let bytes = progress.bytes();
                    if bytes > seen_bytes || (!engaged && progress.started()) {
                        seen_bytes = bytes;
                        activity = true;
                    }
                }
            }
            if activity {
                engaged = true;
                deadline = Instant::now() + timeout;
                if status != TransferStatus::Transferring {
                    status = TransferStatus::Transferring;
                    self.registry().update_live(&job.id, |r| {
                        r.status = TransferStatus::Transferring;
                        r.expires_at_ms = None;
                    });
                }
            }
        }
    }
}

/// Feeds one device's sends to [`AgentService::run_send`] in order.
async fn run_lane(
    svc: Weak<AgentService>,
    key: String,
    mut jobs: mpsc::UnboundedReceiver<Job>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        let job = tokio::select! {
            biased;
            _ = stop.wait_for(|s| *s) => return,
            job = jobs.recv() => match job {
                Some(j) => j,
                None => return,
            },
        };
        let Some(svc) = svc.upgrade() else { return };
        svc.run_send(&key, job).await;
    }
}

/// Mirrors the app's `ShareHandle::stop`: router shutdown (2 s cap), then close.
pub(crate) async fn stop_share(share: SendResult) {
    match tokio::time::timeout(Duration::from_secs(2), share.router.shutdown()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!("share router shutdown error: {e}"),
        Err(_) => tracing::warn!("share router shutdown timed out after 2 s"),
    }
    share.router.endpoint().close().await;
}

/// A not-found query lists what is paired, so the caller can pick; never a guess.
fn unresolved(e: ResolveError, devices: &[PairedDeviceInfo]) -> anyhow::Error {
    if let ResolveError::NotFound { .. } = e {
        let mut msg = e.to_string();
        if devices.is_empty() {
            msg.push_str("; this node has no paired devices");
        } else {
            msg.push_str("; paired devices:");
            for d in devices {
                let _ = write!(msg, "\n  {} ({})", d.display_name, d.endpoint_id);
            }
        }
        return anyhow!(msg);
    }
    anyhow!(e)
}

/// Per-share emitter: progress into the record, plus activity for the worker.
struct SendProgress {
    registry: Registry,
    id: String,
    bytes: AtomicU64,
    started: AtomicBool,
}

impl SendProgress {
    fn new(registry: Registry, id: String) -> Self {
        Self {
            registry,
            id,
            bytes: AtomicU64::new(0),
            started: AtomicBool::new(false),
        }
    }

    fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::SeqCst)
    }

    fn started(&self) -> bool {
        self.started.load(Ordering::SeqCst)
    }
}

impl EventEmitter for SendProgress {
    fn emit_event(&self, name: &str) -> Result<(), String> {
        if name == "transfer-started" {
            self.started.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    fn emit_event_with_payload(&self, name: &str, payload: &str) -> Result<(), String> {
        if name == "transfer-progress" {
            if let Some((bytes, _total)) = parse_progress(payload) {
                self.bytes.fetch_max(bytes, Ordering::SeqCst);
                self.registry.update_live(&self.id, |r| {
                    r.bytes_done = bytes.min(r.total_bytes);
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str, id: &str) -> PairedDeviceInfo {
        PairedDeviceInfo {
            endpoint_id: id.to_string(),
            display_name: name.to_string(),
            device_type: "laptop".to_string(),
            os: "macos".to_string(),
            paired_at: 0,
            last_seen_at: 0,
            relay_url: None,
            pairing_status: Default::default(),
            trusted: false,
            online: false,
        }
    }

    /// The daemon spawns these; a lock held across an await would break that.
    #[allow(dead_code)]
    fn futures_are_send(svc: Arc<AgentService>) {
        fn assert_send<T: Send>(_: T) {}
        assert_send(svc.send_to_device("x", vec![], PathBuf::new()));
        assert_send(svc.cancel_transfer("x"));
    }

    #[test]
    fn not_found_lists_paired_devices() {
        let devices = [dev("MacBook", "aaaa1111"), dev("Pixel", "bbbb2222")];
        let err = resolve_device(&devices, "Macbok").unwrap_err();
        let msg = unresolved(err, &devices).to_string();
        assert!(msg.contains("Macbok"), "{msg}");
        assert!(msg.contains("MacBook (aaaa1111)"), "{msg}");
        assert!(msg.contains("Pixel (bbbb2222)"), "{msg}");

        let err = resolve_device(&[], "x").unwrap_err();
        assert!(unresolved(err, &[])
            .to_string()
            .contains("no paired devices"));
    }

    #[test]
    fn progress_never_touches_terminal_records() {
        let reg = Registry::new(10);
        let rec = reg.insert(TransferRecord {
            id: String::new(),
            seq: 0,
            direction: Direction::Outbound,
            kind: TransferKind::Paired,
            peer: None,
            file_names: vec![],
            total_bytes: 100,
            bytes_done: 0,
            status: TransferStatus::Transferring,
            created_at_ms: 0,
            updated_at_ms: 0,
            error: None,
            saved_paths: vec![],
            ticket: None,
            expires_at_ms: None,
        });
        let p = SendProgress::new(reg.clone(), rec.id.clone());
        p.emit_event("transfer-started").unwrap();
        p.emit_event_with_payload("transfer-progress", "250:300:0")
            .unwrap();
        assert!(p.started());
        assert_eq!(p.bytes(), 250);
        // Capped at the record's total.
        assert_eq!(reg.get(&rec.id).unwrap().bytes_done, 100);

        let done = reg
            .update(&rec.id, |r| r.status = TransferStatus::Cancelled)
            .unwrap();
        p.emit_event_with_payload("transfer-progress", "5:300:0")
            .unwrap();
        assert_eq!(reg.get(&rec.id).unwrap().seq, done.seq);
    }
}
