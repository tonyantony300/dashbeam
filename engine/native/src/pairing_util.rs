use std::collections::HashMap;
use std::sync::Arc;

use iroh::endpoint::Endpoint;
use iroh::{EndpointAddr, EndpointId, TransportAddr};
use protocol::{apply_options, AddrInfoOptions, AppHandle};

use crate::device_identity::PairedDeviceStore;

pub fn build_control_connect_addr(
    endpoint: &Endpoint,
    remote: EndpointId,
    stored_relay: Option<&str>,
) -> EndpointAddr {
    let mut addr = EndpointAddr::from(remote);
    if let Some(relay) = stored_relay {
        if let Ok(url) = relay.parse() {
            addr.addrs.insert(TransportAddr::Relay(url));
        }
    }
    let mut local = endpoint.addr();
    apply_options(&mut local, AddrInfoOptions::Relay);
    if let Some(relay) = local.relay_urls().next() {
        addr.addrs.insert(TransportAddr::Relay(relay.clone()));
    }

    addr
}

/// Emit `paired-invite-received` to the frontend (shared by inbound and outbound readers).
pub fn emit_paired_invite_received(
    app_handle: &AppHandle,
    remote_endpoint_id: &str,
    blob_ticket: &str,
    file_count: u32,
    total_size: u64,
    sender_name: &str,
) {
    let payload = serde_json::json!({
        "blob_ticket": blob_ticket,
        "file_count": file_count,
        "total_size": total_size,
        "sender_name": sender_name,
        "remote_endpoint_id": remote_endpoint_id,
    });

    let Some(handle) = app_handle else {
        return;
    };
    let _ = handle.emit_event_with_payload("paired-invite-received", &payload.to_string());
}

/// Emit `paired-text-received` for a `Text` from a paired peer.
pub fn emit_paired_text_received(
    app_handle: &AppHandle,
    remote_endpoint_id: &str,
    sender_name: &str,
    body: &str,
) {
    let payload = serde_json::json!({
        "remote_endpoint_id": remote_endpoint_id,
        "sender_name": sender_name,
        "body": body,
    });

    let Some(handle) = app_handle else {
        return;
    };
    let _ = handle.emit_event_with_payload("paired-text-received", &payload.to_string());
}

/// Emit `device-paired` with the peer's name. The payload is additive —
/// existing listeners ignore it — and lets the shell name the device in an
/// OS notification instead of saying "a device".
pub fn emit_device_paired(app_handle: &AppHandle, display_name: &str) {
    let payload = serde_json::json!({ "display_name": display_name });

    let Some(handle) = app_handle else {
        return;
    };
    let _ = handle.emit_event_with_payload("device-paired", &payload.to_string());
}

/// Emit `nearby-pair-request-received` when an unpaired peer asks to pair (no files).
pub fn emit_nearby_pair_request(
    app_handle: &AppHandle,
    remote_endpoint_id: &str,
    sender_name: &str,
    device_type: &str,
    os: &str,
) {
    let payload = serde_json::json!({
        "remote_endpoint_id": remote_endpoint_id,
        "sender_name": sender_name,
        "device_type": device_type,
        "os": os,
    });

    let Some(handle) = app_handle else {
        return;
    };
    let _ = handle.emit_event_with_payload("nearby-pair-request-received", &payload.to_string());
}

/// Emit `paired-invite-response` so the sender UI can toast accept/decline.
///
/// `fallback_name` covers the Nearby decline case: a peer that declines never
/// becomes a `PairedDevice`, so `paired_store` has no name to resolve.
pub fn emit_paired_invite_response(
    app_handle: &AppHandle,
    paired_store: &PairedDeviceStore,
    remote_endpoint_id: &str,
    response: &str,
    fallback_name: Option<String>,
) {
    let display_name = paired_store
        .get(remote_endpoint_id)
        .ok()
        .flatten()
        .map(|d| d.display_name)
        .or(fallback_name);
    let payload = serde_json::json!({
        "endpoint_id": remote_endpoint_id,
        "display_name": display_name,
        "response": response,
    });

    let Some(handle) = app_handle else {
        return;
    };
    let _ = handle.emit_event_with_payload("paired-invite-response", &payload.to_string());
}

pub fn set_presence(
    presence: &Arc<std::sync::RwLock<HashMap<String, bool>>>,
    app_handle: &AppHandle,
    paired_store: &PairedDeviceStore,
    endpoint_id: &str,
    online: bool,
) {
    let changed = {
        let mut map = presence.write().expect("presence lock");
        let key = endpoint_id.to_lowercase();
        let prev = map.get(&key).copied();
        let changed = prev != Some(online);
        if changed {
            map.insert(key, online);
        }
        changed
    };

    if !changed {
        return;
    }
    let last_seen_at = paired_store
        .get(endpoint_id)
        .ok()
        .flatten()
        .map(|d| d.last_seen_at)
        .unwrap_or(0);
    let payload = serde_json::json!({
        "endpoint_id": endpoint_id,
        "online": online,
        "last_seen_at": last_seen_at,
    });

    if let Some(handle) = app_handle {
        let _ = handle.emit_event_with_payload("paired-device-presence", &payload.to_string());
    }
}
