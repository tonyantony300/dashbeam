use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverabilitySetting {
    Everyone,
    #[default]
    PairedOnly,
    Off,
}

impl DiscoverabilitySetting {
    pub fn to_engine(self) -> engine::Discoverability {
        match self {
            Self::Everyone => engine::Discoverability::Everyone,
            Self::PairedOnly => engine::Discoverability::PairedOnly,
            Self::Off => engine::Discoverability::Off,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizedPeer {
    pub endpoint_id: String,
    #[serde(default)]
    pub relay_url: Option<String>,
    #[serde(default)]
    pub trust: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PairingConfig {
    pub authorized: Vec<AuthorizedPeer>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    pub name: Option<String>,
    pub receive_dir: Option<PathBuf>,
    pub discoverability: DiscoverabilitySetting,
    pub relay: Option<engine::RelayConfigArg>,
    pub discovery: Option<engine::DiscoveryConfigArg>,
    pub invite_timeout_secs: u64,
    pub ticket_ttl_secs: u64,
    pub max_ticket_receive_bytes: Option<u64>,
    pub pairing: PairingConfig,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            name: None,
            receive_dir: None,
            discoverability: DiscoverabilitySetting::default(),
            relay: None,
            discovery: None,
            invite_timeout_secs: 600,
            ticket_ttl_secs: 3600,
            max_ticket_receive_bytes: None,
            pairing: PairingConfig::default(),
        }
    }
}

impl AgentConfig {
    /// Load `<data_dir>/config.toml`; a missing file yields defaults.
    pub fn load(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join("config.toml");
        match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parse {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    /// A relative `receive_dir` is relative to the data dir, never the
    /// process cwd (which is `/` for an auto-started node).
    pub fn receive_dir(&self, data_dir: &Path) -> PathBuf {
        match &self.receive_dir {
            Some(dir) => data_dir.join(dir),
            None => data_dir.join("received"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_when_missing() {
        let d = tempfile::tempdir().unwrap();
        let c = AgentConfig::load(d.path()).unwrap();
        assert_eq!(c.invite_timeout_secs, 600);
        assert_eq!(c.ticket_ttl_secs, 3600);
        assert_eq!(c.discoverability, DiscoverabilitySetting::PairedOnly);
        assert_eq!(c.max_ticket_receive_bytes, None);
        assert_eq!(c.receive_dir(d.path()), d.path().join("received"));
    }

    #[test]
    fn config_rejects_dangerous_key() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("config.toml"),
            "dangerously_allow_agent_pairing = true\n",
        )
        .unwrap();
        assert!(AgentConfig::load(d.path()).is_err());
    }

    #[test]
    fn config_parses_authorized_peers() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("config.toml"),
            r#"
name = "Box"
discoverability = "off"
receive_dir = "/tmp/rx"

[[pairing.authorized]]
endpoint_id = "aaa"
trust = true

[[pairing.authorized]]
endpoint_id = "bbb"
relay_url = "https://relay.example"
"#,
        )
        .unwrap();
        let c = AgentConfig::load(d.path()).unwrap();
        assert_eq!(c.pairing.authorized.len(), 2);
        assert!(c.pairing.authorized[0].trust);
        assert!(!c.pairing.authorized[1].trust);
        assert_eq!(c.discoverability, DiscoverabilitySetting::Off);
        assert_eq!(c.receive_dir(d.path()), d.path().join("/tmp/rx"));
    }

    #[test]
    fn relative_receive_dir_is_under_data_dir() {
        let c = AgentConfig {
            receive_dir: Some(PathBuf::from("inbox/files")),
            ..AgentConfig::default()
        };
        let data = Path::new("/data/agent");
        assert_eq!(c.receive_dir(data), data.join("inbox/files"));
    }
}
