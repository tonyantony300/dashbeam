use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

use anyhow::Context;
use data_encoding::HEXLOWER;
use iroh::SecretKey;
use protocol::{
    default_device_type, default_display_name, detect_os, is_placeholder_display_name,
    normalize_display_name, DeviceMetaFile, PairedDevice, PairedDeviceList, PairingStatus,
};
use serde::{Deserialize, Serialize};

use crate::identity_store;

#[derive(Debug)]
pub struct DeviceIdentity {
    pub secret_key: SecretKey,
    meta: RwLock<DeviceMetaFile>,
    meta_path: PathBuf,
    /// Set when keychain identity differed from device.json on load.
    pub identity_rotated: bool,
    pub previous_endpoint_id: Option<String>,
}

impl DeviceIdentity {
    pub fn endpoint_id(&self) -> String {
        HEXLOWER.encode(self.secret_key.public().as_bytes())
    }

    pub fn display_name(&self) -> String {
        self.meta
            .read()
            .expect("device meta lock")
            .display_name
            .clone()
    }

    pub fn device_type(&self) -> String {
        self.meta
            .read()
            .expect("device meta lock")
            .device_type
            .clone()
    }

    pub fn os(&self) -> String {
        self.meta.read().expect("device meta lock").os.clone()
    }

    pub fn set_display_name(&self, name: &str) -> anyhow::Result<DeviceInfo> {
        let normalized = normalize_display_name(name).map_err(anyhow::Error::msg)?;
        {
            let mut meta = self.meta.write().expect("device meta lock");
            meta.display_name = normalized;
            meta.name_is_custom = true;
            meta.migrate();
            Self::write_meta(&self.meta_path, &meta)?;
        }
        Ok(DeviceInfo::from(self))
    }
}

pub struct PairedDeviceStore {
    path: PathBuf,
    /// Serializes all read-modify-write cycles so concurrent callers don't
    /// race on the shared `.tmp` rename path.
    file_lock: Mutex<()>,
}

impl PairedDeviceStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("paired-devices.json"),
            file_lock: Mutex::new(()),
        }
    }

    /// The guard protects no in-memory state, so poisoning would only turn a
    /// one-off panic into a permanently unusable store.
    fn lock_file(&self) -> std::sync::MutexGuard<'_, ()> {
        self.file_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn list(&self) -> anyhow::Result<Vec<PairedDevice>> {
        let file = self.read_file()?;
        Ok(file.devices)
    }

    pub fn get(&self, endpoint_id: &str) -> anyhow::Result<Option<PairedDevice>> {
        let id = endpoint_id.to_lowercase();
        Ok(self
            .list()?
            .into_iter()
            .find(|d| d.endpoint_id.to_lowercase() == id))
    }

    pub fn remember(&self, device: PairedDevice) -> anyhow::Result<PairedDevice> {
        let _guard = self.lock_file();
        let mut file = self.read_file()?;
        let id = device.endpoint_id.to_lowercase();
        if let Some(existing) = file
            .devices
            .iter_mut()
            .find(|d| d.endpoint_id.to_lowercase() == id)
        {
            existing.display_name = device.display_name;
            existing.device_type = device.device_type;
            existing.os = device.os;
            existing.last_seen_at = device.last_seen_at;
            if device.relay_url.is_some() {
                existing.relay_url = device.relay_url;
            }
            existing.pairing_status = PairingStatus::Active;
            let saved = existing.clone();
            self.write_file(&file)?;

            return Ok(saved);
        }
        file.devices.push(device.clone());
        self.write_file(&file)?;

        Ok(device)
    }

    pub fn rename(&self, endpoint_id: &str, display_name: &str) -> anyhow::Result<PairedDevice> {
        let normalized = normalize_display_name(display_name).map_err(anyhow::Error::msg)?;
        let _guard = self.lock_file();
        let mut file = self.read_file()?;
        let id = endpoint_id.to_lowercase();
        let existing = file
            .devices
            .iter_mut()
            .find(|d| d.endpoint_id.to_lowercase() == id)
            .context("paired device not found")?;
        existing.display_name = normalized;
        let saved = existing.clone();
        self.write_file(&file)?;
        Ok(saved)
    }

    pub fn trust_paired_device(
        &self,
        endpoint_id: &str,
        trust: bool,
    ) -> anyhow::Result<PairedDevice> {
        let _guard = self.lock_file();
        let mut file = self.read_file()?;
        let id = endpoint_id.to_lowercase();
        let existing = file
            .devices
            .iter_mut()
            .find(|d| d.endpoint_id.to_lowercase() == id)
            .context("paired device not found")?;
        existing.trusted = trust;
        let saved = existing.clone();
        self.write_file(&file)?;
        Ok(saved)
    }

    pub fn forget(&self, endpoint_id: &str) -> anyhow::Result<()> {
        let _guard = self.lock_file();
        let mut file = self.read_file()?;
        let id = endpoint_id.to_lowercase();
        file.devices
            .retain(|d| d.endpoint_id.to_lowercase() != id);
        self.write_file(&file)?;
        Ok(())
    }

    pub fn touch(&self, endpoint_id: &str, last_seen_at: u64) -> anyhow::Result<()> {
        let _guard = self.lock_file();
        let mut file = self.read_file()?;
        let id = endpoint_id.to_lowercase();
        if let Some(existing) = file
            .devices
            .iter_mut()
            .find(|d| d.endpoint_id.to_lowercase() == id)
        {
            existing.last_seen_at = last_seen_at;
            self.write_file(&file)?;
        }
        Ok(())
    }

    pub fn mark_unpaired_remotely(&self, endpoint_id: &str) -> anyhow::Result<Option<PairedDevice>> {
        let _guard = self.lock_file();
        let mut file = self.read_file()?;
        let id = endpoint_id.to_lowercase();
        let saved = if let Some(existing) = file
            .devices
            .iter_mut()
            .find(|d| d.endpoint_id.to_lowercase() == id)
        {
            existing.pairing_status = PairingStatus::UnpairedRemotely;
            Some(existing.clone())
        } else {
            None
        };
        if saved.is_some() {
            self.write_file(&file)?;
        }
        Ok(saved)
    }

    pub fn mark_stale_after_local_identity_rotation(&self) -> anyhow::Result<usize> {
        let _guard = self.lock_file();
        let mut file = self.read_file()?;
        let mut updated = 0usize;
        for device in &mut file.devices {
            if device.pairing_status == PairingStatus::Active {
                device.pairing_status = PairingStatus::StaleLocalIdentity;
                updated += 1;
            }
        }
        if updated > 0 {
            self.write_file(&file)?;

        }
        Ok(updated)
    }

    fn read_file(&self) -> anyhow::Result<PairedDeviceList> {
        if !self.path.exists() {
            return Ok(PairedDeviceList::default());
        }
        let raw = std::fs::read_to_string(&self.path)
            .with_context(|| format!("read {}", self.path.display()))?;
        Ok(serde_json::from_str(&raw).unwrap_or_default())
    }

    fn write_file(&self, list: &PairedDeviceList) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(list)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

impl DeviceIdentity {
    fn write_meta(path: &Path, meta: &DeviceMetaFile) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(meta)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// Identity loading knobs. Default matches the desktop app's behavior.
#[derive(Debug, Clone)]
pub struct IdentityOptions {
    pub use_keychain: bool,
    pub honor_env_secret: bool,
    /// Pinned device type; `None` auto-detects.
    pub device_type: Option<String>,
    /// Display name for new/placeholder metadata; `None` uses the host name.
    pub default_display_name: Option<String>,
}

impl Default for IdentityOptions {
    fn default() -> Self {
        Self {
            use_keychain: true,
            honor_env_secret: true,
            device_type: None,
            default_display_name: None,
        }
    }
}

pub fn load_or_create_identity(data_dir: &Path) -> anyhow::Result<DeviceIdentity> {
    load_or_create_identity_with(data_dir, &IdentityOptions::default())
}

pub fn load_or_create_identity_with(
    data_dir: &Path,
    opts: &IdentityOptions,
) -> anyhow::Result<DeviceIdentity> {
    std::fs::create_dir_all(data_dir)?;
    let meta_path = data_dir.join("device.json");
    let fresh_name = || {
        opts.default_display_name
            .clone()
            .unwrap_or_else(default_display_name)
    };

    let outcome = identity_store::load_or_create_secret_with(
        data_dir,
        opts.use_keychain,
        opts.honor_env_secret,
    )?;
    let secret = outcome.secret;
    // Node identity is only for the persistent control endpoint. Share/receive
    // sessions use ephemeral keys via get_or_create_secret() so they do not
    // collide with this endpoint on the relay.
    let endpoint_id = HEXLOWER.encode(secret.public().as_bytes());

    if matches!(
        outcome.source,
        identity_store::SecretSource::KeychainMigrated | identity_store::SecretSource::Generated
    ) {

    }

    let mut identity_rotated = false;
    let mut previous_endpoint_id = None;
    let mut meta = if meta_path.exists() {
        let raw = std::fs::read_to_string(&meta_path)?;
        let mut meta: DeviceMetaFile =
            serde_json::from_str(&raw).context("invalid device.json")?;
        if meta.endpoint_id.to_lowercase() != endpoint_id {
            identity_rotated = true;
            previous_endpoint_id = Some(meta.endpoint_id.clone());
            tracing::warn!("device.json endpoint_id mismatch; syncing to persisted identity");


            meta.endpoint_id = endpoint_id;
        }
        meta.migrate();
        // Refresh auto-detected hostname only when the user has not renamed.
        if !meta.name_is_custom {
            let detected = fresh_name();
            if is_placeholder_display_name(&meta.display_name) {
                meta.display_name = detected;
            }
        }
        if meta.os.trim().is_empty() {
            meta.os = detect_os();
        }
        // Re-detect form factor on load (not user-editable yet) so older
        // Linux/Windows installs that were hard-coded as "desktop" self-heal.
        meta.device_type = opts
            .device_type
            .clone()
            .unwrap_or_else(default_device_type);
        meta
    } else {
        DeviceMetaFile::new(
            endpoint_id,
            fresh_name(),
            opts.device_type.clone().unwrap_or_else(default_device_type),
        )
    };

    meta.migrate();
    DeviceIdentity::write_meta(&meta_path, &meta)?;

    Ok(DeviceIdentity {
        secret_key: secret,
        meta: RwLock::new(meta),
        meta_path,
        identity_rotated,
        previous_endpoint_id,
    })
}

/// Serializable paired device for the frontend, including ephemeral presence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairedDeviceInfo {
    pub endpoint_id: String,
    pub display_name: String,
    pub device_type: String,
    pub os: String,
    pub paired_at: u64,
    pub last_seen_at: u64,
    #[serde(default)]
    pub relay_url: Option<String>,
    #[serde(default)]
    pub pairing_status: PairingStatus,
    #[serde(default)]
    pub trusted: bool,
    pub online: bool,
}

impl PairedDeviceInfo {
    pub fn from_device(device: PairedDevice, online: bool) -> Self {
        Self {
            endpoint_id: device.endpoint_id,
            display_name: device.display_name,
            device_type: device.device_type,
            os: device.os,
            paired_at: device.paired_at,
            last_seen_at: device.last_seen_at,
            relay_url: device.relay_url,
            pairing_status: device.pairing_status,
            trusted: device.trusted,
            online,
        }
    }
}

/// Online/total counts across *actively* paired devices, for the tray summary.
/// Peers that unpaired remotely — or whose local identity went stale — are
/// excluded from both counts: the tray must not advertise a device the user
/// cannot actually reach.
pub fn presence_summary(devices: &[PairedDeviceInfo]) -> (usize, usize) {
    let active = devices.iter().filter(|d| d.pairing_status.is_active());
    let total = active.clone().count();
    let online = active.filter(|d| d.online).count();
    (online, total)
}

/// Serializable device info for the frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub endpoint_id: String,
    pub display_name: String,
    pub device_type: String,
    pub os: String,
}

impl From<&DeviceIdentity> for DeviceInfo {
    fn from(id: &DeviceIdentity) -> Self {
        Self {
            endpoint_id: id.endpoint_id(),
            display_name: id.display_name(),
            device_type: id.device_type(),
            os: id.os(),
        }
    }
}

#[cfg(test)]
mod presence_summary_tests {
    use super::*;
    use protocol::identity::PairingStatus;

    fn info(endpoint_id: &str, online: bool, status: PairingStatus) -> PairedDeviceInfo {
        PairedDeviceInfo {
            endpoint_id: endpoint_id.to_string(),
            display_name: "Device".to_string(),
            device_type: "laptop".to_string(),
            os: "macos".to_string(),
            paired_at: 0,
            last_seen_at: 0,
            relay_url: None,
            pairing_status: status,
            trusted: false,
            online,
        }
    }

    #[test]
    fn counts_only_actively_paired_devices() {
        let devices = vec![
            info("a", true, PairingStatus::Active),
            info("b", false, PairingStatus::Active),
            info("c", true, PairingStatus::Active),
        ];
        assert_eq!(presence_summary(&devices), (2, 3));
    }

    #[test]
    fn excludes_inactive_pairings_from_both_counts() {
        // An unpaired-remotely peer must not be advertised in the tray, even
        // if a stale presence entry still says it is online.
        let devices = vec![
            info("a", true, PairingStatus::Active),
            info("b", true, PairingStatus::UnpairedRemotely),
            info("c", false, PairingStatus::StaleLocalIdentity),
        ];
        assert_eq!(presence_summary(&devices), (1, 1));
    }

    #[test]
    fn empty_list_is_zero_of_zero() {
        assert_eq!(presence_summary(&[]), (0, 0));
    }
}

#[cfg(test)]
mod trust_flag_tests {
    use super::{PairedDeviceInfo, PairedDeviceStore};
    use protocol::identity::{PairedDevice, PairingStatus};

    fn device(endpoint_id: &str) -> PairedDevice {
        PairedDevice {
            endpoint_id: endpoint_id.to_string(),
            display_name: "Studio Mac".to_string(),
            device_type: "desktop".to_string(),
            os: "macos".to_string(),
            paired_at: 1,
            last_seen_at: 1,
            relay_url: None,
            pairing_status: PairingStatus::Active,
            trusted: false,
        }
    }

    #[test]
    fn trust_persists_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = PairedDeviceStore::new(dir.path());
        store.remember(device("aaaa")).unwrap();

        let saved = store.trust_paired_device("aaaa", true).unwrap();
        assert!(saved.trusted);

        // A fresh store reads the same file back from disk.
        let reopened = PairedDeviceStore::new(dir.path());
        assert!(reopened.get("aaaa").unwrap().unwrap().trusted);
    }

    #[test]
    fn trusting_an_unknown_device_errors() {
        let dir = tempfile::tempdir().unwrap();
        let store = PairedDeviceStore::new(dir.path());
        assert!(store.trust_paired_device("missing", true).is_err());
    }

    #[test]
    fn remember_preserves_trust_on_an_existing_row() {
        let dir = tempfile::tempdir().unwrap();
        let store = PairedDeviceStore::new(dir.path());
        store.remember(device("aaaa")).unwrap();
        store.trust_paired_device("aaaa", true).unwrap();

        // Re-pairing or a presence refresh must not silently untrust.
        let mut refreshed = device("aaaa");
        refreshed.display_name = "Renamed".to_string();
        refreshed.last_seen_at = 99;
        store.remember(refreshed).unwrap();

        let stored = store.get("aaaa").unwrap().unwrap();
        assert!(stored.trusted);
        assert_eq!(stored.display_name, "Renamed");
    }

    #[test]
    fn forget_drops_trust_with_the_pairing() {
        let dir = tempfile::tempdir().unwrap();
        let store = PairedDeviceStore::new(dir.path());
        store.remember(device("aaaa")).unwrap();
        store.trust_paired_device("aaaa", true).unwrap();

        store.forget("aaaa").unwrap();
        assert!(store.get("aaaa").unwrap().is_none());

        // Re-pairing the same peer starts untrusted.
        store.remember(device("aaaa")).unwrap();
        assert!(!store.get("aaaa").unwrap().unwrap().trusted);
    }

    #[test]
    fn legacy_file_without_the_field_reads_as_untrusted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("paired-devices.json"),
            r#"{"devices":[{"endpoint_id":"aaaa","display_name":"Old","device_type":"laptop","os":"linux","paired_at":1,"last_seen_at":2}]}"#,
        )
        .unwrap();

        let store = PairedDeviceStore::new(dir.path());
        let stored = store.get("aaaa").unwrap().unwrap();
        assert!(!stored.trusted);
        assert_eq!(stored.display_name, "Old");
    }

    #[test]
    fn paired_device_info_carries_trust_to_the_frontend() {
        let mut d = device("aaaa");
        d.trusted = true;
        assert!(PairedDeviceInfo::from_device(d, true).trusted);
    }
}

#[cfg(test)]
mod identity_options_tests {
    use super::*;

    #[test]
    fn pinned_device_type_and_default_name_survive_reload() {
        let dir = tempfile::tempdir().unwrap();
        let opts = IdentityOptions {
            use_keychain: false,
            honor_env_secret: false,
            device_type: Some("agent".into()),
            default_display_name: Some("Box Agent".into()),
        };
        let first = load_or_create_identity_with(dir.path(), &opts).unwrap();
        assert_eq!(first.device_type(), "agent");
        assert_eq!(first.display_name(), "Box Agent");
        let again = load_or_create_identity_with(dir.path(), &opts).unwrap();
        assert_eq!(again.device_type(), "agent");
        assert_eq!(again.endpoint_id(), first.endpoint_id());
    }
}
