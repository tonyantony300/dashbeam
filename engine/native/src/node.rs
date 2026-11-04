use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use iroh::endpoint::{
    presets, AfterHandshakeOutcome, Connection, Endpoint, EndpointHooks, RelayMode, Side,
};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::address_lookup::pkarr::{PkarrPublisher, PkarrResolver};
use iroh::address_lookup::dns::DnsAddressLookup;
use iroh::EndpointId;
use protocol::{
    allows_unpaired_control, apply_options, export_connection_keying_material, read_message,
    should_answer_identity, should_publish_mdns, sign_challenge, unpaired_message_allowed,
    verify_challenge, write_message, AddrInfoOptions, AppHandle, ControlMessage, Discoverability,
    DiscoveryModeOption, InviteResponse, PairedDevice, PairingStatus, RememberVote,
    CONTROL_ALPN, PRESENCE_CONNECT_TIMEOUT_SECS,
};
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::device_identity::{
    load_or_create_identity_with, DeviceIdentity, DeviceInfo, IdentityOptions, PairedDeviceInfo,
    PairedDeviceStore,
};
use crate::lan_discovery::{LanDiscovery, LanEvent};
use crate::nearby::{NearbyDevice, NearbyRegistry, ObserveOutcome};
use crate::paired_connections::{invite_wait_timeout, PairedConnectionManager};
use crate::pairing_util::{build_control_connect_addr, set_presence};
use crate::rate_limit::UnpairedRateLimiter;
use crate::runtime::NodeRuntime;

#[derive(Debug)]
pub(crate) struct AccessState {
    pub(crate) allowed: HashSet<EndpointId>,
    /// Peers that may pair even when no pairing host is open.
    pub(crate) authorized: HashSet<EndpointId>,
    pub(crate) pairing_host_open: bool,
    pub(crate) discoverability: Discoverability,
}

/// Grace period for an unpaired peer's direct path to appear — the handshake
/// can complete over the relay before hole punching validates a direct path.
const UNPAIRED_DIRECT_PATH_DEADLINE: Duration = Duration::from_secs(3);

/// Poll interval while waiting out [`UNPAIRED_DIRECT_PATH_DEADLINE`].
const UNPAIRED_DIRECT_PATH_POLL: Duration = Duration::from_millis(100);

/// Waits up to [`UNPAIRED_DIRECT_PATH_DEADLINE`] for `conn` to have a direct
/// (non-relay) path. Not a LAN check — a successful hole punch counts too —
/// but it shuts out strangers reaching us relay-only with just our endpoint id.
async fn has_direct_path(conn: &Connection) -> bool {
    let deadline = tokio::time::Instant::now() + UNPAIRED_DIRECT_PATH_DEADLINE;
    loop {
        if conn.paths().iter().any(|path| path.is_ip()) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(UNPAIRED_DIRECT_PATH_POLL).await;
    }
}

#[derive(Debug)]
struct PairedOnlyHook {
    access: Arc<RwLock<AccessState>>,
    blocked: Arc<RwLock<HashSet<String>>>,
}

impl EndpointHooks for PairedOnlyHook {
    async fn after_handshake(&self, conn: &Connection) -> AfterHandshakeOutcome {
        if conn.side() != Side::Server {

            return AfterHandshakeOutcome::accept();
        }
        if conn.alpn() != CONTROL_ALPN {

            return AfterHandshakeOutcome::accept();
        }
        let remote = conn.remote_id();
        if self.blocked.read().await.contains(&remote.to_string()) {
            return AfterHandshakeOutcome::Reject {
                error_code: 403u32.into(),
                reason: b"blocked peer".to_vec(),
            };
        }
        let (allowed, authorized, pairing_host_open, discoverability) = {
            let access = self.access.read().await;
            (
                access.allowed.contains(&remote),
                access.authorized.contains(&remote),
                access.pairing_host_open,
                access.discoverability,
            )
        };
        if pairing_host_open || allowed || authorized {

            return AfterHandshakeOutcome::accept();
        }

        // Strangers need `Everyone` *and* a direct path, or anyone holding our
        // endpoint id could pop invite dialogs over the relay. Paired peers
        // (accepted above) legitimately run relay-only.
        if allows_unpaired_control(discoverability) && has_direct_path(conn).await {
            return AfterHandshakeOutcome::accept();
        }

        AfterHandshakeOutcome::Reject {
            error_code: 403u32.into(),
            reason: b"unauthorized control peer".to_vec(),
        }
    }
}

#[derive(Clone)]
struct ControlCtx {
    identity: Arc<DeviceIdentity>,
    paired_store: Arc<PairedDeviceStore>,
    access: Arc<RwLock<AccessState>>,
    pairing_host_persistent: Arc<AtomicBool>,
    app_handle: AppHandle,
    /// Updated in the background once the endpoint reaches a home relay.
    home_relay_url: Arc<std::sync::RwLock<Option<String>>>,
    presence: Arc<std::sync::RwLock<HashMap<String, bool>>>,
    paired_connections: Arc<PairedConnectionManager>,
    /// Nearby invites sent and not yet answered. An unpaired peer's
    /// `InviteResponse` is only honoured if it's in here, so a stranger can't
    /// spoof an acceptance for an invite we never sent.
    pending_nearby_invites: Arc<RwLock<HashMap<String, PendingNearbyInvite>>>,
    /// Devices seen on the LAN but not yet paired. Shared with `NodeService`
    /// so `commit_nearby_pairing` can expire an entry it just promoted.
    nearby: Arc<Mutex<NearbyRegistry>>,
    /// Throttles control messages from unpaired peers. Shared with
    /// `NodeService` so a network rebuild doesn't reset the allowance.
    unpaired_limiter: Arc<std::sync::Mutex<UnpairedRateLimiter>>,
}

/// Identity snapshot taken when we invite a nearby peer — the Nearby entry may
/// have expired by the time they answer, so this stands alone.
#[derive(Debug, Clone)]
struct PendingNearbyInvite {
    display_name: String,
    device_type: String,
    os: String,
}

#[derive(Clone)]
struct ControlProtocol {
    ctx: ControlCtx,
}

impl std::fmt::Debug for ControlProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlProtocol").finish_non_exhaustive()
    }
}

impl ControlProtocol {
    async fn handle_connection(&self, conn: Connection) -> anyhow::Result<()> {
        let remote = conn.remote_id();
        let (mut pairing_host_open, authorized) = {
            let access = self.ctx.access.read().await;
            (access.pairing_host_open, access.authorized.contains(&remote))
        };
        let allowed = self.is_allowed(&remote).await;

        if allowed {

            return self.handle_control_session(conn, true).await;
        }

        // Authorized peers get the pairing-host session without an open host.
        pairing_host_open |= authorized;

        if !pairing_host_open {
            // Accepted only because we're discoverable: serve the reduced
            // unpaired message set (identity probes, unpaired invites).
            return self.handle_control_session(conn, false).await;
        }

        let keying = export_connection_keying_material(&conn).context("export keying material")?;

        let (mut send, mut recv) = match conn.accept_bi().await {
            Ok(streams) => {

                streams
            }
            Err(err) => {
                return Err(err).context("accept bi stream for control session");
            }
        };

        let our_info = ControlMessage::PairingInfo {
            endpoint_id: self.ctx.identity.endpoint_id(),
            display_name: self.ctx.identity.display_name(),
            device_type: self.ctx.identity.device_type(),
            os: self.ctx.identity.os(),
            signature: sign_challenge(&self.ctx.identity.secret_key, &keying),
        };
        write_message(&mut send, &our_info)
            .await
            .context("write local PairingInfo")?;

        let mut remote_info: Option<ControlMessage> = None;
        let mut remote_vote: Option<RememberVote> = None;
        let mut pairing_completed = false;
        let mut invite_received = false;
        let session_id = uuid::Uuid::new_v4().to_string();

        loop {
            let msg = match read_message(&mut recv).await {
                Ok(m) => m,
                Err(_err) => {
                    break;
                }
            };
            match msg {
                ControlMessage::PairingInfo {
                    endpoint_id,
                    display_name,
                    device_type,
                    os,
                    signature,
                } => {
                    let Ok(peer_id) = EndpointId::from_str(&endpoint_id) else {

                        continue;
                    };
                    if !verify_challenge(&peer_id, &keying, &signature) {

                        continue;
                    }

                    remote_info = Some(ControlMessage::PairingInfo {
                        endpoint_id,
                        display_name,
                        device_type,
                        os,
                        signature,
                    });
                }
                ControlMessage::RememberVote { vote, .. } => {
                    remote_vote = Some(vote);
                }
                ControlMessage::Invite {
                    blob_ticket,
                    file_count,
                    total_size,
                    sender_name,
                } => {
                    let allowed = self.is_allowed(&remote).await;


                    if !allowed {

                        continue;
                    }
                    invite_received = true;
                    let payload = serde_json::json!({
                        "blob_ticket": blob_ticket,
                        "file_count": file_count,
                        "total_size": total_size,
                        "sender_name": sender_name,
                        "remote_endpoint_id": remote.to_string(),
                    });
                    if let Some(handle) = &self.ctx.app_handle {
                        let _ = handle.emit_event_with_payload(
                            "paired-invite-received",
                            &payload.to_string(),
                        );
                    }
                }
                ControlMessage::InviteResponse { response, .. } => {
                    let response_str = match response {
                        InviteResponse::Accepted => "accepted",
                        InviteResponse::Declined => "declined",
                    };
                    debug!(?response, "invite response from {remote}");
                    crate::pairing_util::emit_paired_invite_response(
                        &self.ctx.app_handle,
                        &self.ctx.paired_store,
                        &remote.to_string(),
                        response_str,
                        None,
                    );
                }
                ControlMessage::Recognition { signature } => {
                    if verify_challenge(&remote, &keying, &signature) {

                        let now = protocol::identity::unix_now_ms();
                        let _ = self.ctx.paired_store.touch(&remote.to_string(), now);
                        set_presence(
                            &self.ctx.presence,
                            &self.ctx.app_handle,
                            &self.ctx.paired_store,
                            &remote.to_string(),
                            true
                        );
                    }
                }
                ControlMessage::Forget { signature } => {
                    if verify_challenge(&remote, &keying, &signature) {

                        if let Ok(Some(device)) = self
                            .ctx
                            .paired_store
                            .mark_unpaired_remotely(&remote.to_string())
                        {
                            {
                                let mut access = self.ctx.access.write().await;
                                access.allowed.remove(&remote);
                            }
                            set_presence(
                                &self.ctx.presence,
                                &self.ctx.app_handle,
                                &self.ctx.paired_store,
                                &remote.to_string(),
                                false
                            );
                            self.ctx
                                .paired_connections
                                .forget(&remote.to_string())
                                .await;
                            let payload = serde_json::json!({
                                "endpoint_id": device.endpoint_id,
                                "display_name": device.display_name,
                                "reason": "remote",
                            });
                            if let Some(handle) = &self.ctx.app_handle {
                                let _ = handle.emit_event_with_payload(
                                    "device-unpaired",
                                    &payload.to_string(),
                                );
                            }
                        }
                    }
                }
                ControlMessage::WhoAreYou
                | ControlMessage::Identity { .. }
                | ControlMessage::PairRequest { .. } => {
                    // Discovery / pair-request messages; ignore on pairing-host sessions
                }
            }

            if remote_info.is_some() && remote_vote == Some(RememberVote::Remember) {

                if let Some(ControlMessage::PairingInfo {
                    endpoint_id,
                    display_name,
                    device_type,
                    os,
                    ..
                }) = &remote_info
                {
                    let now = protocol::identity::unix_now_ms();
                    let relay_url = self
                        .ctx
                        .home_relay_url
                        .read()
                        .expect("home_relay_url")
                        .clone();
                    let device = PairedDevice {
                        endpoint_id: endpoint_id.clone(),
                        display_name: display_name.clone(),
                        device_type: device_type.clone(),
                        os: os.clone(),
                        paired_at: now,
                        last_seen_at: now,
                        relay_url,
                        pairing_status: PairingStatus::Active,
                        trusted: false,
                    };
                    let _ = self.ctx.paired_store.remember(device);
                    self.allow_peer(remote).await;
                    self.ctx.paired_connections.refresh().await;

                    crate::pairing_util::emit_device_paired(&self.ctx.app_handle, display_name);
                }
                pairing_completed = true;
                break;
            }
        }

        if remote_info.is_some() {

            let vote = ControlMessage::RememberVote {
                session_id,
                vote: RememberVote::Remember,
            };
            let _ = write_message(&mut send, &vote).await;
        }

        if pairing_completed {
            let persistent = self
                .ctx
                .pairing_host_persistent
                .load(Ordering::SeqCst);
            if !persistent {
                self.ctx.access.write().await.pairing_host_open = false;

            }
            // Hold the session until the joiner reads our messages and disconnects.
            drop(send);
            drop(recv);

            match tokio::time::timeout(Duration::from_secs(30), conn.closed()).await {
                Ok(_closed) => {},
                Err(_) => {
                    }
            }
        } else if invite_received {
            // Keep the session open until the sender finishes reading our side.
            drop(send);
            drop(recv);

            match tokio::time::timeout(Duration::from_secs(15), conn.closed()).await {
                Ok(_closed) => {},
                Err(_) => {
                    }
            }
        }

        Ok(())
    }

    /// Message loop for an accepted control connection. An unpaired peer may
    /// only probe identity or send an unpaired invite.
    ///
    /// Registration with `paired_connections` is lazy so a probe-only
    /// connection can't clobber the peer's persistent session entry, and
    /// `peer_is_paired` is only ever upgraded — a pairing can commit while
    /// this session is already open.
    async fn handle_control_session(
        &self,
        conn: Connection,
        mut peer_is_paired: bool,
    ) -> anyhow::Result<()> {
        let remote = conn.remote_id();
        let endpoint_id = remote.to_string();
        let mut registered = false;

        let keying = export_connection_keying_material(&conn)
            .context("export keying material for control session")?;

        loop {
            let (mut send, mut recv) = match conn.accept_bi().await {
                Ok(streams) => {

                    streams
                }
                Err(_err) => {

                    break;
                }
            };

            let msg = match read_message(&mut recv).await {
                Ok(m) => m,
                Err(_err) => {

                    continue;
                }
            };

            // Re-read the allowlist before refusing: nearby pairing commits on
            // a sibling connection, so this session can be accepted before the
            // commit and used after it.
            if !peer_is_paired && !unpaired_message_allowed(&msg) {
                peer_is_paired = self.is_allowed(&remote).await;
                if !peer_is_paired {
                    tracing::debug!(
                        target: "dashbeam::_events::control::rejected_unpaired",
                        remote = %remote.fmt_short(),
                        kind = msg.kind(),
                    );
                    conn.close(403u32.into(), b"not permitted for unpaired peer");
                    break;
                }
            }

            // Unpaired peers pay a token per message; an empty bucket means a
            // probe/invite loop. Paired peers are never charged.
            if !peer_is_paired {
                let allowed_now = self
                    .ctx
                    .unpaired_limiter
                    .lock()
                    .expect("unpaired limiter lock")
                    .allow(&endpoint_id, std::time::Instant::now());
                if !allowed_now {
                    tracing::debug!("rate-limiting unpaired control messages from {remote}");
                    conn.close(429u32.into(), b"unpaired control rate limit");
                    break;
                }
            }

            if let ControlMessage::WhoAreYou = msg {
                let setting = self.ctx.access.read().await.discoverability;
                if !should_answer_identity(setting, peer_is_paired) {
                    if !peer_is_paired {
                        conn.close(403u32.into(), b"not discoverable");
                        break;
                    }
                    // A paired peer's session must survive a refused probe.
                    continue;
                }
                let info = DeviceInfo::from(self.ctx.identity.as_ref());
                let reply = ControlMessage::Identity {
                    endpoint_id: info.endpoint_id,
                    display_name: info.display_name,
                    device_type: info.device_type,
                    os: info.os,
                };
                if let Err(err) = write_message(&mut send, &reply).await {
                    tracing::debug!("identity reply failed: {err:#}");
                }
                continue;
            }

            if peer_is_paired && !registered {
                self.ctx
                    .paired_connections
                    .register_inbound(&endpoint_id, conn.clone())
                    .await;
                registered = true;
            }

            let unpaired_now = self
                .handle_paired_control_message(&remote, &keying, msg, peer_is_paired)
                .await;
            if unpaired_now {
                // Close so the sender's delivery wait resolves promptly.
                conn.close(0u32.into(), b"unpaired");
                break;
            }
        }

        if registered {
            self.ctx
                .paired_connections
                .unregister_inbound(&endpoint_id, &conn)
                .await;
        }

        Ok(())
    }

    /// Returns true when the peer unpaired us and the session should close.
    async fn handle_paired_control_message(
        &self,
        remote: &EndpointId,
        keying: &[u8],
        msg: ControlMessage,
        peer_is_paired: bool,
    ) -> bool {
        match msg {
            ControlMessage::Invite {
                blob_ticket,
                file_count,
                total_size,
                sender_name,
            } => {

                crate::pairing_util::emit_paired_invite_received(
                    &self.ctx.app_handle,
                    &remote.to_string(),
                    &blob_ticket,
                    file_count,
                    total_size,
                    &sender_name,
                );
            }
            ControlMessage::PairRequest {
                sender_name,
                device_type,
                os,
            } => {
                crate::pairing_util::emit_nearby_pair_request(
                    &self.ctx.app_handle,
                    &remote.to_string(),
                    &sender_name,
                    &device_type,
                    &os,
                );
            }
            ControlMessage::InviteResponse { response, .. } => {
                // An unpaired peer could claim to be answering an invite we
                // never sent, so match it against `pending_nearby_invites`.
                // A decline creates no `PairedDevice`, so the pending snapshot
                // is the only name the response event can carry.
                let mut invited_name = None;
                if !peer_is_paired {
                    let pending = self
                        .ctx
                        .pending_nearby_invites
                        .write()
                        .await
                        .remove(&remote.to_string());
                    let Some(pending) = pending else {
                        tracing::debug!(
                            "ignoring InviteResponse from unpaired {remote} with no outstanding nearby invite"
                        );
                        return false;
                    };
                    invited_name = Some(pending.display_name.clone());
                    // Mirrors `accept_nearby_invite` on the receiver's side;
                    // without it their presence connection back to us is
                    // rejected. A decline stays toast-only.
                    if matches!(response, InviteResponse::Accepted) {
                        self.commit_nearby_pairing(remote, pending).await;
                    }
                }

                let response_str = match response {
                    InviteResponse::Accepted => "accepted",
                    InviteResponse::Declined => "declined",
                };

                crate::pairing_util::emit_paired_invite_response(
                    &self.ctx.app_handle,
                    &self.ctx.paired_store,
                    &remote.to_string(),
                    response_str,
                    invited_name,
                );
            }
            ControlMessage::Recognition { signature } => {

                if verify_challenge(remote, keying, &signature) {

                    let now = protocol::identity::unix_now_ms();
                    let _ = self
                        .ctx
                        .paired_store
                        .touch(&remote.to_string(), now);
                    set_presence(
                        &self.ctx.presence,
                        &self.ctx.app_handle,
                        &self.ctx.paired_store,
                        &remote.to_string(),
                        true
                    );
                }
            }
            ControlMessage::Forget { signature } => {

                if verify_challenge(remote, keying, &signature) {

                    let marked = self
                        .ctx
                        .paired_store
                        .mark_unpaired_remotely(&remote.to_string());
                    if let Ok(Some(device)) = marked {
                        {
                            let mut access = self.ctx.access.write().await;
                            access.allowed.remove(remote);
                        }
                        set_presence(
                            &self.ctx.presence,
                            &self.ctx.app_handle,
                            &self.ctx.paired_store,
                            &remote.to_string(),
                            false
                        );
                        self.ctx.paired_connections.forget(&remote.to_string()).await;
                        let payload = serde_json::json!({
                            "endpoint_id": device.endpoint_id,
                            "display_name": device.display_name,
                            "reason": "remote",
                        });
                        if let Some(handle) = &self.ctx.app_handle {

                            let _ = handle.emit_event_with_payload(
                                "device-unpaired",
                                &payload.to_string(),
                            );
                        }
                    }
                    return true;
                }
            }
            _other => {

            }
        }
        false
    }

    /// Sender's half of mutual nearby pairing — the same record, allowlist
    /// entry, refresh and `device-paired` event `accept_nearby_invite` gives
    /// the receiver. Without it pairing stays one-sided and the peer's
    /// presence connection back to us is rejected.
    async fn commit_nearby_pairing(&self, remote: &EndpointId, pending: PendingNearbyInvite) {
        let endpoint_id = remote.to_string();
        tracing::debug!(
            target: "dashbeam::_events::pairing::nearby_commit",
            remote = %remote.fmt_short(),
        );

        // `pending` was snapshotted at click time and may hold an endpoint-id
        // prefix; prefer whatever Nearby knows now.
        let identified = {
            let nearby = self.ctx.nearby.lock().await;
            nearby
                .list()
                .into_iter()
                .find(|d| d.identified && d.endpoint_id.eq_ignore_ascii_case(&endpoint_id))
        };
        let (display_name, device_type, os) = match identified {
            Some(fresh) => (
                fresh.display_name.unwrap_or(pending.display_name),
                fresh.device_type.unwrap_or(pending.device_type),
                fresh.os.unwrap_or(pending.os),
            ),
            None => (pending.display_name, pending.device_type, pending.os),
        };

        let now = protocol::identity::unix_now_ms();
        if let Err(err) = self.ctx.paired_store.remember(PairedDevice {
            endpoint_id: endpoint_id.clone(),
            display_name,
            device_type,
            os,
            paired_at: now,
            last_seen_at: now,
            relay_url: None,
            pairing_status: PairingStatus::default(),
            trusted: false,
        }) {
            tracing::debug!(
                "failed to remember nearby peer {remote} after mutual accept: {err:#}"
            );
            return;
        }

        // A paired record must not also show under Nearby.
        self.ctx.nearby.lock().await.expire(&endpoint_id);
        self.ctx.access.write().await.allowed.insert(*remote);
        self.ctx.paired_connections.refresh().await;

        let display_name = self
            .ctx
            .paired_store
            .get(&endpoint_id)
            .ok()
            .flatten()
            .map(|d| d.display_name)
            .unwrap_or_default();
        crate::pairing_util::emit_device_paired(&self.ctx.app_handle, &display_name);
    }

    async fn is_allowed(&self, remote: &EndpointId) -> bool {
        self.ctx.access.read().await.allowed.contains(remote)
    }

    async fn allow_peer(&self, remote: EndpointId) {
        let mut access = self.ctx.access.write().await;
        access.allowed.insert(remote);
    }
}

impl ProtocolHandler for ControlProtocol {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let this = self.clone();

        tokio::spawn(async move {
            let _ = this.handle_connection(connection).await;
        });

        Ok(())
    }
}

pub struct NodeService {
    pub(crate) runtime: Arc<Mutex<NodeRuntime>>,
    pub(crate) identity: Arc<DeviceIdentity>,
    pub(crate) paired_store: Arc<PairedDeviceStore>,
    pub(crate) access: Arc<RwLock<AccessState>>,
    pub(crate) pairing_host_open: Arc<AtomicBool>,
    pub(crate) pairing_host_persistent: Arc<AtomicBool>,
    /// True after the endpoint has reached its home relay (or relay is disabled).
    network_ready: Arc<AtomicBool>,
    pub(crate) pairing_expire_task: Mutex<Option<JoinHandle<()>>>,
    pub(crate) paired_connections: Arc<PairedConnectionManager>,
    connections_supervisor: Mutex<Option<JoinHandle<()>>>,
    presence: Arc<std::sync::RwLock<HashMap<String, bool>>>,
    pub(crate) app_handle: AppHandle,
    relay_mode: Mutex<RelayMode>,
    discovery_mode: Mutex<DiscoveryModeOption>,
    /// Devices seen on the local network but not yet paired. Fed by
    /// `lan_discovery`'s mDNS pump; see `spawn_lan_event_loop`.
    nearby: Arc<Mutex<NearbyRegistry>>,
    /// `None` when discovery isn't running: `Discoverability::Off`, or the
    /// mDNS pump failed to start (no multicast, VPN, isolated guest network).
    lan_discovery: Mutex<Option<LanDiscoveryHandle>>,
    /// Why LAN discovery failed to start, or `None` when running or off. Kept
    /// queryable because `nearby-unavailable` can fire before the UI listens.
    nearby_unavailable: Arc<std::sync::RwLock<Option<String>>>,
    /// See `ControlCtx::unpaired_limiter`. Owned here so rebuilds reuse it.
    unpaired_limiter: Arc<std::sync::Mutex<UnpairedRateLimiter>>,
    /// Peers declined-and-blocked from a nearby invite. Checked in
    /// `PairedOnlyHook::after_handshake`, so they're rejected at the handshake.
    blocked: Arc<RwLock<HashSet<String>>>,
    /// Same map as `ControlCtx::pending_nearby_invites`, shared with it.
    pending_nearby_invites: Arc<RwLock<HashMap<String, PendingNearbyInvite>>>,
    /// Serializes `reconfigure_network`, `set_discoverability` and `shutdown`.
    /// Held across each one's whole decide-rebuild-settle sequence so the
    /// decision can't go stale before its consequence runs.
    network_transition: Mutex<()>,
}

impl NodeService {
    /// `discoverability` is applied before discovery starts, so a device set
    /// to `Off` never registers the mDNS publisher — not even briefly.
    pub async fn start(
        data_dir: &Path,
        relay_mode: RelayMode,
        discovery_mode: DiscoveryModeOption,
        discoverability: Discoverability,
        app_handle: AppHandle,
    ) -> anyhow::Result<Self> {
        Self::start_with(
            data_dir,
            relay_mode,
            discovery_mode,
            discoverability,
            app_handle,
            IdentityOptions::default(),
        )
        .await
    }

    pub async fn start_with(
        data_dir: &Path,
        relay_mode: RelayMode,
        discovery_mode: DiscoveryModeOption,
        discoverability: Discoverability,
        app_handle: AppHandle,
        identity_options: IdentityOptions,
    ) -> anyhow::Result<Self> {
        let identity = Arc::new(load_or_create_identity_with(data_dir, &identity_options)?);
        let paired_store = Arc::new(PairedDeviceStore::new(data_dir));
        let allowed = load_allowed_from_store(&paired_store)?;

        if identity.identity_rotated {
            let stale_count = paired_store
                .mark_stale_after_local_identity_rotation()
                .unwrap_or(0);

            if stale_count > 0 {
                if let Some(handle) = &app_handle {
                    let payload = serde_json::json!({
                        "previous_endpoint_id": identity.previous_endpoint_id,
                        "current_endpoint_id": identity.endpoint_id(),
                        "stale_device_count": stale_count,
                    });

                    let _ = handle.emit_event_with_payload(
                        "identity-rotated",
                        &payload.to_string(),
                    );
                }
            }
        }

        let access = Arc::new(RwLock::new(AccessState {
            allowed: allowed.clone(),
            authorized: HashSet::new(),
            pairing_host_open: false,
            discoverability,
        }));
        let pairing_host_open = Arc::new(AtomicBool::new(false));
        let pairing_host_persistent = Arc::new(AtomicBool::new(false));
        let network_ready = Arc::new(AtomicBool::new(false));
        let presence = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let blocked: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));
        let pending_nearby_invites: Arc<RwLock<HashMap<String, PendingNearbyInvite>>> =
            Arc::new(RwLock::new(HashMap::new()));
        // Created before `build_runtime` because `ControlCtx` needs it too.
        let nearby = Arc::new(Mutex::new(NearbyRegistry::new()));
        let nearby_unavailable: Arc<std::sync::RwLock<Option<String>>> =
            Arc::new(std::sync::RwLock::new(None));
        let unpaired_limiter = Arc::new(std::sync::Mutex::new(UnpairedRateLimiter::new()));

        let paired_connections = Arc::new(PairedConnectionManager::new(
            identity.clone(),
            paired_store.clone(),
            access.clone(),
            presence.clone(),
            app_handle.clone(),
        ));

        let runtime = build_runtime(
            identity.clone(),
            paired_store.clone(),
            access.clone(),
            blocked.clone(),
            pending_nearby_invites.clone(),
            nearby.clone(),
            unpaired_limiter.clone(),
            pairing_host_persistent.clone(),
            app_handle.clone(),
            presence.clone(),
            paired_connections.clone(),
            network_ready.clone(),
            relay_mode.clone(),
            discovery_mode.clone(),
        )
        .await?;
        let runtime = Arc::new(Mutex::new(runtime));
        paired_connections.attach_runtime(runtime.clone());
        let connections_supervisor = paired_connections.start();

        let lan_discovery = if should_publish_mdns(access.read().await.discoverability) {
            let endpoint = {
                let runtime = runtime.lock().await;
                runtime.endpoint.clone()
            };
            start_lan_discovery(
                &endpoint,
                nearby.clone(),
                access.clone(),
                paired_connections.clone(),
                runtime.clone(),
                app_handle.clone(),
                &nearby_unavailable,
            )
        } else {
            None
        };

        Ok(Self {
            runtime,
            identity,
            paired_store,
            access,
            pairing_host_open,
            pairing_host_persistent,
            network_ready,
            pairing_expire_task: Mutex::new(None),
            paired_connections,
            connections_supervisor: Mutex::new(Some(connections_supervisor)),
            presence,
            app_handle,
            relay_mode: Mutex::new(relay_mode),
            discovery_mode: Mutex::new(discovery_mode),
            nearby,
            lan_discovery: Mutex::new(lan_discovery),
            nearby_unavailable,
            unpaired_limiter,
            network_transition: Mutex::new(()),
            blocked,
            pending_nearby_invites,
        })
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        // Same lock the network transitions hold, so a shutdown can't race an
        // in-flight rebuild in either direction.
        let _guard = self.network_transition.lock().await;

        self.stop_pairing_host().await;
        self.stop_lan_discovery().await;
        if let Some(handle) = self.connections_supervisor.lock().await.take() {
            handle.abort();

        }
        self.paired_connections.shutdown().await;
        let runtime = self.runtime.lock().await;
        runtime.router.shutdown().await?;
        runtime.endpoint.close().await;

        Ok(())
    }

    pub async fn reconfigure_network(
        &self,
        relay_mode: RelayMode,
        discovery_mode: DiscoveryModeOption,
    ) -> anyhow::Result<()> {
        let _guard = self.network_transition.lock().await;

        {
            let current_relay = self.relay_mode.lock().await;
            let current_discovery = self.discovery_mode.lock().await;
            if format!("{current_relay:?}") == format!("{relay_mode:?}")
                && format!("{current_discovery:?}") == format!("{discovery_mode:?}")
            {
                return Ok(());
            }
        }

        self.rebuild_network(relay_mode, discovery_mode).await
    }

    /// Rebuilds the endpoint and router, then restarts LAN discovery per the
    /// current `Discoverability`. A full rebuild is the only way to stop mDNS
    /// advertising: iroh 1.0.3 can't unregister an address-lookup service from
    /// a live endpoint, and toggling would leak an actor per switch.
    ///
    /// Callers must already hold `self.network_transition`; this doesn't
    /// acquire it.
    async fn rebuild_network(
        &self,
        relay_mode: RelayMode,
        discovery_mode: DiscoveryModeOption,
    ) -> anyhow::Result<()> {
        self.stop_pairing_host().await;

        self.network_ready.store(false, Ordering::SeqCst);
        if let Some(handle) = &self.app_handle {
            let _ = handle.emit_event("device-node-network-warming");
        }

        // Stop the old consumer + pump before their endpoint is closed below,
        // so the mDNS actor and its multicast socket die instead of leaking.
        self.stop_lan_discovery().await;

        let mut runtime = self.runtime.lock().await;
        runtime.router.shutdown().await?;
        runtime.endpoint.close().await;

        let new_runtime = build_runtime(
            self.identity.clone(),
            self.paired_store.clone(),
            self.access.clone(),
            self.blocked.clone(),
            self.pending_nearby_invites.clone(),
            self.nearby.clone(),
            self.unpaired_limiter.clone(),
            self.pairing_host_persistent.clone(),
            self.app_handle.clone(),
            self.presence.clone(),
            self.paired_connections.clone(),
            self.network_ready.clone(),
            relay_mode.clone(),
            discovery_mode.clone(),
        )
        .await?;

        let endpoint = new_runtime.endpoint.clone();
        *runtime = new_runtime;
        drop(runtime);

        self.paired_connections.refresh().await;
        *self.relay_mode.lock().await = relay_mode;
        *self.discovery_mode.lock().await = discovery_mode;

        if should_publish_mdns(self.access.read().await.discoverability) {
            let discovery = start_lan_discovery(
                &endpoint,
                self.nearby.clone(),
                self.access.clone(),
                self.paired_connections.clone(),
                self.runtime.clone(),
                self.app_handle.clone(),
                &self.nearby_unavailable,
            );
            *self.lan_discovery.lock().await = discovery;
        } else {
            // Off is a choice, not a failure: clear any stale reason.
            *self.nearby_unavailable.write().expect("nearby_unavailable") = None;
        }

        Ok(())
    }

    /// Stops the LAN-discovery consumer task and its mDNS pump, if running.
    async fn stop_lan_discovery(&self) {
        if let Some(handle) = self.lan_discovery.lock().await.take() {
            handle.shutdown();
        }
    }

    pub fn is_network_ready(&self) -> bool {
        self.network_ready.load(Ordering::SeqCst)
    }

    pub fn device_info(&self) -> DeviceInfo {
        DeviceInfo::from(self.identity.as_ref())
    }

    pub fn set_device_display_name(&self, display_name: &str) -> anyhow::Result<DeviceInfo> {
        let info = self.identity.set_display_name(display_name)?;

        Ok(info)
    }

    pub fn rename_paired(
        &self,
        endpoint_id: &str,
        display_name: &str,
    ) -> anyhow::Result<PairedDevice> {
        let device = self.paired_store.rename(endpoint_id, display_name)?;

        Ok(device)
    }

    pub fn trust_paired(
        &self,
        endpoint_id: &str,
        trust: bool,
    ) -> anyhow::Result<PairedDeviceInfo> {
        let device = self.paired_store.trust_paired_device(endpoint_id, trust)?;
        let online = self
            .presence
            .read()
            .expect("presence lock")
            .get(&device.endpoint_id.to_lowercase())
            .copied()
            .unwrap_or(false);

        Ok(PairedDeviceInfo::from_device(device, online))
    }

    pub fn list_paired(&self) -> anyhow::Result<Vec<PairedDeviceInfo>> {
        let devices = self.paired_store.list()?;
        let presence = self.presence.read().expect("presence lock");
        let infos: Vec<PairedDeviceInfo> = devices
            .into_iter()
            .map(|device| {
                let online = presence
                    .get(&device.endpoint_id.to_lowercase())
                    .or_else(|| presence.get(&device.endpoint_id))
                    .copied()
                    .unwrap_or(false);
                PairedDeviceInfo::from_device(device, online)
            })
            .collect();

        Ok(infos)
    }

    /// `(online, total)` across actively-paired devices. Used by the desktop
    /// tray, which has no access to the presence map directly.
    pub fn presence_summary(&self) -> anyhow::Result<(usize, usize)> {
        Ok(crate::device_identity::presence_summary(&self.list_paired()?))
    }

    pub async fn forget_paired(&self, endpoint_id: &str) -> anyhow::Result<()> {
        debug!(
            target: "dashbeam::_events::pairing::forget",
            remote = %endpoint_id,
        );

        let stored_relay = self
            .paired_store
            .get(endpoint_id)?
            .and_then(|d| d.relay_url);

        // Prefer the live presence link so Forget arrives before teardown; the
        // redial fallback is best-effort and must not block the Devices UI.
        let delivered = if let Some(conn) = self.paired_connections.live_session(endpoint_id).await
        {
            send_forget_on_connection(&self.identity, &conn)
                .await
                .is_ok()
        } else {
            false
        };
        if !delivered {
            let runtime = self.runtime.clone();
            let identity = self.identity.clone();
            let notify_id = endpoint_id.to_string();
            tokio::spawn(async move {
                let _ =
                    send_forget_to_peer(&runtime, &identity, &notify_id, stored_relay.as_deref())
                        .await;
            });
        }

        if let Ok(id) = EndpointId::from_str(endpoint_id) {
            self.access.write().await.allowed.remove(&id);

        }
        self.paired_store.forget(endpoint_id)?;
        self.paired_connections.forget(endpoint_id).await;
        set_presence(
            &self.presence,
            &self.app_handle,
            &self.paired_store,
            endpoint_id,
            false
        );
        if let Some(handle) = &self.app_handle {
            let payload = serde_json::json!({
                "endpoint_id": endpoint_id,
                "reason": "local",
            });
            let _ = handle.emit_event_with_payload("device-unpaired", &payload.to_string());
        }

        Ok(())
    }

    /// Immediate pairing code from local identity. Never waits on presence or
    /// relay probes — optional custom-relay hint is best-effort via try_lock.
    /// Replace the set of peers allowed to pair without an open pairing host.
    /// Rejects the whole update if any id is invalid.
    pub async fn set_authorized_peers(&self, ids: Vec<String>) -> anyhow::Result<()> {
        let set = ids
            .iter()
            .map(|id| EndpointId::from_str(id).with_context(|| format!("invalid endpoint id: {id}")))
            .collect::<anyhow::Result<HashSet<_>>>()?;
        self.access.write().await.authorized = set;
        Ok(())
    }

    pub fn pairing_ticket(&self) -> anyhow::Result<String> {
        let relay_url = match self.runtime.try_lock() {
            Ok(runtime) => {
                let mut addr = runtime.endpoint.addr();
                apply_options(&mut addr, AddrInfoOptions::Relay);
                let url = addr.relay_urls().next().map(|u| u.to_string());
                url
            }
            Err(_) => None,
        };
        let ticket = protocol::PairingTicket {
            v: 1,
            kind: protocol::PairingTicket::KIND.to_string(),
            endpoint_id: self.identity.endpoint_id(),
            relay_url,
        };
        ticket.encode()
    }

    pub async fn invite_paired_device(
        &self,
        remote_endpoint_id: &str,
        blob_ticket: &str,
        file_count: u32,
        total_size: u64,
    ) -> anyhow::Result<bool> {
        let remote = EndpointId::from_str(remote_endpoint_id)?;
        debug!(
            target: "dashbeam::_events::pairing::invite_sent",
            remote = %remote.fmt_short(),
            file_count,
            total_size,
        );
        let access = self.access.read().await;
        let in_allowlist = access.allowed.contains(&remote);
        drop(access);

        if !in_allowlist {

            anyhow::bail!("unknown paired device");
        }

        self.deliver_invite(remote_endpoint_id, blob_ticket, file_count, total_size, true)
            .await
    }

    /// Sends an already-minted share ticket to a nearby device — the same wire
    /// `Invite` a paired device gets, but the receiver's UI shows the sender's
    /// fingerprint for confirmation since there's no `PairedDevice` record yet.
    pub async fn invite_nearby_device(
        &self,
        endpoint_id: &str,
        blob_ticket: &str,
        file_count: u32,
        total_size: u64,
    ) -> anyhow::Result<bool> {
        let nearby_device = self
            .nearby
            .lock()
            .await
            .list()
            .into_iter()
            .find(|d| d.endpoint_id == endpoint_id);
        anyhow::ensure!(nearby_device.is_some(), "device is not on the local network");
        anyhow::ensure!(!blob_ticket.is_empty(), "no blob ticket provided");

        // No cached session is possible for a device we've never talked to.
        let delivered = self
            .deliver_invite(endpoint_id, blob_ticket, file_count, total_size, false)
            .await?;
        if !delivered {
            return Ok(false);
        }

        // Track the invite so the peer's `InviteResponse` is recognized as a
        // real answer and carries enough identity for mutual pairing.
        // Snapshotted now — the Nearby entry may have expired by then.
        let pending = match nearby_device.filter(|d| d.identified) {
            Some(d) => PendingNearbyInvite {
                display_name: d
                    .display_name
                    .unwrap_or_else(|| endpoint_id.chars().take(8).collect()),
                device_type: d
                    .device_type
                    .unwrap_or_else(protocol::identity::default_device_type),
                os: d.os.unwrap_or_default(),
            },
            None => PendingNearbyInvite {
                display_name: endpoint_id.chars().take(8).collect(),
                device_type: protocol::identity::default_device_type(),
                os: String::new(),
            },
        };
        self.pending_nearby_invites
            .write()
            .await
            .insert(endpoint_id.to_string(), pending);

        Ok(true)
    }

    /// Asks a Nearby (unpaired LAN) device to pair — no file ticket.
    /// Receiver confirms on name/device type; accept reuses
    /// [`Self::accept_nearby_invite`] + `InviteResponse` mutual pairing.
    pub async fn request_nearby_pair(&self, endpoint_id: &str) -> anyhow::Result<bool> {
        let nearby_device = self
            .nearby
            .lock()
            .await
            .list()
            .into_iter()
            .find(|d| d.endpoint_id == endpoint_id);
        anyhow::ensure!(nearby_device.is_some(), "device is not on the local network");

        let delivered = self.deliver_pair_request(endpoint_id).await?;
        if !delivered {
            return Ok(false);
        }

        let pending = match nearby_device.filter(|d| d.identified) {
            Some(d) => PendingNearbyInvite {
                display_name: d
                    .display_name
                    .unwrap_or_else(|| endpoint_id.chars().take(8).collect()),
                device_type: d
                    .device_type
                    .unwrap_or_else(protocol::identity::default_device_type),
                os: d.os.unwrap_or_default(),
            },
            None => PendingNearbyInvite {
                display_name: endpoint_id.chars().take(8).collect(),
                device_type: protocol::identity::default_device_type(),
                os: String::new(),
            },
        };
        self.pending_nearby_invites
            .write()
            .await
            .insert(endpoint_id.to_string(), pending);

        Ok(true)
    }

    /// Delivers a single `Invite` to `remote_endpoint_id`. Shared by the paired
    /// and nearby invite paths — only their preconditions differ.
    /// `use_cached_session` is false for a peer we've never talked to.
    async fn deliver_invite(
        &self,
        remote_endpoint_id: &str,
        blob_ticket: &str,
        file_count: u32,
        total_size: u64,
        use_cached_session: bool,
    ) -> anyhow::Result<bool> {
        let remote = EndpointId::from_str(remote_endpoint_id)?;
        let stored_relay = self
            .paired_store
            .get(remote_endpoint_id)?
            .and_then(|d| d.relay_url);

        let cached = if use_cached_session {
            self.paired_connections
                .wait_for_connection(remote_endpoint_id, invite_wait_timeout())
                .await
        } else {
            None
        };

        let conn = match cached {
            Some(conn) => conn,
            None => {
                let endpoint = {
                    let runtime = self.runtime.lock().await;
                    runtime.endpoint.clone()
                };
                let addr = build_control_connect_addr(
                    &endpoint,
                    remote,
                    stored_relay.as_deref(),
                );

                let connect = tokio::time::timeout(
                    Duration::from_secs(PRESENCE_CONNECT_TIMEOUT_SECS),
                    endpoint.connect(addr, CONTROL_ALPN),
                )
                .await;
                match connect {
                    Ok(Ok(conn)) => {
                        if use_cached_session {
                            let now = protocol::identity::unix_now_ms();
                            let _ = self.paired_store.touch(remote_endpoint_id, now);
                            set_presence(
                                &self.presence,
                                &self.app_handle,
                                &self.paired_store,
                                remote_endpoint_id,
                                true,
                            );
                        }
                        conn
                    }
                    Ok(Err(_err)) => {
                        return Ok(false);
                    }
                    Err(_) => {
                        return Ok(false);
                    }
                }
            }
        };

        let (mut send, _recv) = match conn.open_bi().await {
            Ok(streams) => streams,
            Err(err) => {
                return Err(err).context("open bi stream for invite");
            }
        };

        let invite = ControlMessage::Invite {
            blob_ticket: blob_ticket.to_string(),
            file_count,
            total_size,
            sender_name: self.identity.display_name(),
        };
        if let Err(err) = write_message(&mut send, &invite).await {

            return Err(err).context("write Invite message");
        }

        // Hold the connection open in the background so the receiver can read
        // the invite, without blocking the caller.
        drop(send);
        tokio::spawn(async move {

            match tokio::time::timeout(Duration::from_secs(15), conn.closed()).await {
                Ok(_closed) => {},
                // Expected while the receiver downloads; the invite landed.
                Err(_) => {},
            }
        });

        Ok(true)
    }

    async fn deliver_pair_request(&self, remote_endpoint_id: &str) -> anyhow::Result<bool> {
        let remote = EndpointId::from_str(remote_endpoint_id)?;
        let endpoint = {
            let runtime = self.runtime.lock().await;
            runtime.endpoint.clone()
        };
        let addr = build_control_connect_addr(&endpoint, remote, None);

        let connect = tokio::time::timeout(
            Duration::from_secs(PRESENCE_CONNECT_TIMEOUT_SECS),
            endpoint.connect(addr, CONTROL_ALPN),
        )
        .await;
        let conn = match connect {
            Ok(Ok(conn)) => conn,
            Ok(Err(_err)) => return Ok(false),
            Err(_) => return Ok(false),
        };

        let (mut send, _recv) = match conn.open_bi().await {
            Ok(streams) => streams,
            Err(err) => {
                return Err(err).context("open bi stream for pair request");
            }
        };

        let request = ControlMessage::PairRequest {
            sender_name: self.identity.display_name(),
            device_type: self.identity.device_type(),
            os: self.identity.os(),
        };
        if let Err(err) = write_message(&mut send, &request).await {
            return Err(err).context("write PairRequest message");
        }

        drop(send);
        tokio::spawn(async move {
            match tokio::time::timeout(Duration::from_secs(15), conn.closed()).await {
                Ok(_closed) => {}
                Err(_) => {}
            }
        });

        Ok(true)
    }

    pub async fn respond_paired_invite(
        &self,
        remote_endpoint_id: &str,
        accepted: bool,
    ) -> anyhow::Result<()> {
        let remote = EndpointId::from_str(remote_endpoint_id)?;
        debug!(
            target: "dashbeam::_events::pairing::invite_response",
            remote = %remote.fmt_short(),
            accepted,
        );
        let access = self.access.read().await;
        let in_allowlist = access.allowed.contains(&remote);
        drop(access);
        if !in_allowlist {

            anyhow::bail!("unknown paired device");
        }

        self.deliver_invite_response(remote_endpoint_id, accepted, true)
            .await
    }

    /// Records the peer as paired — the durable consequence of the user having
    /// compared the fingerprint on screen — then notifies the sender via
    /// [`Self::deliver_invite_response`].
    pub async fn accept_nearby_invite(&self, endpoint_id: &str) -> anyhow::Result<()> {
        // The probe reply's `endpoint_id` is self-reported; only trust its
        // cosmetic fields, and only if the id matches the verified one.
        let info = nearby_peer_identity(endpoint_id, self.probe_identity(endpoint_id).await.ok());

        let now = protocol::identity::unix_now_ms();
        let display_name = info.display_name.clone();
        self.paired_store.remember(PairedDevice {
            endpoint_id: info.endpoint_id.clone(),
            display_name: info.display_name,
            device_type: info.device_type,
            os: info.os,
            paired_at: now,
            last_seen_at: now,
            relay_url: None,
            pairing_status: PairingStatus::default(),
            trusted: false,
        })?;

        // Now that a paired record exists, it must not also show under Nearby.
        self.nearby.lock().await.expire(endpoint_id);
        self.access.write().await.allowed.insert(endpoint_id.parse()?);

        crate::pairing_util::emit_device_paired(&self.app_handle, &display_name);

        // The pairing above is already durable, so notifying a sender that may
        // have gone away must not report the accept as failed. No cached
        // session either — we just met this peer.
        //
        // Must precede `paired_connections.refresh()`, whose `Recognition` an
        // uncommitted sender closes as unpaired, reading as a remote unpair.
        if let Err(err) = self.deliver_invite_response(endpoint_id, true, false).await {
            tracing::debug!(
                "accepted nearby invite from {endpoint_id} but could not notify the sender: {err:#}"
            );
        }

        tracing::debug!(
            target: "dashbeam::_events::pairing::nearby_accept",
            remote = %short_remote(endpoint_id),
        );
        self.paired_connections.refresh().await;

        Ok(())
    }

    pub async fn decline_nearby_invite(
        &self,
        endpoint_id: &str,
        block: bool,
    ) -> anyhow::Result<()> {
        if block {
            self.nearby.lock().await.expire(endpoint_id);
            self.blocked.write().await.insert(endpoint_id.to_string());
        }
        self.deliver_invite_response(endpoint_id, false, false)
            .await
    }

    /// Connects to `remote_endpoint_id` and delivers an `InviteResponse`.
    /// Shared by [`Self::respond_paired_invite`], [`Self::accept_nearby_invite`],
    /// and [`Self::decline_nearby_invite`] — see [`Self::deliver_invite`] for
    /// why `use_cached_session` exists.
    async fn deliver_invite_response(
        &self,
        remote_endpoint_id: &str,
        accepted: bool,
        use_cached_session: bool,
    ) -> anyhow::Result<()> {
        let remote = EndpointId::from_str(remote_endpoint_id)?;
        let response = if accepted {
            InviteResponse::Accepted
        } else {
            InviteResponse::Declined
        };

        let stored_relay = self
            .paired_store
            .get(remote_endpoint_id)?
            .and_then(|d| d.relay_url);

        let cached = if use_cached_session {
            self.paired_connections
                .wait_for_connection(remote_endpoint_id, invite_wait_timeout())
                .await
        } else {
            None
        };

        let conn = match cached {
            Some(conn) => conn,
            None => {
                let endpoint = {
                    let runtime = self.runtime.lock().await;
                    runtime.endpoint.clone()
                };
                let addr = build_control_connect_addr(
                    &endpoint,
                    remote,
                    stored_relay.as_deref(),
                );
                let connect = tokio::time::timeout(
                    Duration::from_secs(PRESENCE_CONNECT_TIMEOUT_SECS),
                    endpoint.connect(addr, CONTROL_ALPN),
                )
                .await;
                match connect {
                    Ok(Ok(conn)) => conn,
                    Ok(Err(err)) => {
                        return Err(err).context("invite response connect failed");
                    }
                    Err(_) => anyhow::bail!("invite response connect timeout"),
                }
            }
        };

        let (mut send, _recv) = conn
            .open_bi()
            .await
            .context("open bi stream for invite response")?;
        let message = ControlMessage::InviteResponse {
            session_id: String::new(),
            response,
        };
        write_message(&mut send, &message)
            .await
            .context("write InviteResponse message")?;

        // On a freshly dialed connection `conn` is the only handle, and iroh
        // closes on last-handle drop — returning now would race the in-flight
        // write against the receiver's `accept_bi()`. Hold it in the background.
        drop(send);
        tokio::spawn(async move {
            let _ = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await;
        });

        Ok(())
    }

    /// Rebuilds the network only when the transition crosses the `Off`
    /// boundary — `Everyone` <-> `PairedOnly` both publish over mDNS and
    /// differ only in who gets an identity reply. A rebuild is required
    /// because iroh 1.0.3 can't unregister a live address-lookup service; see
    /// `rebuild_network`.
    ///
    /// The whole decide-rebuild-clear sequence runs under
    /// `network_transition` so a competing transition can't invalidate
    /// `now_off` before the clear runs.
    pub async fn set_discoverability(&self, setting: Discoverability) -> anyhow::Result<()> {
        let _guard = self.network_transition.lock().await;

        tracing::debug!(
            target: "dashbeam::_events::nearby::discoverability",
            requested = ?setting,
        );

        let previous = {
            let mut access = self.access.write().await;
            let previous = access.discoverability;
            access.discoverability = setting;
            previous
        };

        let now_off = matches!(setting, Discoverability::Off);
        if now_off == matches!(previous, Discoverability::Off) {
            return Ok(());
        }

        let (relay_mode, discovery_mode) = {
            (
                self.relay_mode.lock().await.clone(),
                self.discovery_mode.lock().await.clone(),
            )
        };
        if let Err(err) = self.rebuild_network(relay_mode, discovery_mode).await {
            // The node is effectively dead: restore the previous setting so
            // `discoverability()` reflects that the transition did not land.
            self.access.write().await.discoverability = previous;
            return Err(err.context("rebuild network for discoverability change"));
        }

        if now_off {
            // Still under `network_transition`, so `now_off` is still accurate.
            self.nearby.lock().await.clear();
        }

        Ok(())
    }

    pub async fn discoverability(&self) -> Discoverability {
        self.access.read().await.discoverability
    }

    /// Devices currently seen on the local network but not yet paired.
    pub async fn list_nearby(&self) -> Vec<NearbyDevice> {
        self.nearby.lock().await.list()
    }

    /// Why LAN discovery is unavailable, or `None` when running or `Off`. Lets
    /// the UI recover a reason whose event fired before it was listening.
    pub fn nearby_unavailable_reason(&self) -> Option<String> {
        self.nearby_unavailable
            .read()
            .expect("nearby_unavailable")
            .clone()
    }

    /// Test-only: seeds the Nearby registry as `spawn_lan_event_loop` does on
    /// `LanEvent::Appeared`, minus the socket — CI runners block multicast.
    #[doc(hidden)]
    pub async fn inject_nearby_device_for_tests(&self, endpoint_id: &str) {
        self.nearby
            .lock()
            .await
            .observe(endpoint_id, false, protocol::identity::unix_now_ms());
    }

    /// Test-only: injects a nearby peer *and* runs the identity probe that
    /// mDNS discovery normally triggers, so the device lands in the registry
    /// identified — the state a user actually sees before clicking Pair.
    #[doc(hidden)]
    pub async fn inject_identified_nearby_device_for_tests(
        &self,
        endpoint_id: &str,
    ) -> anyhow::Result<()> {
        self.nearby
            .lock()
            .await
            .observe(endpoint_id, false, protocol::identity::unix_now_ms());
        let info = probe_identity_via(&self.runtime, endpoint_id).await?;
        let os = if info.os.is_empty() {
            None
        } else {
            Some(info.os)
        };
        self.nearby.lock().await.set_identity(
            endpoint_id,
            info.display_name,
            info.device_type,
            os,
        );
        Ok(())
    }

    /// Test-only: the `ObserveOutcome::Paired` branch of
    /// `spawn_lan_event_loop` — rediscovery must not tear down a live session.
    #[doc(hidden)]
    pub async fn simulate_paired_lan_appeared_for_tests(&self, endpoint_id: &str) {
        self.paired_connections.nudge_reconnect(endpoint_id).await;
    }

    /// Test-only: the pre-fix `nudge_reconnect` behaviour — abort and re-dial
    /// even with a live session, to prove the other side stays online.
    #[doc(hidden)]
    pub async fn force_paired_reconnect_for_tests(&self, endpoint_id: &str) {
        self.paired_connections
            .force_reconnect_for_tests(endpoint_id)
            .await;
    }

    /// Test-only: drop a peer locally without sending `Forget`. Simulates a
    /// missed unpair notify (fire-and-forget dial failed) so the remaining
    /// side can prove it still detects remote unpair from the close reason.
    #[doc(hidden)]
    pub async fn forget_paired_silently_for_tests(&self, endpoint_id: &str) -> anyhow::Result<()> {
        if let Ok(id) = EndpointId::from_str(endpoint_id) {
            self.access.write().await.allowed.remove(&id);
        }
        self.paired_store.forget(endpoint_id)?;
        self.paired_connections.forget(endpoint_id).await;
        set_presence(
            &self.presence,
            &self.app_handle,
            &self.paired_store,
            endpoint_id,
            false,
        );
        Ok(())
    }

    /// Test-only: commit a pairing without a handshake, so a test can place it
    /// at an exact point relative to a control connection.
    #[doc(hidden)]
    pub async fn remember_paired_device_for_tests(&self, endpoint_id: &str) -> anyhow::Result<()> {
        let now = protocol::identity::unix_now_ms();
        self.paired_store.remember(PairedDevice {
            endpoint_id: endpoint_id.to_string(),
            display_name: short_remote(endpoint_id),
            device_type: "laptop".to_string(),
            os: "test".to_string(),
            paired_at: now,
            last_seen_at: now,
            relay_url: None,
            pairing_status: PairingStatus::default(),
            trusted: false,
        })?;
        self.access
            .write()
            .await
            .allowed
            .insert(EndpointId::from_str(endpoint_id)?);
        self.paired_connections.refresh().await;
        Ok(())
    }

    /// Test-only: dial + probe a peer, returning the open connection so a test
    /// controls when the next message lands. The probe reply proves the peer's
    /// session loop already decided whether we count as paired.
    #[doc(hidden)]
    pub async fn open_control_connection_for_tests(
        &self,
        endpoint_id: &str,
    ) -> anyhow::Result<Connection> {
        let remote = EndpointId::from_str(endpoint_id)?;
        let endpoint = {
            let runtime = self.runtime.lock().await;
            runtime.endpoint.clone()
        };
        let addr = build_control_connect_addr(&endpoint, remote, None);
        let conn = tokio::time::timeout(
            Duration::from_secs(PRESENCE_CONNECT_TIMEOUT_SECS),
            endpoint.connect(addr, CONTROL_ALPN),
        )
        .await
        .context("control connect timeout")?
        .context("control connect failed")?;

        let (mut send, mut recv) = conn.open_bi().await.context("open bi for probe")?;
        write_message(&mut send, &ControlMessage::WhoAreYou).await?;
        match tokio::time::timeout(
            Duration::from_secs(PRESENCE_CONNECT_TIMEOUT_SECS),
            read_message(&mut recv),
        )
        .await
        .context("identity reply timed out")??
        {
            ControlMessage::Identity { .. } => Ok(conn),
            other => anyhow::bail!("unexpected reply to WhoAreYou: {other:?}"),
        }
    }

    /// Test-only: send a `Recognition` over an existing control connection —
    /// the second half of `PairedConnectionManager::connect_and_recognize`,
    /// split out so a test can delay it past a pairing commit.
    #[doc(hidden)]
    pub async fn send_recognition_for_tests(&self, conn: &Connection) -> anyhow::Result<()> {
        let keying = export_connection_keying_material(conn).context("export keying")?;
        let (mut send, _recv) = conn.open_bi().await.context("open bi for recognition")?;
        let recognition = ControlMessage::Recognition {
            signature: sign_challenge(&self.identity.secret_key, &keying),
        };
        write_message(&mut send, &recognition)
            .await
            .context("write recognition")
    }

    /// Dial a peer's control ALPN and ask who it is. Used for devices found on
    /// the local network, where mDNS supplies a node id and nothing else.
    pub async fn probe_identity(&self, endpoint_id: &str) -> anyhow::Result<DeviceInfo> {
        probe_identity_via(&self.runtime, endpoint_id).await
    }
}

/// Identity to record when promoting a nearby peer to paired. The
/// TLS-proven `endpoint_id` is always what gets stored; the probe reply only
/// contributes cosmetic fields, and only when its self-reported id matches —
/// otherwise we'd durably allowlist a key the user never verified.
/// `ControlProtocol::commit_nearby_pairing` applies the same rule on the
/// sender's side by construction — it only ever uses the connection's id.
fn nearby_peer_identity(endpoint_id: &str, probed: Option<DeviceInfo>) -> DeviceInfo {
    let fallback = || DeviceInfo {
        endpoint_id: endpoint_id.to_string(),
        display_name: endpoint_id.chars().take(8).collect(),
        device_type: protocol::identity::default_device_type(),
        os: String::new(),
    };
    match probed {
        Some(info) if info.endpoint_id.eq_ignore_ascii_case(endpoint_id) => DeviceInfo {
            endpoint_id: endpoint_id.to_string(),
            ..info
        },
        Some(info) => {
            tracing::warn!(
                "identity probe for {endpoint_id} replied with mismatching endpoint id {}; ignoring the reply",
                info.endpoint_id
            );
            fallback()
        }
        None => fallback(),
    }
}

/// Shortens an id the way `EndpointId::fmt_short` does, so our logs line up
/// with iroh's own `remote_id=` fields. Takes a `&str` for string-only paths.
pub(crate) fn short_remote(endpoint_id: &str) -> String {
    endpoint_id.chars().take(10).collect()
}

fn load_allowed_from_store(paired_store: &PairedDeviceStore) -> anyhow::Result<HashSet<EndpointId>> {
    let mut allowed = HashSet::new();
    for device in paired_store.list()? {
        if !device.pairing_status.is_connectable() {

            continue;
        }
        if let Ok(id) = EndpointId::from_str(&device.endpoint_id) {
            allowed.insert(id);
        }
    }

    Ok(allowed)
}

/// Bundles the mDNS pump with its consumer task so both tear down together.
/// A clean shutdown also needs the endpoint closed — see `rebuild_network`.
struct LanDiscoveryHandle {
    pump: LanDiscovery,
    consumer: JoinHandle<()>,
}

impl LanDiscoveryHandle {
    /// Aborts the consumer, then the pump. `abort()` only requests
    /// cancellation — the task stops at its next await point.
    fn shutdown(self) {
        self.consumer.abort();
        self.pump.shutdown();
    }
}

/// Starts the mDNS pump and its consumer loop, wiring sightings into `nearby`
/// and identity probes into the discovery/identified events.
///
/// Failure is not fatal (no multicast, VPN, isolated guest network) — the app
/// keeps working with pairing codes and relays.
fn start_lan_discovery(
    endpoint: &Endpoint,
    nearby: Arc<Mutex<NearbyRegistry>>,
    access: Arc<RwLock<AccessState>>,
    paired_connections: Arc<PairedConnectionManager>,
    runtime: Arc<Mutex<NodeRuntime>>,
    app_handle: AppHandle,
    unavailable: &Arc<std::sync::RwLock<Option<String>>>,
) -> Option<LanDiscoveryHandle> {
    let (tx, rx) = mpsc::unbounded_channel();
    match LanDiscovery::start(endpoint, tx) {
        Ok(pump) => {
            *unavailable.write().expect("nearby_unavailable") = None;
            let consumer =
                spawn_lan_event_loop(rx, nearby, access, paired_connections, runtime, app_handle);
            Some(LanDiscoveryHandle { pump, consumer })
        }
        Err(err) => {
            tracing::debug!(
                target: "dashbeam::_events::nearby::mdns_unavailable",
                error = %format!("{err:#}"),
            );
            // Recorded as well as emitted, so a frontend that mounts after this
            // fires can still hydrate via `nearby_unavailable_reason`.
            *unavailable.write().expect("nearby_unavailable") = Some(err.to_string());
            emit_nearby_reason(&app_handle, "nearby-unavailable", &err.to_string());
            None
        }
    }
}

/// Turns `LanEvent`s into `NearbyRegistry` updates and UI events. All policy
/// about what a sighting means lives here, not in `lan_discovery`.
fn spawn_lan_event_loop(
    mut rx: mpsc::UnboundedReceiver<LanEvent>,
    nearby: Arc<Mutex<NearbyRegistry>>,
    access: Arc<RwLock<AccessState>>,
    paired_connections: Arc<PairedConnectionManager>,
    runtime: Arc<Mutex<NodeRuntime>>,
    app_handle: AppHandle,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                LanEvent::Appeared { endpoint_id } => {
                    let is_paired = is_paired_endpoint(&access, &endpoint_id).await;
                    let outcome = nearby.lock().await.observe(
                        &endpoint_id,
                        is_paired,
                        protocol::identity::unix_now_ms(),
                    );
                    match outcome {
                        ObserveOutcome::ProbeNeeded => {
                            emit_nearby(&app_handle, "nearby-device-found", &endpoint_id);
                            spawn_identity_probe(
                                endpoint_id,
                                runtime.clone(),
                                nearby.clone(),
                                app_handle.clone(),
                            );
                        }
                        ObserveOutcome::RefreshNeeded => {
                            // No `nearby-device-found`: the row is already on
                            // screen and a rename isn't an arrival.
                            spawn_identity_probe(
                                endpoint_id,
                                runtime.clone(),
                                nearby.clone(),
                                app_handle.clone(),
                            );
                        }
                        ObserveOutcome::Paired => {
                            // A known device just came online — retry presence
                            // now rather than waiting out the backoff. No-op if
                            // a live session already exists.
                            paired_connections.nudge_reconnect(&endpoint_id).await;
                        }
                        ObserveOutcome::Known | ObserveOutcome::Invalid => {}
                    }
                }
                LanEvent::Vanished { endpoint_id } => {
                    if nearby.lock().await.expire(&endpoint_id) {
                        emit_nearby(&app_handle, "nearby-device-lost", &endpoint_id);
                    }
                }
            }
        }
    })
}

/// Runs `probe_identity_via` in the background and feeds a successful reply
/// back into the registry. Old build peers or a declined probe leave the
/// device listed but unidentified, so the user can still send to it.
fn spawn_identity_probe(
    endpoint_id: String,
    runtime: Arc<Mutex<NodeRuntime>>,
    nearby: Arc<Mutex<NearbyRegistry>>,
    app_handle: AppHandle,
) {
    tokio::spawn(async move {
        tracing::debug!(
            target: "dashbeam::_events::nearby::probe_started",
            remote = %endpoint_id,
        );
        match probe_identity_via(&runtime, &endpoint_id).await {
            Ok(info) => {
                // `Identity.os` is `#[serde(default)]`, so an old-build peer's
                // reply arrives as `""` — normalize rather than store it.
                let os = if info.os.is_empty() {
                    None
                } else {
                    Some(info.os)
                };
                let updated = nearby.lock().await.set_identity(
                    &endpoint_id,
                    info.display_name,
                    info.device_type,
                    os,
                );
                if updated {
                    emit_nearby(&app_handle, "nearby-device-identified", &endpoint_id);
                }
            }
            Err(err) => {
                tracing::debug!(
                    target: "dashbeam::_events::nearby::probe_failed",
                    remote = %endpoint_id,
                    error = %format!("{err:#}"),
                );
            }
        }
    });
}

/// Whether `endpoint_id` is already in the paired allowlist. Nearby ignores
/// paired peers entirely — normal presence tracking applies to them instead.
async fn is_paired_endpoint(access: &Arc<RwLock<AccessState>>, endpoint_id: &str) -> bool {
    match EndpointId::from_str(endpoint_id) {
        Ok(id) => access.read().await.allowed.contains(&id),
        Err(_) => false,
    }
}

/// Dial a peer's control ALPN and ask who it is. Shared by
/// `NodeService::probe_identity` and the background Nearby identity probe.
async fn probe_identity_via(
    runtime: &Arc<Mutex<NodeRuntime>>,
    endpoint_id: &str,
) -> anyhow::Result<DeviceInfo> {
    let remote: EndpointId = endpoint_id.parse()?;
    let endpoint = {
        let runtime = runtime.lock().await;
        runtime.endpoint.clone()
    };
    let conn = tokio::time::timeout(
        Duration::from_secs(5),
        endpoint.connect(remote, CONTROL_ALPN),
    )
    .await
    .context("identity probe timed out")??;

    let (mut send, mut recv) = conn.open_bi().await?;
    write_message(&mut send, &ControlMessage::WhoAreYou).await?;
    match tokio::time::timeout(Duration::from_secs(5), read_message(&mut recv))
        .await
        .context("identity reply timed out")??
    {
        ControlMessage::Identity {
            endpoint_id,
            display_name,
            device_type,
            os,
        } => Ok(DeviceInfo {
            endpoint_id,
            display_name,
            device_type,
            os,
        }),
        other => anyhow::bail!("unexpected reply to WhoAreYou: {other:?}"),
    }
}

/// Emits a Nearby event carrying just the endpoint id, matching what the
/// frontend needs to look up the affected row in its own Nearby list.
fn emit_nearby(app_handle: &AppHandle, event: &str, endpoint_id: &str) {
    emit_nearby_payload(
        app_handle,
        event,
        serde_json::json!({ "endpointId": endpoint_id }),
    );
}

/// Emits a Nearby event carrying a free-form reason, used for `nearby-unavailable`.
fn emit_nearby_reason(app_handle: &AppHandle, event: &str, reason: &str) {
    emit_nearby_payload(app_handle, event, serde_json::json!({ "reason": reason }));
}

fn emit_nearby_payload(app_handle: &AppHandle, event: &str, payload: serde_json::Value) {
    let Some(handle) = app_handle.as_ref() else {
        return;
    };
    if let Err(err) = handle.emit_event_with_payload(event, &payload.to_string()) {
        tracing::debug!("emit {event} failed: {err:#}");
    }
}

async fn send_forget_to_peer(
    runtime: &Arc<Mutex<NodeRuntime>>,
    identity: &DeviceIdentity,
    endpoint_id: &str,
    stored_relay: Option<&str>,
) -> anyhow::Result<()> {
    let remote = EndpointId::from_str(endpoint_id)?;

    let endpoint = {
        let runtime_guard = runtime.lock().await;
        runtime_guard.endpoint.clone()
    };
    let addr = build_control_connect_addr(&endpoint, remote, stored_relay);

    let connect = tokio::time::timeout(
        Duration::from_secs(PRESENCE_CONNECT_TIMEOUT_SECS),
        endpoint.connect(addr, CONTROL_ALPN),
    )
    .await;

    let conn = match connect {
        Ok(Ok(conn)) => {

            conn
        }
        Ok(Err(err)) => {
            return Err(err).context("forget connect failed");
        }
        Err(_) => {

            anyhow::bail!("forget connect timeout");
        }
    };

    send_forget_on_connection(identity, &conn).await
}

async fn send_forget_on_connection(
    identity: &DeviceIdentity,
    conn: &iroh::endpoint::Connection,
) -> anyhow::Result<()> {
    let keying = export_connection_keying_material(conn)?;
    let (mut send, _recv) = conn.open_bi().await.context("forget open bi")?;
    let forget = ControlMessage::Forget {
        signature: sign_challenge(&identity.secret_key, &keying),
    };
    write_message(&mut send, &forget)
        .await
        .context("forget write message")?;
    let _ = send.finish();
    // Timeout is a flush fallback for older peers that don't close.
    match tokio::time::timeout(Duration::from_secs(5), conn.closed()).await {
        Ok(_closed) => {},
        Err(_) => {},
    }

    Ok(())
}

fn endpoint_home_relay_url(endpoint: &Endpoint) -> Option<String> {
    let mut local_addr = endpoint.addr();
    apply_options(&mut local_addr, AddrInfoOptions::Relay);
    let url = local_addr.relay_urls().next().map(|u| u.to_string());
    url
}

async fn build_runtime(
    identity: Arc<DeviceIdentity>,
    paired_store: Arc<PairedDeviceStore>,
    access: Arc<RwLock<AccessState>>,
    blocked: Arc<RwLock<HashSet<String>>>,
    pending_nearby_invites: Arc<RwLock<HashMap<String, PendingNearbyInvite>>>,
    nearby: Arc<Mutex<NearbyRegistry>>,
    unpaired_limiter: Arc<std::sync::Mutex<UnpairedRateLimiter>>,
    pairing_host_persistent: Arc<AtomicBool>,
    app_handle: AppHandle,
    presence: Arc<std::sync::RwLock<HashMap<String, bool>>>,
    paired_connections: Arc<PairedConnectionManager>,
    network_ready: Arc<AtomicBool>,
    relay_mode: RelayMode,
    discovery_mode: DiscoveryModeOption,
) -> anyhow::Result<NodeRuntime> {

    let hook = PairedOnlyHook {
        access: access.clone(),
        blocked,
    };

    // The control endpoint must publish (so paired peers find us) and resolve
    // (to reach them). Custom mode uses a self-hosted pkarr relay, with
    // dns_origin optionally adding real-DNS resolve; default keeps n0.
    //
    // OS CA trust only for custom infra — n0's trailing-dot hostnames break
    // Windows CERT_CHAIN_POLICY_SSL name matching.
    let custom_infra = matches!(discovery_mode, DiscoveryModeOption::Custom { .. })
        || matches!(relay_mode, RelayMode::Custom(_));
    let builder = match &discovery_mode {
        DiscoveryModeOption::Custom {
            pkarr_relay_url,
            dns_origin,
        } => {
            let mut builder =
                protocol::with_system_ca_if_custom(Endpoint::builder(presets::Minimal), custom_infra)
                    .address_lookup(PkarrPublisher::builder(pkarr_relay_url.clone()))
                    .address_lookup(PkarrResolver::builder(pkarr_relay_url.clone()));
            if let Some(origin) = dns_origin {
                builder = builder.address_lookup(DnsAddressLookup::builder(origin.clone()));
            }
            builder
        }
        DiscoveryModeOption::Default => {
            protocol::with_system_ca_if_custom(Endpoint::builder(presets::N0), custom_infra)
                .address_lookup(PkarrPublisher::n0_dns())
        }
    };

    let endpoint = builder
        .secret_key(identity.secret_key.clone())
        .relay_mode(relay_mode.clone())
        .hooks(hook)
        .alpns(vec![CONTROL_ALPN.to_vec()])
        .bind()
        .await?;

    // Publish the node immediately after bind so pairing UI/API are available
    // without waiting on relay home connection (often several seconds on mobile).
    let home_relay_url = Arc::new(std::sync::RwLock::new(endpoint_home_relay_url(&endpoint)));

    let control = ControlProtocol {
        ctx: ControlCtx {
            identity,
            paired_store,
            access,
            pairing_host_persistent,
            app_handle: app_handle.clone(),
            home_relay_url: home_relay_url.clone(),
            presence,
            paired_connections,
            pending_nearby_invites,
            nearby,
            unpaired_limiter,
        },
    };

    let router = Router::builder(endpoint.clone())
        .accept(CONTROL_ALPN, control)
        .spawn();

    let mark_network_ready = {
        let network_ready = network_ready.clone();
        let home_relay_url = home_relay_url.clone();
        let app_handle = app_handle.clone();
        move |endpoint: &Endpoint| {
            if let Some(url) = endpoint_home_relay_url(endpoint) {
                if let Ok(mut guard) = home_relay_url.write() {
                    *guard = Some(url);
                }
            }
            network_ready.store(true, Ordering::SeqCst);
            if let Some(handle) = &app_handle {
                let _ = handle.emit_event("device-node-network-ready");
            }
        }
    };

    // No relay path to wait on — treat network as ready immediately.
    if matches!(relay_mode, RelayMode::Disabled) {
        mark_network_ready(&endpoint);
    } else {
        let online_endpoint = endpoint.clone();
        tokio::spawn(async move {
            online_endpoint.online().await;
            mark_network_ready(&online_endpoint);
        });
    }

    Ok(NodeRuntime { endpoint, router })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe_reply(endpoint_id: &str) -> DeviceInfo {
        DeviceInfo {
            endpoint_id: endpoint_id.to_string(),
            display_name: "Bob's Laptop".to_string(),
            device_type: "laptop".to_string(),
            os: "macos".to_string(),
        }
    }

    #[test]
    fn nearby_peer_identity_keeps_cosmetics_from_a_matching_reply() {
        let info = nearby_peer_identity("abcdef1234567890", Some(probe_reply("abcdef1234567890")));
        assert_eq!(info.endpoint_id, "abcdef1234567890");
        assert_eq!(info.display_name, "Bob's Laptop");
        assert_eq!(info.device_type, "laptop");
        assert_eq!(info.os, "macos");
    }

    #[test]
    fn nearby_peer_identity_matches_ids_case_insensitively() {
        let info = nearby_peer_identity("ABCDEF1234567890", Some(probe_reply("abcdef1234567890")));
        assert_eq!(
            info.endpoint_id, "ABCDEF1234567890",
            "the connection-verified id is stored verbatim"
        );
        assert_eq!(info.display_name, "Bob's Laptop");
    }

    /// Guards against a peer answering the probe with someone else's endpoint
    /// id: the connection-verified id wins and the reply is discarded.
    #[test]
    fn nearby_peer_identity_rejects_a_mismatching_reply_id() {
        let info = nearby_peer_identity("abcdef1234567890", Some(probe_reply("attacker0000000")));
        assert_eq!(info.endpoint_id, "abcdef1234567890");
        assert_eq!(info.display_name, "abcdef12", "falls back to the id prefix");
        assert_ne!(info.display_name, "Bob's Laptop");
    }

    #[test]
    fn nearby_peer_identity_falls_back_when_the_probe_failed() {
        let info = nearby_peer_identity("abcdef1234567890", None);
        assert_eq!(info.endpoint_id, "abcdef1234567890");
        assert_eq!(info.display_name, "abcdef12");
        assert_eq!(info.os, "");
    }
}
