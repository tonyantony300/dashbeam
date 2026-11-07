pub mod control;
pub mod discovery;
pub mod identity;
pub mod nearby;
pub mod pairing;
pub mod pairing_auth;
pub mod progress;
pub mod receive;
pub mod relay;
pub mod send;
pub mod time_compat;
pub mod tls_config;
pub mod types;

pub use control::{
    ControlMessage, InviteResponse, PairingTicket, RememberVote, CONTROL_ALPN, MAX_TEXT_BYTES,
};
pub use identity::{
    default_device_type, default_display_name, detect_os, device_type_from_chassis,
    device_type_from_mac_model, is_placeholder_display_name, normalize_display_name,
    sanitize_folder_name, DeviceMetaFile, PairedDevice, PairedDeviceList, PairingStatus,
};
pub use pairing::{
    pairing_host_is_persistent, PAIRING_VOTE_TIMEOUT_SECS, PAIRED_INVITE_WAIT_SECS,
    PAIRED_RECONNECT_MAX_SECS, PAIRED_RECONNECT_MIN_SECS, PRESENCE_CONNECT_TIMEOUT_SECS,
    RECENT_PAIRING_GRACE_MS, SETTLING_PAIRING_RETRY_SECS,
};
pub use pairing_auth::{export_connection_keying_material, sign_challenge, verify_challenge};
pub use nearby::{
    allows_unpaired_control, should_answer_identity, should_publish_mdns,
    should_run_background_presence, unpaired_message_allowed, Discoverability,
};
pub use progress::duration_ms;
pub use receive::{download_to_store, fetch_metadata, DownloadToStoreResult};
pub use relay::{
    build_relay_mode, get_relay_status, relay_fallback_policy, resolve_relay_mode_with_fallback,
    verify_relays, RelayConfigArg, RelayFallbackPolicy, RelayStatusResponse, VerifyRelaysResponse,
};
pub use discovery::{
    build_discovery_mode, parse_dns_origin, parse_pkarr_relay_url, DiscoveryConfigArg,
    VerifyDiscoveryResponse,
};
#[cfg(not(target_arch = "wasm32"))]
pub use discovery::verify_discovery;
pub use tls_config::{uses_custom_infra, with_system_ca_if_custom};
pub use control::{read_message, write_message};
pub use send::{run_share_on_endpoint, run_share_session, MetadataProtocol, ShareSessionOutcome, METADATA_ALPN};
pub use types::*;

#[cfg(target_arch = "wasm32")]
pub use types::set_wasm_secret_key;
