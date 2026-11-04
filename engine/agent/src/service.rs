//! `AgentService`: the headless node plus the agent's orchestration state.

use crate::approver::Approver;
use crate::cancel::{Cancelled, Cancels};
use crate::config::AgentConfig;
use crate::events::{ChannelEmitter, EngineEvent};
use crate::inbound::{self, Inbound};
use crate::outbound::Outbound;
use crate::registry::{Registry, TransferRecord, TransferStatus};
use anyhow::{anyhow, bail, Context, Result};
use engine::{
    DeviceInfo, Discoverability, IdentityOptions, NearbyDevice, NodeService, PairedDeviceInfo,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

/// Device type the agent pins in its identity.
pub const AGENT_DEVICE_TYPE: &str = "agent";
/// Transfers kept in the in-memory registry.
const REGISTRY_CAP: usize = 500;
/// How long `cancel_transfer` waits for a running transfer to wind down.
const CANCEL_WAIT: Duration = Duration::from_secs(10);

pub struct AgentOptions {
    pub data_dir: PathBuf,
    pub config: AgentConfig,
    /// `--dangerously-allow-agent-pairing`; CLI flag only, never config.
    pub allow_agent_pairing: bool,
    /// `up --discoverable`: forces `Discoverability::Everyone`.
    pub discoverable_override: bool,
    pub approver: Arc<dyn Approver>,
}

pub struct AgentService {
    data_dir: PathBuf,
    config: AgentConfig,
    allow_agent_pairing: bool,
    approver: Arc<dyn Approver>,
    node: Arc<NodeService>,
    registry: Registry,
    pub(crate) inbound: Inbound,
    pub(crate) outbound: Outbound,
    /// Cancel slots for every cancellable transfer, inbound and outbound.
    pub(crate) cancels: Cancels,
    stop_tx: watch::Sender<bool>,
    event_loop: Mutex<Option<JoinHandle<()>>>,
    auto_worker: Mutex<Option<JoinHandle<()>>>,
    pairing_tasks: Mutex<Vec<JoinHandle<()>>>,
    /// Lowercase ids already paired when trust was last evaluated.
    pub(crate) paired_seen: Mutex<std::collections::HashSet<String>>,
    shut_down: AtomicBool,
}

/// `"<host name> Agent"`, so the agent is distinguishable from the app on the same machine.
pub fn default_agent_name() -> String {
    format!("{} Agent", engine::default_display_name())
}

/// Keychain-free, env-free identity pinned to the agent device type.
pub fn agent_identity_options() -> IdentityOptions {
    IdentityOptions {
        use_keychain: false,
        honor_env_secret: false,
        device_type: Some(AGENT_DEVICE_TYPE.to_string()),
        default_display_name: Some(default_agent_name()),
    }
}

impl AgentService {
    pub async fn start(opts: AgentOptions) -> Result<Arc<Self>> {
        // The agent never uses IROH_SECRET; drop it so ephemeral share sessions
        // (which read it via protocol::get_or_create_secret) can't either.
        if std::env::var_os("IROH_SECRET").is_some() {
            tracing::warn!(
                "IROH_SECRET is set but ignored by the agent; removing it from the environment"
            );
            std::env::remove_var("IROH_SECRET");
        }

        let relay_mode = engine::build_relay_mode(opts.config.relay.clone())
            .map_err(|e| anyhow!("invalid relay config: {e}"))?;
        let discovery_mode = engine::build_discovery_mode(opts.config.discovery.clone())
            .map_err(|e| anyhow!("invalid discovery config: {e}"))?;
        let discoverability = if opts.discoverable_override {
            Discoverability::Everyone
        } else {
            opts.config.discoverability.to_engine()
        };

        let (emitter, events_rx) = ChannelEmitter::new();
        let node = NodeService::start_with(
            &opts.data_dir,
            relay_mode.into(),
            discovery_mode,
            discoverability,
            Some(Arc::new(emitter)),
            agent_identity_options(),
        )
        .await
        .context("start agent node")?;
        let node = Arc::new(node);

        if let Err(e) = apply_config(&node, &opts.config).await {
            let _ = node.shutdown().await;
            return Err(e);
        }

        let (stop_tx, stop_rx) = watch::channel(false);
        let svc = Arc::new(Self {
            data_dir: opts.data_dir,
            config: opts.config,
            allow_agent_pairing: opts.allow_agent_pairing,
            approver: opts.approver,
            node,
            registry: Registry::new(REGISTRY_CAP),
            inbound: Inbound::new(),
            outbound: Outbound::default(),
            cancels: Cancels::default(),
            stop_tx,
            event_loop: Mutex::new(None),
            auto_worker: Mutex::new(None),
            pairing_tasks: Mutex::new(Vec::new()),
            paired_seen: Mutex::new(Default::default()),
            shut_down: AtomicBool::new(false),
        });
        let handle = tokio::spawn(run_event_loop(Arc::downgrade(&svc), events_rx, stop_rx));
        *svc.event_loop.lock().expect("event loop lock") = Some(handle);
        let worker = tokio::spawn(inbound::run_auto_worker(
            Arc::downgrade(&svc),
            svc.inbound.auto_notify(),
            svc.stop_signal(),
        ));
        *svc.auto_worker.lock().expect("auto worker lock") = Some(worker);

        svc.snapshot_paired();
        *svc.pairing_tasks.lock().expect("pairing tasks lock") = svc.spawn_authorized_pairing();

        let me = svc.identity();
        tracing::info!(
            endpoint_id = %me.endpoint_id,
            name = %me.display_name,
            "agent node started"
        );
        Ok(svc)
    }

    /// The underlying node, for tests and advanced use.
    pub fn node(&self) -> &Arc<NodeService> {
        &self.node
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    pub fn approver(&self) -> &Arc<dyn Approver> {
        &self.approver
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn allow_agent_pairing(&self) -> bool {
        self.allow_agent_pairing
    }

    /// Flips to `true` once on shutdown; background tasks exit on it.
    pub(crate) fn stop_signal(&self) -> watch::Receiver<bool> {
        self.stop_tx.subscribe()
    }

    pub fn identity(&self) -> DeviceInfo {
        self.node.device_info()
    }

    pub fn list_devices(&self) -> Result<Vec<PairedDeviceInfo>> {
        self.node.list_paired()
    }

    pub async fn list_nearby(&self) -> Vec<NearbyDevice> {
        self.node.list_nearby().await
    }

    /// Persists the new display name. A `name` in config.toml still wins on next start.
    pub fn rename(&self, name: &str) -> Result<DeviceInfo> {
        self.node.set_device_display_name(name)
    }

    /// Cancels a queued or running transfer, inbound or outbound, and returns
    /// its record. An already-terminal transfer comes back unchanged (not an
    /// error). A running one gets up to 10 s to stop its share or download;
    /// if it is still winding down after that, the live record is returned
    /// and the cancel still lands.
    pub async fn cancel_transfer(&self, id: &str) -> Result<TransferRecord> {
        let unknown = || anyhow!("no transfer with id {id}");
        let rec = self.registry.get(id).ok_or_else(unknown)?;
        if rec.status.is_terminal() {
            return Ok(rec);
        }
        let how = self.cancels.cancel(id);
        if how == Cancelled::Pending {
            // Never started, so nothing else will write its terminal state.
            return self
                .registry
                .update_live(id, |r| r.status = TransferStatus::Cancelled)
                .or_else(|| self.registry.get(id))
                .ok_or_else(unknown);
        }
        let (rec, running) = self
            .registry
            .wait_terminal(id, CANCEL_WAIT)
            .await
            .ok_or_else(unknown)?;
        if running && how == Cancelled::NotTracked {
            bail!("transfer {id} can't be cancelled");
        }
        Ok(rec)
    }

    /// Stops the event loop, then the node. Idempotent.
    pub async fn shutdown(&self) -> Result<()> {
        if self.shut_down.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let _ = self.stop_tx.send(true);
        let handle = self.event_loop.lock().expect("event loop lock").take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
        let worker = self.auto_worker.lock().expect("auto worker lock").take();
        if let Some(worker) = worker {
            worker.abort();
            let _ = worker.await;
        }
        let tasks = std::mem::take(&mut *self.pairing_tasks.lock().expect("pairing tasks lock"));
        for t in tasks {
            t.abort();
            let _ = t.await;
        }
        self.node.shutdown().await
    }

    /// Single dispatch point for engine events. Handlers must not block: spawn
    /// with a cloned `Arc` for anything that awaits (e.g. the approver).
    fn handle_event(self: &Arc<Self>, name: &str, payload: Option<&str>) {
        // Invite payloads carry blob tickets, which are bearer secrets.
        let logged = if name == "paired-invite-received" {
            Some("<elided>")
        } else {
            payload
        };
        tracing::trace!(event = name, payload = ?logged, "engine event");
        match name {
            "paired-invite-received" => self.on_invite_received(payload),
            "nearby-pair-request-received" => self.on_pair_request(payload),
            "paired-invite-response" => self.on_invite_response(payload),
            // Progress arrives on per-transfer emitters, not the node's.
            "transfer-progress" | "receive-progress" => {}
            "device-paired" => self.on_device_paired(),
            "device-unpaired" => {}
            _ => {}
        }
    }
}

async fn apply_config(node: &NodeService, config: &AgentConfig) -> Result<()> {
    if let Some(name) = config
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        node.set_device_display_name(name)
            .context("apply config name")?;
    }
    let ids = config
        .pairing
        .authorized
        .iter()
        .map(|p| p.endpoint_id.clone())
        .collect();
    node.set_authorized_peers(ids)
        .await
        .context("apply pairing.authorized")?;
    Ok(())
}

async fn run_event_loop(
    svc: Weak<AgentService>,
    mut events: mpsc::UnboundedReceiver<EngineEvent>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        let (name, payload) = tokio::select! {
            biased;
            _ = stop.changed() => break,
            ev = events.recv() => match ev {
                Some(ev) => ev,
                None => break,
            },
        };
        let Some(svc) = svc.upgrade() else { break };
        svc.handle_event(&name, payload.as_deref());
    }
}
