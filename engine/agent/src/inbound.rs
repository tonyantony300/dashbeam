//! Inbound invites: auto-accept from trusted devices, pending invites from
//! paired-but-untrusted ones, and the policy for unpaired (nearby) senders.

use crate::registry::{Direction, Peer, Registry, TransferKind, TransferRecord, TransferStatus};
use crate::service::AgentService;
use anyhow::{anyhow, bail, Context, Result};
use engine::{EventEmitter, PairedDeviceInfo, ReceiveOptions};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;
use tokio::sync::{oneshot, watch, Notify};
use tokio::time::Instant;

/// Same cap as the app's `AUTO_ACCEPT_QUEUE_LIMIT`; also bounds each sender's pending FIFO.
pub const AUTO_ACCEPT_QUEUE_LIMIT: usize = 20;

/// `paired-invite-received` payload; sent for paired and nearby senders alike.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct InvitePayload {
    pub blob_ticket: String,
    pub file_count: u32,
    pub total_size: u64,
    pub sender_name: String,
    pub remote_endpoint_id: String,
}

/// `nearby-pair-request-received` payload.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PairRequestPayload {
    pub remote_endpoint_id: String,
    #[serde(default)]
    pub sender_name: String,
}

/// An invite waiting for an answer. At most one per device is listed: the oldest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingInvite {
    pub invite_id: String,
    pub endpoint_id: String,
    pub sender_name: String,
    pub file_count: u32,
    pub total_size: u64,
    pub received_at_ms: u64,
}

/// An `accept_incoming` / `decline_incoming` call, handed to the sender's driver.
enum Answer {
    Accept(oneshot::Sender<Result<TransferRecord>>),
    Decline(oneshot::Sender<Result<()>>),
}

struct PendingEntry {
    info: PendingInvite,
    ticket: String,
    nearby: bool,
    deadline: Instant,
    /// Whoever takes this owns the answer: an API call, or the driver itself.
    answer_tx: Option<oneshot::Sender<Answer>>,
    answer_rx: Option<oneshot::Receiver<Answer>>,
}

#[derive(Default)]
struct AutoQueue {
    queue: VecDeque<InvitePayload>,
    in_flight: Option<String>,
}

pub(crate) struct Inbound {
    auto: Mutex<AutoQueue>,
    auto_notify: Arc<Notify>,
    /// Per-sender FIFO keyed by lowercase endpoint id. A key exists exactly
    /// while that sender's driver task runs.
    pending: Mutex<HashMap<String, VecDeque<PendingEntry>>>,
    /// One download at a time per destination folder: the engine's export
    /// checks for an existing file and then copies, so two concurrent
    /// downloads into one folder could overwrite each other.
    folder_locks: Mutex<HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>>,
}

impl Inbound {
    pub(crate) fn new() -> Self {
        Self {
            auto: Mutex::new(AutoQueue::default()),
            auto_notify: Arc::new(Notify::new()),
            pending: Mutex::new(HashMap::new()),
            folder_locks: Mutex::new(HashMap::new()),
        }
    }

    fn folder_lock(&self, dir: &Path) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.folder_locks.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, l| l.strong_count() > 0);
        if let Some(lock) = map.get(dir).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        map.insert(dir.to_path_buf(), Arc::downgrade(&lock));
        lock
    }

    pub(crate) fn auto_notify(&self) -> Arc<Notify> {
        self.auto_notify.clone()
    }

    fn auto(&self) -> MutexGuard<'_, AutoQueue> {
        self.auto.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn pending(&self) -> MutexGuard<'_, HashMap<String, VecDeque<PendingEntry>>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn take_auto(&self) -> Option<InvitePayload> {
        let mut q = self.auto();
        let next = q.queue.pop_front()?;
        q.in_flight = Some(next.blob_ticket.clone());
        Some(next)
    }

    fn finish_auto(&self) {
        self.auto().in_flight = None;
    }
}

enum Verdict {
    Answered(Answer),
    Approved,
    Denied,
    Expired,
}

impl AgentService {
    /// Oldest pending invite per device, oldest first.
    pub fn list_incoming(&self) -> Vec<PendingInvite> {
        let mut out: Vec<PendingInvite> = self
            .inbound
            .pending()
            .values()
            .filter_map(|q| q.front())
            .map(|e| e.info.clone())
            .collect();
        out.sort_by_key(|i| i.received_at_ms);
        out
    }

    /// Accepts a pending invite and starts the download. Returns the new record.
    pub async fn accept_incoming(&self, invite_id: &str) -> Result<TransferRecord> {
        let (tx, rx) = oneshot::channel();
        self.answer_pending(invite_id, |nearby| {
            if nearby && !self.allow_agent_pairing() {
                bail!(
                    "invite {invite_id} is from an unpaired nearby device; accepting it would \
                     pair that device, which needs a human (or --dangerously-allow-agent-pairing)"
                );
            }
            Ok(Answer::Accept(tx))
        })?;
        rx.await
            .map_err(|_| anyhow!("invite {invite_id} is no longer pending"))?
    }

    /// Declines a pending invite and tells the sender.
    pub async fn decline_incoming(&self, invite_id: &str) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.answer_pending(invite_id, |_| Ok(Answer::Decline(tx)))?;
        rx.await
            .map_err(|_| anyhow!("invite {invite_id} is no longer pending"))?
    }

    fn answer_pending(
        &self,
        invite_id: &str,
        make: impl FnOnce(bool) -> Result<Answer>,
    ) -> Result<()> {
        let mut map = self.inbound.pending();
        for q in map.values_mut() {
            let Some(pos) = q.iter().position(|e| e.info.invite_id == invite_id) else {
                continue;
            };
            if pos != 0 {
                bail!(
                    "invite {invite_id} is queued behind an older invite from the same device; \
                     answer that one first"
                );
            }
            let entry = &mut q[0];
            let answer = make(entry.nearby)?;
            let tx = entry
                .answer_tx
                .take()
                .ok_or_else(|| anyhow!("invite {invite_id} is already being answered"))?;
            return tx
                .send(answer)
                .map_err(|_| anyhow!("invite {invite_id} is no longer pending"));
        }
        bail!("no pending invite with id {invite_id}")
    }

    /// `paired-invite-received`. Routing is synchronous so invites from one
    /// sender keep their order; anything that awaits is spawned.
    pub(crate) fn on_invite_received(self: &Arc<Self>, payload: Option<&str>) {
        let Some(payload) = payload else { return };
        match serde_json::from_str::<InvitePayload>(payload) {
            Ok(invite) => self.route_invite(invite),
            Err(e) => tracing::warn!("malformed paired-invite-received payload: {e}"),
        }
    }

    fn route_invite(self: &Arc<Self>, invite: InvitePayload) {
        let device = match self.paired_device(&invite.remote_endpoint_id) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("dropping invite: could not read paired devices: {e:#}");
                return;
            }
        };
        let authorized = self.is_authorized(&invite.remote_endpoint_id);
        match route_for(device.as_ref(), authorized) {
            Route::Auto => self.enqueue_auto(invite),
            Route::Pending { nearby } => self.hold_pending(invite, nearby),
            Route::PreApproved => {
                let svc = self.clone();
                tokio::spawn(async move {
                    let info = pending_info(&invite);
                    if let Err(e) = svc.accept_invite(&info, invite.blob_ticket, true).await {
                        tracing::warn!(
                            sender = %info.endpoint_id,
                            "could not accept pre-approved nearby invite: {e:#}"
                        );
                    }
                });
            }
        }
    }

    fn enqueue_auto(&self, invite: InvitePayload) {
        {
            let mut q = self.inbound.auto();
            let dup = q.in_flight.as_deref() == Some(invite.blob_ticket.as_str())
                || q.queue.iter().any(|i| i.blob_ticket == invite.blob_ticket);
            if dup {
                return;
            }
            if q.queue.len() >= AUTO_ACCEPT_QUEUE_LIMIT {
                tracing::warn!(
                    sender = %invite.remote_endpoint_id,
                    "auto-accept queue full ({AUTO_ACCEPT_QUEUE_LIMIT}); dropping invite"
                );
                return;
            }
            q.queue.push_back(invite);
        }
        self.inbound.auto_notify.notify_one();
    }

    /// One auto-accepted invite: notify the sender, then download in place.
    async fn process_auto(self: &Arc<Self>, invite: InvitePayload) {
        // Trust may have changed while the invite waited; the later state counts.
        match self.paired_device(&invite.remote_endpoint_id) {
            Ok(Some(d)) if should_auto_accept(&d) => {}
            _ => return self.route_invite(invite),
        }
        let id = invite.remote_endpoint_id.clone();
        let node = self.node().clone();
        tokio::spawn(async move {
            // Best-effort notify, as the app does; the transfer proceeds regardless.
            if let Err(e) = node.respond_paired_invite(&id, true).await {
                tracing::debug!(sender = %id, "could not notify sender of auto-accept: {e:#}");
            }
        });
        let name = self.peer_name(&invite.remote_endpoint_id, &invite.sender_name);
        let rec = self.insert_inbound(&invite.remote_endpoint_id, &name, invite.total_size);
        self.receive(
            rec.id,
            invite.blob_ticket,
            &invite.remote_endpoint_id,
            &name,
        )
        .await;
    }

    fn hold_pending(self: &Arc<Self>, invite: InvitePayload, nearby: bool) {
        let key = invite.remote_endpoint_id.to_lowercase();
        let (tx, rx) = oneshot::channel();
        let entry = PendingEntry {
            info: pending_info(&invite),
            ticket: invite.blob_ticket,
            nearby,
            deadline: Instant::now() + Duration::from_secs(self.config().invite_timeout_secs),
            answer_tx: Some(tx),
            answer_rx: Some(rx),
        };
        let start_driver = {
            let mut map = self.inbound.pending();
            match map.get_mut(&key) {
                Some(q) => {
                    if q.iter().any(|e| e.ticket == entry.ticket) {
                        return;
                    }
                    if q.len() >= AUTO_ACCEPT_QUEUE_LIMIT {
                        tracing::warn!(
                            sender = %entry.info.endpoint_id,
                            "pending invite queue full ({AUTO_ACCEPT_QUEUE_LIMIT}); dropping invite"
                        );
                        return;
                    }
                    q.push_back(entry);
                    false
                }
                None => {
                    map.insert(key.clone(), VecDeque::from([entry]));
                    true
                }
            }
        };
        if start_driver {
            tokio::spawn(self.clone().drive_pending(key));
        }
    }

    /// Answers one sender's pending invites in order: whichever comes first of
    /// an API answer, the approver, or expiry decides the head.
    async fn drive_pending(self: Arc<Self>, key: String) {
        let mut stop = self.stop_signal();
        loop {
            let (info, ticket, nearby, deadline, rx) = {
                let mut map = self.inbound.pending();
                let Some(q) = map.get_mut(&key) else { return };
                let Some(head) = q.front_mut() else {
                    map.remove(&key);
                    return;
                };
                (
                    head.info.clone(),
                    head.ticket.clone(),
                    head.nearby,
                    head.deadline,
                    head.answer_rx.take(),
                )
            };
            let Some(mut rx) = rx else {
                tracing::error!("pending invite without an answer channel; dropping it");
                self.pop_pending(&key);
                continue;
            };

            let consult = consult_approver(
                nearby,
                self.allow_agent_pairing(),
                self.approver().answers_paired_invites(),
            );
            let approver = self.approver().clone();
            let ask = async {
                if consult {
                    approver
                        .approve_invite(
                            &info.sender_name,
                            &info.endpoint_id,
                            info.file_count,
                            info.total_size,
                        )
                        .await
                } else {
                    std::future::pending().await
                }
            };
            let verdict = tokio::select! {
                answer = &mut rx => match answer {
                    Ok(a) => Verdict::Answered(a),
                    // Unreachable: the sender lives in the entry until taken and used.
                    Err(_) => {
                        tracing::error!("pending invite lost its answer channel; dropping it");
                        self.pop_pending(&key);
                        continue;
                    }
                },
                approved = ask => if approved { Verdict::Approved } else { Verdict::Denied },
                _ = tokio::time::sleep_until(deadline) => Verdict::Expired,
                _ = stop.wait_for(|s| *s) => return,
            };
            let verdict = match verdict {
                Verdict::Answered(a) => Verdict::Answered(a),
                own => {
                    // Claim the head; an API answer that got in first wins.
                    let claimed = self
                        .inbound
                        .pending()
                        .get_mut(&key)
                        .and_then(|q| q.front_mut())
                        .and_then(|e| e.answer_tx.take())
                        .is_some();
                    if claimed {
                        own
                    } else {
                        match rx.await {
                            Ok(a) => Verdict::Answered(a),
                            Err(_) => own,
                        }
                    }
                }
            };
            self.pop_pending(&key);

            match verdict {
                Verdict::Answered(Answer::Accept(reply)) => {
                    let _ = reply.send(self.accept_invite(&info, ticket, nearby).await);
                }
                Verdict::Answered(Answer::Decline(reply)) => {
                    let _ = reply.send(self.decline_invite(&info, nearby).await);
                }
                Verdict::Approved => {
                    if let Err(e) = self.accept_invite(&info, ticket, nearby).await {
                        tracing::warn!(sender = %info.endpoint_id, "approved invite failed: {e:#}");
                    }
                }
                Verdict::Denied | Verdict::Expired => {
                    let expired = matches!(verdict, Verdict::Expired);
                    tracing::info!(
                        sender = %info.endpoint_id,
                        expired,
                        "declining incoming invite"
                    );
                    if let Err(e) = self.decline_invite(&info, nearby).await {
                        tracing::debug!(sender = %info.endpoint_id, "decline not delivered: {e:#}");
                    }
                }
            }
        }
    }

    fn pop_pending(&self, key: &str) {
        if let Some(q) = self.inbound.pending().get_mut(key) {
            q.pop_front();
        }
    }

    /// Nearby: `accept_nearby_invite` records the pairing first. The download
    /// is spawned; the returned record is still `Transferring`.
    async fn accept_invite(
        self: &Arc<Self>,
        info: &PendingInvite,
        ticket: String,
        nearby: bool,
    ) -> Result<TransferRecord> {
        let id = info.endpoint_id.clone();
        if nearby {
            self.node()
                .accept_nearby_invite(&id)
                .await
                .context("accept nearby invite")?;
        }
        let name = self.peer_name(&id, &info.sender_name);
        let rec = self.insert_inbound(&id, &name, info.total_size);
        let svc = self.clone();
        let rec_id = rec.id.clone();
        let sender = id.clone();
        tokio::spawn(async move { svc.receive(rec_id, ticket, &sender, &name).await });
        if !nearby {
            if let Err(e) = self.node().respond_paired_invite(&id, true).await {
                tracing::debug!(sender = %id, "could not notify sender of accept: {e:#}");
            }
        }
        Ok(rec)
    }

    async fn decline_invite(&self, info: &PendingInvite, nearby: bool) -> Result<()> {
        if nearby {
            self.node()
                .decline_nearby_invite(&info.endpoint_id, false)
                .await
        } else {
            self.node()
                .respond_paired_invite(&info.endpoint_id, false)
                .await
        }
    }

    /// `nearby-pair-request-received`: accepted only for `pairing.authorized`
    /// ids or a human's yes. `--dangerously-allow-agent-pairing` does not
    /// accept pair requests.
    pub(crate) fn on_pair_request(self: &Arc<Self>, payload: Option<&str>) {
        let Some(payload) = payload else { return };
        let req = match serde_json::from_str::<PairRequestPayload>(payload) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("malformed nearby-pair-request-received payload: {e}");
                return;
            }
        };
        let svc = self.clone();
        tokio::spawn(async move {
            let id = req.remote_endpoint_id.as_str();
            let accept = svc.is_authorized(id) || {
                let timeout = Duration::from_secs(svc.config().invite_timeout_secs);
                let ask = svc.approver().approve_pairing(&req.sender_name, id);
                let mut stop = svc.stop_signal();
                tokio::select! {
                    r = tokio::time::timeout(timeout, ask) => r.unwrap_or(false),
                    _ = stop.wait_for(|s| *s) => return,
                }
            };
            let res = if accept {
                svc.node().accept_nearby_invite(id).await
            } else {
                svc.node().decline_nearby_invite(id, false).await
            };
            match res {
                Ok(()) => tracing::info!(sender = %id, accept, "answered nearby pair request"),
                Err(e) => tracing::warn!(sender = %id, accept, "nearby pair request: {e:#}"),
            }
        });
    }

    pub(crate) fn paired_device(&self, endpoint_id: &str) -> Result<Option<PairedDeviceInfo>> {
        Ok(self
            .list_devices()?
            .into_iter()
            .find(|d| d.endpoint_id.eq_ignore_ascii_case(endpoint_id)))
    }

    /// Listed in `pairing.authorized`: the only automatic yes for an unpaired sender.
    fn is_authorized(&self, endpoint_id: &str) -> bool {
        self.config()
            .pairing
            .authorized
            .iter()
            .any(|p| p.endpoint_id.eq_ignore_ascii_case(endpoint_id))
    }

    /// The locally stored name wins, as in the app's `subFolderFor`.
    fn peer_name(&self, endpoint_id: &str, sender_name: &str) -> String {
        self.paired_device(endpoint_id)
            .ok()
            .flatten()
            .map(|d| d.display_name.trim().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| sender_name.trim().to_string())
    }

    /// Inserts the record and its cancel slot; [`Self::receive`] starts it.
    fn insert_inbound(&self, endpoint_id: &str, name: &str, total: u64) -> TransferRecord {
        let rec = self.registry().insert(TransferRecord {
            id: String::new(),
            seq: 0,
            direction: Direction::Inbound,
            kind: TransferKind::Paired,
            peer: Some(Peer {
                name: Some(name.to_string()),
                endpoint_id: Some(endpoint_id.to_string()),
            }),
            file_names: Vec::new(),
            total_bytes: total,
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
        rec
    }

    /// Downloads into `receive_dir/<sanitized sender>/` and makes the single
    /// terminal update on `rec_id`.
    async fn receive(&self, rec_id: String, ticket: String, endpoint_id: &str, name: &str) {
        let output_dir = self
            .config()
            .receive_dir(self.data_dir())
            .join(sender_folder(name, endpoint_id));
        self.receive_into(rec_id, ticket, output_dir).await;
    }

    /// Runs the download for `rec_id` into `output_dir` and makes the single
    /// terminal update. Shared by paired invites and ticket receives.
    pub(crate) async fn receive_into(&self, rec_id: String, ticket: String, output_dir: PathBuf) {
        // `cancel_transfer` already finalized it.
        let Some(mut cancel_rx) = self.cancels.start(&rec_id) else {
            return;
        };
        // Queue behind other downloads into the same folder; cancellable meanwhile.
        let lock = self.inbound.folder_lock(&output_dir);
        let _guard = tokio::select! {
            guard = lock.lock_owned() => guard,
            _ = &mut cancel_rx => {
                self.cancels.finish(&rec_id);
                tracing::info!(transfer = %rec_id, "inbound transfer cancelled");
                self.registry()
                    .update(&rec_id, |r| r.status = TransferStatus::Cancelled);
                return;
            }
        };
        let progress = Arc::new(ReceiveProgress::new(
            self.registry().clone(),
            rec_id.clone(),
        ));
        let result = self
            .download(ticket, output_dir, progress.clone(), cancel_rx)
            .await;
        let cancelled = !self.cancels.finish(&rec_id);
        match result {
            Ok(out_dir) => {
                let saved = progress.saved_paths(&out_dir);
                self.registry().update(&rec_id, |r| {
                    r.status = TransferStatus::Received;
                    r.bytes_done = r.total_bytes;
                    r.saved_paths = saved;
                });
            }
            Err(_) if cancelled => {
                tracing::info!(transfer = %rec_id, "inbound transfer cancelled");
                self.registry()
                    .update(&rec_id, |r| r.status = TransferStatus::Cancelled);
            }
            Err(e) => {
                tracing::warn!(transfer = %rec_id, "inbound transfer failed: {e:#}");
                self.registry().update(&rec_id, |r| {
                    r.status = TransferStatus::Failed;
                    r.error = Some(format!("{e:#}"));
                });
            }
        }
    }

    /// Relay/discovery from config for downloads and metadata probes.
    pub(crate) fn receive_options(&self, output_dir: Option<PathBuf>) -> Result<ReceiveOptions> {
        let relay_mode = engine::build_relay_mode(self.config().relay.clone())
            .map_err(|e| anyhow!("invalid relay config: {e}"))?;
        let discovery_mode = engine::build_discovery_mode(self.config().discovery.clone())
            .map_err(|e| anyhow!("invalid discovery config: {e}"))?;
        Ok(ReceiveOptions {
            output_dir,
            relay_mode,
            discovery_mode,
            magic_ipv4_addr: None,
            magic_ipv6_addr: None,
        })
    }

    async fn download(
        &self,
        ticket: String,
        output_dir: PathBuf,
        progress: Arc<ReceiveProgress>,
        cancel_rx: oneshot::Receiver<()>,
    ) -> Result<PathBuf> {
        let options = self.receive_options(Some(output_dir))?;
        let result = engine::download(ticket, options, Some(progress), cancel_rx).await?;
        Ok(result.file_path)
    }
}

/// Spawned by `AgentService::start`; drains the auto-accept queue one invite at a time.
pub(crate) async fn run_auto_worker(
    svc: Weak<AgentService>,
    notify: Arc<Notify>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        let next = match svc.upgrade() {
            Some(s) => s.inbound.take_auto(),
            None => return,
        };
        match next {
            Some(invite) => {
                let Some(s) = svc.upgrade() else { return };
                s.process_auto(invite).await;
                s.inbound.finish_auto();
            }
            None => tokio::select! {
                _ = notify.notified() => {}
                _ = stop.wait_for(|s| *s) => return,
            },
        }
    }
}

/// Mirrors the app's `shouldAutoAccept`: trusted and still actively paired.
fn should_auto_accept(d: &PairedDeviceInfo) -> bool {
    d.trusted && d.pairing_status.is_active()
}

#[derive(Debug, PartialEq, Eq)]
enum Route {
    Auto,
    /// Held for an answer; `nearby` = accepting pairs (or re-pairs) the sender.
    Pending {
        nearby: bool,
    },
    /// Unpaired but listed in `pairing.authorized`: accepted without asking.
    PreApproved,
}

/// A record that is no longer actively paired (unpaired remotely, stale
/// identity) routes like no record at all.
fn route_for(device: Option<&PairedDeviceInfo>, authorized: bool) -> Route {
    match device {
        Some(d) if should_auto_accept(d) => Route::Auto,
        Some(d) if d.pairing_status.is_active() => Route::Pending { nearby: false },
        _ if authorized => Route::PreApproved,
        _ => Route::Pending { nearby: true },
    }
}

/// Whether the approver decides a pending invite, racing the API. A
/// non-interactive approver leaves paired invites to the API; nearby invites
/// it declines, unless the dangerous flag lets the API accept them.
fn consult_approver(nearby: bool, allow_agent_pairing: bool, answers_paired: bool) -> bool {
    if nearby && !allow_agent_pairing {
        return true;
    }
    answers_paired
}

fn pending_info(invite: &InvitePayload) -> PendingInvite {
    PendingInvite {
        invite_id: uuid::Uuid::new_v4().to_string(),
        endpoint_id: invite.remote_endpoint_id.clone(),
        sender_name: invite.sender_name.clone(),
        file_count: invite.file_count,
        total_size: invite.total_size,
        received_at_ms: engine::unix_now_ms(),
    }
}

/// Peer-chosen name made safe as one path component; falls back to a short id.
fn sender_folder(name: &str, endpoint_id: &str) -> String {
    let short: String = endpoint_id.chars().take(12).collect();
    let fallback = if short.is_empty() {
        "unknown".to_string()
    } else {
        short
    };
    engine::sanitize_folder_name(name.trim(), &fallback)
}

/// `"{bytes}:{total}:{speed_milli}"` from `protocol::progress::format_progress_payload`.
pub(crate) fn parse_progress(payload: &str) -> Option<(u64, u64)> {
    let mut parts = payload.split(':');
    let bytes = parts.next()?.parse().ok()?;
    let total = parts.next()?.parse().ok()?;
    Some((bytes, total))
}

#[derive(Deserialize)]
struct ExportConflict {
    original: String,
    resolved: String,
}

/// Per-transfer emitter: progress into the record, plus what's needed to
/// report the real saved paths (names, and renames from conflict resolution).
pub(crate) struct ReceiveProgress {
    registry: Registry,
    id: String,
    names: Mutex<Vec<String>>,
    conflicts: Mutex<Vec<ExportConflict>>,
}

impl ReceiveProgress {
    pub(crate) fn new(registry: Registry, id: String) -> Self {
        Self {
            registry,
            id,
            names: Mutex::new(Vec::new()),
            conflicts: Mutex::new(Vec::new()),
        }
    }

    /// One path per received file; the output dir if the names never arrived.
    fn saved_paths(&self, out_dir: &Path) -> Vec<PathBuf> {
        let names = self.names.lock().unwrap_or_else(|e| e.into_inner());
        if names.is_empty() {
            return vec![out_dir.to_path_buf()];
        }
        let conflicts = self.conflicts.lock().unwrap_or_else(|e| e.into_inner());
        names
            .iter()
            .map(|name| {
                let mut path = out_dir.to_path_buf();
                path.extend(name.split('/'));
                conflicts
                    .iter()
                    .find(|c| Path::new(&c.original) == path)
                    .map(|c| PathBuf::from(&c.resolved))
                    .unwrap_or(path)
            })
            .collect()
    }
}

impl EventEmitter for ReceiveProgress {
    fn emit_event(&self, _name: &str) -> Result<(), String> {
        Ok(())
    }

    fn emit_event_with_payload(&self, name: &str, payload: &str) -> Result<(), String> {
        match name {
            "receive-progress" => {
                if let Some((bytes, total)) = parse_progress(payload) {
                    self.registry.update(&self.id, |r| {
                        r.bytes_done = bytes;
                        if total > 0 {
                            r.total_bytes = total;
                        }
                    });
                }
            }
            "receive-file-names" => {
                if let Ok(names) = serde_json::from_str::<Vec<String>>(payload) {
                    *self.names.lock().unwrap_or_else(|e| e.into_inner()) = names.clone();
                    self.registry.update(&self.id, |r| r.file_names = names);
                }
            }
            "receive-conflicts" => {
                if let Ok(c) = serde_json::from_str::<Vec<ExportConflict>>(payload) {
                    *self.conflicts.lock().unwrap_or_else(|e| e.into_inner()) = c;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(trusted: bool, status: engine::PairingStatus) -> PairedDeviceInfo {
        PairedDeviceInfo {
            endpoint_id: "abcd".into(),
            display_name: "Laptop".into(),
            device_type: "laptop".into(),
            os: "macos".into(),
            paired_at: 0,
            last_seen_at: 0,
            relay_url: None,
            pairing_status: status,
            trusted,
            online: false,
        }
    }

    #[test]
    fn inactive_pairing_routes_as_nearby() {
        use engine::PairingStatus::*;
        let pending = |nearby| Route::Pending { nearby };
        assert_eq!(route_for(Some(&device(true, Active)), false), Route::Auto);
        assert_eq!(
            route_for(Some(&device(false, Active)), true),
            pending(false)
        );
        for status in [UnpairedRemotely, StaleLocalIdentity] {
            for trusted in [true, false] {
                let d = device(trusted, status);
                assert_eq!(route_for(Some(&d), false), pending(true), "{status:?}");
                assert_eq!(route_for(Some(&d), true), Route::PreApproved, "{status:?}");
            }
        }
        assert_eq!(route_for(None, false), pending(true));
        assert_eq!(route_for(None, true), Route::PreApproved);
    }

    #[test]
    fn approver_consulted_unless_flag_leaves_nearby_to_api() {
        // Without the flag a nearby invite is always the approver's call.
        assert!(consult_approver(true, false, false));
        assert!(consult_approver(true, false, true));
        // With it, a non-interactive approver leaves it pending for accept_incoming.
        assert!(!consult_approver(true, true, false));
        assert!(consult_approver(true, true, true));
        // Paired invites follow the approver's own preference.
        assert!(!consult_approver(false, false, false));
        assert!(consult_approver(false, true, true));
    }

    #[tokio::test]
    async fn folder_lock_is_shared_per_folder() {
        let inbound = Inbound::new();
        let a = inbound.folder_lock(Path::new("/rx/Laptop"));
        let held = a.clone().lock_owned().await;
        let same = inbound.folder_lock(Path::new("/rx/Laptop"));
        assert!(Arc::ptr_eq(&a, &same));
        assert!(same.try_lock().is_err(), "same folder must wait");
        let other = inbound.folder_lock(Path::new("/rx/Phone"));
        assert!(other.try_lock().is_ok(), "other folders proceed");
        drop((held, a, same, other));
        // Unused locks are pruned.
        inbound.folder_lock(Path::new("/rx/New"));
        assert_eq!(inbound.folder_locks.lock().unwrap().len(), 1);
    }

    #[test]
    fn parses_progress_payload() {
        assert_eq!(parse_progress("5:10:1234"), Some((5, 10)));
        assert_eq!(parse_progress("5:10"), Some((5, 10)));
        assert_eq!(parse_progress("x:10:1"), None);
        assert_eq!(parse_progress(""), None);
    }

    #[test]
    fn sender_folder_never_escapes() {
        assert_eq!(sender_folder("Laptop", "abcdef0123456789"), "Laptop");
        assert_eq!(sender_folder("../../etc", "abcdef0123456789"), "etc");
        assert_eq!(sender_folder("..", "abcdef0123456789"), "abcdef012345");
        assert_eq!(sender_folder("  ", "abcdef0123456789"), "abcdef012345");
        assert!(!sender_folder("a/b\\c", "id").contains(['/', '\\']));
    }

    #[test]
    fn saved_paths_follow_names_and_conflicts() {
        let p = ReceiveProgress::new(Registry::new(1), "x".into());
        let out = Path::new("/rx/Laptop");
        assert_eq!(p.saved_paths(out), vec![out.to_path_buf()]);

        p.emit_event_with_payload("receive-file-names", r#"["a.txt","dir/b.txt"]"#)
            .unwrap();
        p.emit_event_with_payload(
            "receive-conflicts",
            r#"[{"original":"/rx/Laptop/a.txt","resolved":"/rx/Laptop/a (1).txt"}]"#,
        )
        .unwrap();
        assert_eq!(
            p.saved_paths(out),
            vec![
                PathBuf::from("/rx/Laptop/a (1).txt"),
                PathBuf::from("/rx/Laptop/dir/b.txt"),
            ]
        );
    }

    #[test]
    fn progress_updates_record() {
        let reg = Registry::new(10);
        let rec = reg.insert(TransferRecord {
            id: String::new(),
            seq: 0,
            direction: Direction::Inbound,
            kind: TransferKind::Paired,
            peer: None,
            file_names: vec![],
            total_bytes: 1,
            bytes_done: 0,
            status: TransferStatus::Transferring,
            created_at_ms: 0,
            updated_at_ms: 0,
            error: None,
            saved_paths: vec![],
            ticket: None,
            expires_at_ms: None,
        });
        let p = ReceiveProgress::new(reg.clone(), rec.id.clone());
        p.emit_event_with_payload("receive-progress", "40:100:0")
            .unwrap();
        let r = reg.get(&rec.id).unwrap();
        assert_eq!((r.bytes_done, r.total_bytes), (40, 100));
    }
}
