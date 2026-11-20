use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[cfg(not(target_arch = "wasm32"))]
pub trait EventEmitter: Send + Sync {
    fn emit_event(&self, event_name: &str) -> Result<(), String>;
    fn emit_event_with_payload(&self, event_name: &str, payload: &str) -> Result<(), String>;
}

#[cfg(target_arch = "wasm32")]
pub trait EventEmitter {
    fn emit_event(&self, event_name: &str) -> Result<(), String>;
    fn emit_event_with_payload(&self, event_name: &str, payload: &str) -> Result<(), String>;
}

/// Optional callback surface for transfer progress (Tauri events or JS callbacks).
pub type AppHandle = Option<Arc<dyn EventEmitter>>;

#[derive(Debug, Default)]
pub struct SendOptions {
    pub relay_mode: RelayModeOption,
    pub discovery_mode: DiscoveryModeOption,
    pub ticket_type: AddrInfoOptions,
    pub magic_ipv4_addr: Option<std::net::SocketAddrV4>,
    pub magic_ipv6_addr: Option<std::net::SocketAddrV6>,
}

#[derive(Debug, Default)]
pub struct ReceiveOptions {
    pub output_dir: Option<std::path::PathBuf>,
    pub relay_mode: RelayModeOption,
    pub discovery_mode: DiscoveryModeOption,
    pub magic_ipv4_addr: Option<std::net::SocketAddrV4>,
    pub magic_ipv6_addr: Option<std::net::SocketAddrV6>,
}

#[derive(Clone, Debug)]
pub enum RelayModeOption {
    Disabled,
    Default,
    Custom {
        urls: Vec<iroh::RelayUrl>,
        auth_token: Option<String>,
    },
}

impl Default for RelayModeOption {
    fn default() -> Self {
        Self::Default
    }
}

impl From<RelayModeOption> for iroh::endpoint::RelayMode {
    fn from(value: RelayModeOption) -> Self {
        match value {
            RelayModeOption::Disabled => iroh::endpoint::RelayMode::Disabled,
            RelayModeOption::Default => iroh::endpoint::RelayMode::Default,
            RelayModeOption::Custom { urls, auth_token } => {
                let map = iroh::RelayMap::from_iter(urls);
                let map = match auth_token {
                    Some(token) if !token.is_empty() => map.with_auth_token(token),
                    _ => map,
                };
                iroh::endpoint::RelayMode::Custom(map)
            }
        }
    }
}

/// Discovery (pkarr) mode. `Default` keeps iroh's n0 discovery; `Custom` publishes
/// and resolves against a self-hosted pkarr relay over HTTPS. When `dns_origin`
/// is set, resolve also uses real DNS (`DnsAddressLookup`) under that origin.
#[derive(Clone, Debug)]
pub enum DiscoveryModeOption {
    Default,
    Custom {
        pkarr_relay_url: url::Url,
        dns_origin: Option<String>,
    },
}

impl Default for DiscoveryModeOption {
    fn default() -> Self {
        Self::Default
    }
}

#[cfg(test)]
mod relay_mode_tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn custom_relay_mode_builds_relay_map() {
        let url = iroh::RelayUrl::from_str("https://relay.example.com").unwrap();
        let mode = RelayModeOption::Custom {
            urls: vec![url],
            auth_token: None,
        };
        let relay_mode: iroh::endpoint::RelayMode = mode.into();
        assert!(matches!(relay_mode, iroh::endpoint::RelayMode::Custom(_)));
    }

    #[test]
    fn custom_relay_mode_with_auth_token() {
        let url = iroh::RelayUrl::from_str("https://relay.example.com").unwrap();
        let mode = RelayModeOption::Custom {
            urls: vec![url],
            auth_token: Some("secret-token".to_string()),
        };
        let relay_mode: iroh::endpoint::RelayMode = mode.into();
        assert!(matches!(relay_mode, iroh::endpoint::RelayMode::Custom(_)));
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilePreviewItem {
    pub file_name: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMetadata {
    pub file_name: String,
    pub item_count: u32,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<FilePreviewItem>>,
}

#[derive(
    Copy,
    Clone,
    PartialEq,
    Eq,
    Default,
    Debug,
    derive_more::Display,
    derive_more::FromStr,
    serde::Serialize,
    serde::Deserialize,
)]
pub enum AddrInfoOptions {
    #[default]
    Id,
    RelayAndAddresses,
    Relay,
    Addresses,
}

pub fn apply_options(addr: &mut iroh::EndpointAddr, opts: AddrInfoOptions) {
    use iroh::TransportAddr;
    match opts {
        AddrInfoOptions::Id => {
            addr.addrs.clear();
        }
        AddrInfoOptions::RelayAndAddresses => {}
        AddrInfoOptions::Relay => {
            addr.addrs
                .retain(|transport_addr| matches!(transport_addr, TransportAddr::Relay(_)));
        }
        AddrInfoOptions::Addresses => {
            addr.addrs
                .retain(|transport_addr| matches!(transport_addr, TransportAddr::Ip(_)));
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
use anyhow::Context;
#[cfg(not(target_arch = "wasm32"))]
use std::str::FromStr;

#[cfg(target_arch = "wasm32")]
mod wasm_secret {
    use std::sync::{Mutex, OnceLock};

    static SECRET: OnceLock<Mutex<Option<iroh::SecretKey>>> = OnceLock::new();

    pub fn get_or_create() -> anyhow::Result<iroh::SecretKey> {
        let slot = SECRET.get_or_init(|| Mutex::new(None));
        let mut guard = slot.lock().expect("wasm secret mutex poisoned");
        if let Some(key) = guard.as_ref() {
            return Ok(key.clone());
        }
        let key = iroh::SecretKey::generate();
        *guard = Some(key.clone());
        Ok(key)
    }

    pub fn set(key: iroh::SecretKey) {
        let slot = SECRET.get_or_init(|| Mutex::new(None));
        *slot.lock().expect("wasm secret mutex poisoned") = Some(key);
    }
}

pub fn get_or_create_secret() -> anyhow::Result<iroh::SecretKey> {
    #[cfg(target_arch = "wasm32")]
    {
        return wasm_secret::get_or_create();
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        match std::env::var("IROH_SECRET") {
            Ok(secret) => iroh::SecretKey::from_str(&secret).context("invalid secret"),
            Err(_) => Ok(iroh::SecretKey::generate()),
        }
    }
}

/// Persist the node secret key for browser sessions (survives page reload when set from JS).
#[cfg(target_arch = "wasm32")]
pub fn set_wasm_secret_key(key: iroh::SecretKey) {
    wasm_secret::set(key);
}

/// Key for a connectivity probe (relay/discovery "Test connection").
///
/// Never the configured identity: a probe under the same id would replace
/// that identity's pkarr record or kick its live relay connection.
pub fn probe_secret_key() -> iroh::SecretKey {
    iroh::SecretKey::generate()
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod probe_key_tests {
    use super::*;

    #[test]
    fn probe_key_ignores_configured_identity() {
        let configured = iroh::SecretKey::generate();
        let hex: String = configured
            .to_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        // Sanity: the configured key round-trips through the env var.
        std::env::set_var("IROH_SECRET", &hex);
        let via_env = get_or_create_secret().unwrap().public();
        let probe = probe_secret_key().public();
        std::env::remove_var("IROH_SECRET");

        assert_eq!(via_env, configured.public());
        assert_ne!(probe, configured.public());
    }
}
