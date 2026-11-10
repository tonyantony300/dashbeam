use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use iroh::endpoint::{Connection, ConnectionError};
use iroh::EndpointId;
use protocol::{
    export_connection_keying_material, read_message, sign_challenge, verify_challenge,
    write_message, ControlMessage, InviteResponse, PairedDevice, CONTROL_ALPN,
    PAIRED_INVITE_WAIT_SECS, PAIRED_RECONNECT_MAX_SECS, PAIRED_RECONNECT_MIN_SECS,
    PRESENCE_CONNECT_TIMEOUT_SECS, RECENT_PAIRING_GRACE_MS, SETTLING_PAIRING_RETRY_SECS,
};
use tokio::sync::{Mutex, Notify, OnceCell, RwLock};
use tokio::task::JoinHandle;

use crate::device_identity::{DeviceIdentity, PairedDeviceStore};
use crate::node::AccessState;
use crate::pairing_util::{
    build_control_connect_addr, emit_paired_invite_received,
    emit_paired_invite_response, set_presence,
};
use protocol::AppHandle;

struct SessionWaiter {
    notify: Arc<Notify>,
}

/// Maintains persistent outbound control connections to paired devices.
/// Presence is derived from live connection state instead of polling probes.
pub struct PairedConnectionManager {
    runtime: Arc<OnceCell<Arc<Mutex<crate::runtime::NodeRuntime>>>>,
    identity: Arc<DeviceIdentity>,
    paired_store: Arc<PairedDeviceStore>,
    access: Arc<RwLock<AccessState>>,
    presence: Arc<std::sync::RwLock<HashMap<String, bool>>>,
    app_handle: AppHandle,
    sessions: Arc<RwLock<HashMap<String, Connection>>>,
    waiters: Arc<Mutex<HashMap<String, SessionWaiter>>>,
    tasks: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
    shutdown: Arc<AtomicBool>,
}

impl PairedConnectionManager {
    pub(crate) fn new(
        identity: Arc<DeviceIdentity>,
        paired_store: Arc<PairedDeviceStore>,
        access: Arc<RwLock<AccessState>>,
        presence: Arc<std::sync::RwLock<HashMap<String, bool>>>,
        app_handle: AppHandle,
    ) -> Self {

        Self {
            runtime: Arc::new(OnceCell::new()),
            identity,
            paired_store,
            access,
            presence,
            app_handle,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            waiters: Arc::new(Mutex::new(HashMap::new())),
            tasks: Arc::new(Mutex::new(HashMap::new())),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn attach_runtime(&self, runtime: Arc<Mutex<crate::runtime::NodeRuntime>>) {

        let _ = self.runtime.set(runtime);
    }

    pub fn start(self: &Arc<Self>) -> JoinHandle<()> {

        let manager = Arc::clone(self);
        tokio::spawn(async move {
            manager.run_supervisor().await;
        })
    }

    pub async fn refresh(&self) {

        let devices = match self.paired_store.list() {
            Ok(devices) => devices,
            Err(_err) => {

                return;
            }
        };
        let active: Vec<PairedDevice> = devices
            .into_iter()
            .filter(|d| d.pairing_status.is_connectable())
            .collect();
        let active_ids: std::collections::HashSet<String> = active
            .iter()
            .map(|d| d.endpoint_id.to_lowercase())
            .collect();

        let mut tasks = self.tasks.lock().await;
        let before = tasks.len();
        tasks.retain(|endpoint_id, handle| {
            if active_ids.contains(endpoint_id) {
                true
            } else {

                handle.abort();
                false
            }
        });
        if tasks.len() != before {

        }

        for device in active {
            let key = device.endpoint_id.to_lowercase();
            if tasks.contains_key(&key) {

                continue;
            }

            let handle = self.spawn_connect_task(device);
            tasks.insert(key, handle);
        }

    }

    /// Resets a paired device's reconnect backoff and connects immediately —
    /// used when mDNS reports it just appeared, rather than waiting out a
    /// backoff that may be minutes long.
    ///
    /// A no-op if the device isn't connectable or a live session already
    /// exists: mDNS re-fires `Discovered` often, and aborting a live session
    /// each time flaps presence.
    pub async fn nudge_reconnect(&self, endpoint_id: &str) {
        let key = endpoint_id.to_lowercase();
        let Ok(Some(device)) = self.paired_store.get(endpoint_id) else {
            return;
        };
        if !device.pairing_status.is_connectable() {
            return;
        }

        // Only wake a task sleeping in backoff; leave a live session alone.
        if self.sessions.read().await.contains_key(&key) {
            return;
        }

        self.respawn_connect_task(key, device).await;
    }

    /// Test-only: the old nudge behaviour — abort and re-dial even when a live
    /// session exists. Used to simulate an unfixed peer flapping its outbound.
    #[doc(hidden)]
    pub async fn force_reconnect_for_tests(&self, endpoint_id: &str) {
        let key = endpoint_id.to_lowercase();
        let Ok(Some(device)) = self.paired_store.get(endpoint_id) else {
            return;
        };
        if !device.pairing_status.is_connectable() {
            return;
        }
        self.respawn_connect_task(key, device).await;
    }

    async fn respawn_connect_task(&self, key: String, device: PairedDevice) {
        let mut tasks = self.tasks.lock().await;
        if let Some(handle) = tasks.remove(&key) {
            handle.abort();
        }
        let handle = self.spawn_connect_task(device);
        tasks.insert(key, handle);
    }

    pub async fn forget(&self, endpoint_id: &str) {
        let key = endpoint_id.to_lowercase();

        if let Some(handle) = self.tasks.lock().await.remove(&key) {
            handle.abort();

        }
        self.remove_session(&key).await;
    }

    /// Live outbound/inbound session for `endpoint_id`, if one is currently held.
    pub async fn live_session(&self, endpoint_id: &str) -> Option<Connection> {
        let key = endpoint_id.to_lowercase();
        self.sessions.read().await.get(&key).cloned()
    }

    pub async fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let mut tasks = self.tasks.lock().await;
        for (_, handle) in tasks.drain() {
            handle.abort();
        }
        self.sessions.write().await.clear();
    }

    /// Returns a live connection if one exists, otherwise waits until connected or timeout.
    pub async fn wait_for_connection(
        &self,
        endpoint_id: &str,
        timeout: Duration,
    ) -> Option<Connection> {
        let key = endpoint_id.to_lowercase();
                if let Some(conn) = self.sessions.read().await.get(&key).cloned() {

            return Some(conn);
        }

        let notify = {
            let mut waiters = self.waiters.lock().await;
            waiters
                .entry(key.clone())
                .or_insert_with(|| SessionWaiter {
                    notify: Arc::new(Notify::new()),
                })
                .notify
                .clone()
        };

        let wait = async {
            loop {
                if let Some(conn) = self.sessions.read().await.get(&key).cloned() {
                    return Some(conn);
                }
                notify.notified().await;
            }
        };
        match tokio::time::timeout(timeout, wait).await {
            Ok(Some(conn)) => {

                Some(conn)
            }
            Ok(None) => None,
            Err(_) => {

                None
            }
        }
    }

    pub async fn register_inbound(&self, endpoint_id: &str, conn: Connection) {
        let key = endpoint_id.to_lowercase();
        // Don't replace a session we already hold (almost always our own
        // outbound) — the inbound is still served for invites, and replacing
        // would let its later unregister wipe our live outbound.
        if self.sessions.read().await.contains_key(&key) {
            set_presence(
                &self.presence,
                &self.app_handle,
                &self.paired_store,
                &key,
                true,
            );
            return;
        }
        self.set_session(&key, conn).await;
    }

    pub async fn unregister_inbound(&self, endpoint_id: &str, conn: &Connection) {
        let key = endpoint_id.to_lowercase();
        // Only clear presence if *this* connection is the one we stored.
        self.remove_session_if(&key, conn).await;
    }

    async fn run_supervisor(self: Arc<Self>) {

        self.refresh().await;
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }

            self.refresh().await;
        }

    }

    fn spawn_connect_task(&self, device: PairedDevice) -> JoinHandle<()> {
        let manager = Arc::new(PairedConnectionManager {
            runtime: self.runtime.clone(),
            identity: self.identity.clone(),
            paired_store: self.paired_store.clone(),
            access: self.access.clone(),
            presence: self.presence.clone(),
            app_handle: self.app_handle.clone(),
            sessions: self.sessions.clone(),
            waiters: self.waiters.clone(),
            tasks: self.tasks.clone(),
            shutdown: self.shutdown.clone(),
        });
        let endpoint_id = device.endpoint_id.clone();
        let relay_url = device.relay_url.clone();
        tokio::spawn(async move {
            manager
                .connect_loop(&endpoint_id, relay_url.as_deref())
                .await;
        })
    }

    async fn connect_loop(&self, endpoint_id: &str, stored_relay: Option<&str>) {
        let key = endpoint_id.to_lowercase();
        let mut backoff_secs = PAIRED_RECONNECT_MIN_SECS;
        let mut attempt: u32 = 0;

        while !self.shutdown.load(Ordering::SeqCst) {
            attempt += 1;
            if !self.device_still_active(endpoint_id) {

                break;
            }

            match self.connect_and_recognize(endpoint_id, stored_relay, attempt).await {
                Ok(conn) => {
                    backoff_secs = PAIRED_RECONNECT_MIN_SECS;

                    self.set_session(&key, conn.clone()).await;
                    self.spawn_identity_heal(endpoint_id, &conn);

                    // Read invites on the outbound (client) connection. The peer opens
                    // bi streams on this link when they are the QUIC server.
                    let unpaired_via_forget = tokio::select! {
                        _ = conn.closed() => false,
                        unpaired = self.read_outbound_control_loop(&conn) => unpaired,
                    };
                    self.remove_session_if(&key, &conn).await;
                    // "Last seen" is when the session ended, not when it began.
                    let _ = self
                        .paired_store
                        .touch(endpoint_id, protocol::identity::unix_now_ms());

                    if unpaired_via_forget {
                        break;
                    }

                    // Forget notify is best-effort: a peer that already dropped
                    // us accepts the dial then rejects Recognition, so treat
                    // that close as a remote unpair.
                    //
                    // Unless the pairing is still settling — each side commits
                    // independently, so a refusal this soon means the peer
                    // hasn't stored their half yet.
                    let close_err = conn.closed().await;
                    if close_implies_remote_unpair(&close_err) {
                        if self.pairing_is_settling(endpoint_id) {
                            tracing::debug!(
                                target: "dashbeam::_events::pairing::remote_unpair",
                                remote = %crate::node::short_remote(endpoint_id),
                                applied = false,
                                cause = "pairing still settling",
                            );
                            backoff_secs = SETTLING_PAIRING_RETRY_SECS;
                        } else if self.apply_remote_unpair(endpoint_id).await {
                            break;
                        }
                    }
                }
                Err(_err) => {
                    set_presence(
                        &self.presence,
                        &self.app_handle,
                        &self.paired_store,
                        endpoint_id,
                        false,
                    );
                }
            }

            if self.shutdown.load(Ordering::SeqCst) {

                break;
            }

            tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs * 2).min(PAIRED_RECONNECT_MAX_SECS);
        }

    }

    /// If the stored record still has pair-time fallbacks (see
    /// [`crate::device_identity::needs_identity_heal`]), asks the peer over
    /// this connection — its session loop answers paired peers — and fills
    /// them in. Best effort.
    fn spawn_identity_heal(&self, endpoint_id: &str, conn: &Connection) {
        let needs = matches!(
            self.paired_store.get(endpoint_id),
            Ok(Some(d)) if crate::device_identity::needs_identity_heal(&d)
        );
        if !needs {
            return;
        }
        let (store, conn, id) = (
            self.paired_store.clone(),
            conn.clone(),
            endpoint_id.to_string(),
        );
        tokio::spawn(async move {
            let reply = async {
                let (mut send, mut recv) = conn.open_bi().await.ok()?;
                write_message(&mut send, &ControlMessage::WhoAreYou)
                    .await
                    .ok()?;
                tokio::time::timeout(Duration::from_secs(10), read_message(&mut recv))
                    .await
                    .ok()?
                    .ok()
            }
            .await;
            let Some(ControlMessage::Identity {
                endpoint_id,
                display_name,
                device_type,
                os,
            }) = reply
            else {
                return;
            };
            // Self-reported: only cosmetic fields, and only for the id this
            // connection authenticated.
            if !endpoint_id.eq_ignore_ascii_case(&id) {
                return;
            }
            match store.heal_identity(&id, &display_name, &device_type, &os) {
                Ok(Some(d)) => tracing::info!(
                    remote = %crate::node::short_remote(&id),
                    name = %d.display_name,
                    "filled in paired device identity"
                ),
                Ok(None) => {}
                Err(e) => tracing::debug!("identity heal for {id} failed: {e:#}"),
            }
        });
    }

    async fn connect_and_recognize(
        &self,
        endpoint_id: &str,
        stored_relay: Option<&str>,
        _attempt: u32,
    ) -> anyhow::Result<Connection> {
        let remote = EndpointId::from_str(endpoint_id)?;
        let runtime = self
            .runtime
            .get()
            .context("paired connection manager runtime not attached")?;

        // Clone the endpoint under the short-lived runtime lock, then connect
        // outside it. Holding the mutex across connect timeouts serialized every
        // presence reconnect and blocked pairing_ticket / Devices UI for ~8s
        // per offline peer.
        let endpoint = {
            let runtime = runtime.lock().await;
            runtime.endpoint.clone()
        };
        let addr = build_control_connect_addr(&endpoint, remote, stored_relay);
        let conn = tokio::time::timeout(
            Duration::from_secs(PRESENCE_CONNECT_TIMEOUT_SECS),
            endpoint.connect(addr, CONTROL_ALPN),
        )
        .await
        .context("paired connect timeout")?
        .context("paired connect failed")?;
        
        let keying = export_connection_keying_material(&conn).context("export keying")?;

        let (mut send, _recv) = conn
            .open_bi()
            .await
            .context("open bi for recognition")?;
        let recognition = ControlMessage::Recognition {
            signature: sign_challenge(&self.identity.secret_key, &keying),
        };
        write_message(&mut send, &recognition)
            .await
            .context("write recognition")?;

        Ok(conn)
    }

    /// Accept bi streams on an outbound (client) control connection and handle invites.
    /// Returns true when the peer unpaired us and the connect loop should stop.
    async fn read_outbound_control_loop(&self, conn: &Connection) -> bool {
        let remote = conn.remote_id();
        let remote_id = remote.to_string();
                let keying = match export_connection_keying_material(conn) {
            Ok(keying) => keying,
            Err(_err) => {

                return false;
            }
        };
        loop {
            let (_send, mut recv) = match conn.accept_bi().await {
                Ok(streams) => streams,
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

            match msg {
                ControlMessage::Invite {
                    blob_ticket,
                    file_count,
                    total_size,
                    sender_name,
                } => {

                    emit_paired_invite_received(
                        &self.app_handle,
                        &remote_id,
                        &blob_ticket,
                        file_count,
                        total_size,
                        &sender_name,
                    );
                }
                ControlMessage::InviteResponse { response, .. } => {
                    let response_str = match response {
                        InviteResponse::Accepted => "accepted",
                        InviteResponse::Declined => "declined",
                    };

                    // Always an already-paired peer, so `paired_store` has
                    // the name.
                    emit_paired_invite_response(
                        &self.app_handle,
                        &self.paired_store,
                        &remote_id,
                        response_str,
                        None,
                    );
                }
                ControlMessage::Recognition { signature } => {
                    if verify_challenge(&remote, &keying, &signature) {

                        let now = protocol::identity::unix_now_ms();
                        let _ = self.paired_store.touch(&remote_id, now);
                        set_presence(
                            &self.presence,
                            &self.app_handle,
                            &self.paired_store,
                            &remote_id,
                            true
                        );
                    }
                }
                ControlMessage::Forget { signature } => {
                    if verify_challenge(&remote, &keying, &signature)
                        && self.apply_remote_unpair(&remote_id).await
                    {
                        return true;
                    }
                }
                // This connection is to a paired peer by construction.
                ControlMessage::Text { sender_name, body } => {
                    if body.len() <= protocol::MAX_TEXT_BYTES {
                        crate::pairing_util::emit_paired_text_received(
                            &self.app_handle,
                            &remote_id,
                            &sender_name,
                            &body,
                        );
                    }
                }
                _other => {

                }
            }
        }
        false
    }

    /// True while our record is new enough that a refused `Recognition` more
    /// likely means the peer hasn't committed yet than that it unpaired us.
    /// A genuine unpair still lands immediately via the signed `Forget`.
    fn pairing_is_settling(&self, endpoint_id: &str) -> bool {
        let Ok(Some(existing)) = self.paired_store.get(endpoint_id) else {
            return false;
        };
        protocol::identity::unix_now_ms().saturating_sub(existing.paired_at)
            < RECENT_PAIRING_GRACE_MS
    }

    /// Marks the peer unpaired-remotely, drops allowlist + presence, emits
    /// `device-unpaired`. Returns true when a previously-active record was updated.
    async fn apply_remote_unpair(&self, endpoint_id: &str) -> bool {
        let Ok(Some(existing)) = self.paired_store.get(endpoint_id) else {
            tracing::debug!(
                target: "dashbeam::_events::pairing::remote_unpair",
                remote = %crate::node::short_remote(endpoint_id),
                applied = false,
                cause = "no stored record",
            );
            return false;
        };
        if !existing.pairing_status.is_active() {
            tracing::debug!(
                target: "dashbeam::_events::pairing::remote_unpair",
                remote = %crate::node::short_remote(endpoint_id),
                applied = false,
                cause = "record not active",
            );
            return false;
        }
        let marked = self.paired_store.mark_unpaired_remotely(endpoint_id);
        let Ok(Some(device)) = marked else {
            tracing::debug!(
                target: "dashbeam::_events::pairing::remote_unpair",
                remote = %crate::node::short_remote(endpoint_id),
                applied = false,
                cause = "store write failed",
            );
            return false;
        };
        // `age_ms` separates a genuine unpair from a not-yet-committed pairing.
        tracing::debug!(
            target: "dashbeam::_events::pairing::remote_unpair",
            remote = %crate::node::short_remote(endpoint_id),
            applied = true,
            age_ms = protocol::identity::unix_now_ms().saturating_sub(existing.paired_at),
        );
        if let Ok(id) = EndpointId::from_str(endpoint_id) {
            self.access.write().await.allowed.remove(&id);
        }
        set_presence(
            &self.presence,
            &self.app_handle,
            &self.paired_store,
            endpoint_id,
            false,
        );
        let payload = serde_json::json!({
            "endpoint_id": device.endpoint_id,
            "display_name": device.display_name,
            "reason": "remote",
        });
        if let Some(handle) = &self.app_handle {
            let _ = handle.emit_event_with_payload("device-unpaired", &payload.to_string());
        }
        true
    }

    fn device_still_active(&self, endpoint_id: &str) -> bool {
        self.paired_store
            .get(endpoint_id)
            .ok()
            .flatten()
            .is_some_and(|d| d.pairing_status.is_active())
    }

    async fn set_session(&self, key: &str, conn: Connection) {
        {
            let mut sessions = self.sessions.write().await;
            sessions.insert(key.to_string(), conn);
        }
        set_presence(
            &self.presence,
            &self.app_handle,
            &self.paired_store,
            key,
            true,
        );
        let now = protocol::identity::unix_now_ms();
        let _ = self.paired_store.touch(key, now);
        self.notify_waiters(key).await;
    }

    async fn remove_session(&self, key: &str) {
        let removed = self.sessions.write().await.remove(key).is_some();
        if removed {
            set_presence(
                &self.presence,
                &self.app_handle,
                &self.paired_store,
                key,
                false,
            );
        }
    }

    /// Drop the stored session only when it is still `conn`, so an old teardown
    /// can't clear a newer live link.
    async fn remove_session_if(&self, key: &str, conn: &Connection) {
        let removed = {
            let mut sessions = self.sessions.write().await;
            let matches = sessions
                .get(key)
                .is_some_and(|current| current.stable_id() == conn.stable_id());
            if matches {
                sessions.remove(key).is_some()
            } else {
                false
            }
        };
        if removed {
            set_presence(
                &self.presence,
                &self.app_handle,
                &self.paired_store,
                key,
                false,
            );
        }
    }

    async fn notify_waiters(&self, key: &str) {
        let mut waiters = self.waiters.lock().await;
        if let Some(waiter) = waiters.remove(key) {
            waiter.notify.notify_waiters();
        }
    }
}

/// Peer closed after rejecting our Recognition as an unpaired stranger.
/// (`b"unpaired"` is the ack after *they* processed our Forget — not a signal
/// that we should mark *them* unpaired-remotely.)
fn close_implies_remote_unpair(err: &ConnectionError) -> bool {
    matches!(
        err,
        ConnectionError::ApplicationClosed(close)
            if close.reason.as_ref() == b"not permitted for unpaired peer"
    )
}

pub fn invite_wait_timeout() -> Duration {
    Duration::from_secs(PAIRED_INVITE_WAIT_SECS)
}
