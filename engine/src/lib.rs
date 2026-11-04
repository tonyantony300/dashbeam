//! # `engine` — stable public API for Tauri and integration tests
//!
//! ## Canonical imports
//!
//! ```ignore
//! use engine::{
//!     download, fetch_metadata, get_or_create_secret, start_share_items,
//!     FileMetadata, ReceiveOptions, SendOptions, SendResult,
//! };
//! ```
//!
//! Desktop/mobile builds use the `native` platform crate (re-exported here).
//! Browser transfer logic lives in `wasm-io` and is reached via the root-level
//! `wasm-bridge` crate, not through this facade.
//!
//! Workspace layout: `protocol` (shared P2P logic) · `native` (disk I/O) · `wasm-io` (memory I/O).

#[cfg(not(target_arch = "wasm32"))]
pub use native::*;

#[cfg(target_arch = "wasm32")]
pub use wasm_io::*;

pub use protocol::identity::{default_display_name, unix_now_ms};
/// Shared protocol helpers not re-exported by the platform crates.
pub use protocol::{
    allows_unpaired_control, build_relay_mode, download_to_store, get_relay_status,
    pairing_host_is_persistent, relay_fallback_policy, resolve_relay_mode_with_fallback,
    run_share_session, sanitize_folder_name, should_answer_identity, should_publish_mdns,
    should_run_background_presence, sign_challenge, unpaired_message_allowed, verify_challenge,
    verify_relays, ControlMessage, Discoverability, DownloadToStoreResult, PairedDevice,
    PairingStatus, PairingTicket, RelayConfigArg, RelayFallbackPolicy, RelayStatusResponse,
    ShareSessionOutcome, VerifyRelaysResponse, METADATA_ALPN,
};
