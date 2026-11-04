//! Pairing: pre-approved peers at startup (like `authorized_keys`) and the
//! pairing API used by the CLI.

use crate::devices::{resolve_device, resolve_index, ResolveError};
use crate::service::AgentService;
use anyhow::{anyhow, bail, Result};
use engine::{PairedDeviceInfo, PairingTicket};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

const BACKOFF_START: Duration = Duration::from_secs(5);
const BACKOFF_CAP: Duration = Duration::from_secs(300);

const AGENT_PAIRING_DISABLED: &str =
    "agent pairing is disabled; start the node with --dangerously-allow-agent-pairing";

impl AgentService {
    /// Starts one join task per `pairing.authorized` entry; returns their handles.
    pub(crate) fn spawn_authorized_pairing(self: &Arc<Self>) -> Vec<tokio::task::JoinHandle<()>> {
        let me = self.identity().endpoint_id.to_lowercase();
        self.config()
            .pairing
            .authorized
            .iter()
            .filter(|p| p.endpoint_id.to_lowercase() != me)
            .map(|p| {
                tokio::spawn(join_authorized(
                    Arc::downgrade(self),
                    p.endpoint_id.clone(),
                    p.relay_url.clone(),
                    self.stop_signal(),
                ))
            })
            .collect()
    }

    /// Records the currently paired ids as the baseline for `apply_authorized_trust`.
    pub(crate) fn snapshot_paired(&self) {
        let ids = self.paired_ids();
        *self.paired_seen.lock().expect("paired seen lock") = ids;
    }

    /// Drops ids that are no longer paired so a later re-pair counts as new.
    pub(crate) fn prune_paired_seen(&self) {
        let now = self.paired_ids();
        self.paired_seen
            .lock()
            .expect("paired seen lock")
            .retain(|id| now.contains(id));
    }

    fn paired_ids(&self) -> HashSet<String> {
        self.list_devices()
            .map(|ds| {
                ds.into_iter()
                    .map(|d| d.endpoint_id.to_lowercase())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Trusts peers that became paired since the last snapshot and have an
    /// authorized `trust = true` entry. Earlier pairings are never re-trusted,
    /// so a manual untrust sticks.
    pub(crate) fn apply_authorized_trust(&self) {
        let new: Vec<String> = {
            let mut seen = self.paired_seen.lock().expect("paired seen lock");
            let now = self.paired_ids();
            let new = now.difference(&seen).cloned().collect();
            *seen = now;
            new
        };
        for p in self.config().pairing.authorized.iter().filter(|p| p.trust) {
            let id = p.endpoint_id.to_lowercase();
            if !new.contains(&id) {
                continue;
            }
            if let Err(e) = self.node().trust_paired(&id, true) {
                tracing::warn!(peer = %id, error = %e, "apply authorized trust failed");
            }
        }
    }

    /// Handler for `device-paired`; never blocks the event loop.
    pub(crate) fn on_device_paired(self: &Arc<Self>) {
        let svc = self.clone();
        tokio::task::spawn_blocking(move || svc.apply_authorized_trust());
    }

    /// Handler for `device-unpaired` (remote unpair); never blocks the event loop.
    pub(crate) fn on_device_unpaired(self: &Arc<Self>) {
        let svc = self.clone();
        tokio::task::spawn_blocking(move || svc.prune_paired_seen());
    }

    /// This node's pairing code, for the other side's `join`.
    pub fn pairing_code(&self) -> Result<String> {
        self.node().pairing_ticket()
    }

    /// Opens the pairing host (optionally for `ttl_secs`) and returns the code.
    pub async fn host_pairing(&self, ttl_secs: Option<u64>) -> Result<String> {
        self.node().start_pairing_host(ttl_secs).await
    }

    /// Pairs with the host that issued `code`.
    pub async fn join(&self, code: &str) -> Result<()> {
        self.node().join_pairing(code.trim()).await?;
        self.apply_authorized_trust();
        Ok(())
    }

    /// Sends a pair request to a nearby device; `false` when it wasn't delivered.
    pub async fn pair_nearby(&self, query: &str) -> Result<bool> {
        let nearby = self.list_nearby().await;
        let entries: Vec<(&str, &str)> = nearby
            .iter()
            .map(|d| {
                (
                    d.endpoint_id.as_str(),
                    d.display_name.as_deref().unwrap_or(""),
                )
            })
            .collect();
        let i = resolve_index(&entries, query).map_err(|e| match e {
            ResolveError::NotFound { query } => {
                anyhow!("no nearby device matches \"{query}\"")
            }
            other => anyhow!(other.to_string().replace("paired", "nearby")),
        })?;
        let id = nearby[i].endpoint_id.clone();
        self.node().request_nearby_pair(&id).await
    }

    pub fn trust(&self, query: &str, trusted: bool) -> Result<PairedDeviceInfo> {
        let d = self.resolve_paired(query)?;
        self.node().trust_paired(&d.endpoint_id, trusted)
    }

    /// Unpairs a device. Removing it from `pairing.authorized` never unpairs.
    pub async fn forget(&self, query: &str) -> Result<()> {
        let d = self.resolve_paired(query)?;
        self.node().forget_paired(&d.endpoint_id).await?;
        self.prune_paired_seen();
        Ok(())
    }

    /// `pair_nearby` for agent callers; gated by `--dangerously-allow-agent-pairing`.
    pub async fn agent_pair_nearby(&self, query: &str) -> Result<bool> {
        self.require_agent_pairing()?;
        self.pair_nearby(query).await
    }

    /// `trust` for agent callers; gated by `--dangerously-allow-agent-pairing`.
    pub async fn agent_trust(&self, query: &str, trusted: bool) -> Result<PairedDeviceInfo> {
        self.require_agent_pairing()?;
        self.trust(query, trusted)
    }

    fn require_agent_pairing(&self) -> Result<()> {
        if !self.allow_agent_pairing() {
            bail!(AGENT_PAIRING_DISABLED);
        }
        Ok(())
    }

    fn resolve_paired(&self, query: &str) -> Result<PairedDeviceInfo> {
        Ok(resolve_device(&self.list_devices()?, query)?)
    }

    fn is_paired(&self, endpoint_id: &str) -> bool {
        self.list_devices().is_ok_and(|ds| {
            ds.iter()
                .any(|d| d.endpoint_id.eq_ignore_ascii_case(endpoint_id))
        })
    }
}

/// Joins one authorized peer with exponential backoff until paired or shutdown.
async fn join_authorized(
    svc: std::sync::Weak<AgentService>,
    endpoint_id: String,
    relay_url: Option<String>,
    mut stop: watch::Receiver<bool>,
) {
    let code = PairingTicket {
        v: 1,
        kind: PairingTicket::KIND.to_string(),
        endpoint_id: endpoint_id.clone(),
        relay_url,
    }
    .encode();
    let code = match code {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(peer = %endpoint_id, error = %e, "invalid authorized peer; not pairing");
            return;
        }
    };
    let mut delay = BACKOFF_START;
    loop {
        {
            let Some(svc) = svc.upgrade() else { return };
            // The other side may have paired with us in the meantime.
            if svc.is_paired(&endpoint_id) {
                svc.apply_authorized_trust();
                return;
            }
            let attempt = tokio::select! {
                _ = stop.changed() => return,
                r = svc.node().join_pairing(&code) => r,
            };
            match attempt {
                Ok(()) => {
                    svc.apply_authorized_trust();
                    return;
                }
                Err(e) => {
                    tracing::warn!(peer = %endpoint_id, error = %e, "authorized pairing attempt failed")
                }
            }
        }
        tokio::select! {
            _ = stop.changed() => return,
            _ = tokio::time::sleep(delay) => {}
        }
        delay = (delay * 2).min(BACKOFF_CAP);
    }
}
