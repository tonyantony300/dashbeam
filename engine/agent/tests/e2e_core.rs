//! AgentService startup E2E. Binds real endpoints: run with `--test-threads=1`.

use std::sync::Arc;

use agent::approver::DenyAll;
use agent::config::AgentConfig;
use agent::service::{AgentOptions, AgentService};
use engine::{Discoverability, IdentityOptions, NodeService};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_identity_is_agent_type_and_separate_from_app_dir() {
    let agent_dir = tempfile::tempdir().unwrap();
    let svc = AgentService::start(AgentOptions {
        data_dir: agent_dir.path().to_path_buf(),
        config: AgentConfig::default(),
        allow_agent_pairing: false,
        discoverable_override: false,
        approver: Arc::new(DenyAll),
    })
    .await
    .expect("agent start");

    let me = svc.identity();
    assert_eq!(me.device_type, "agent");
    assert!(me.display_name.ends_with(" Agent"), "{}", me.display_name);
    assert!(agent_dir.path().join("identity.key").exists());
    assert!(!svc.allow_agent_pairing());
    assert!(svc.list_devices().unwrap().is_empty());

    // A plain (non-agent) node in another dir gets its own identity. Keychain-free
    // so the test never touches the developer's real OS keychain.
    let other_dir = tempfile::tempdir().unwrap();
    let plain = NodeService::start_with(
        other_dir.path(),
        engine::build_relay_mode(None).unwrap().into(),
        engine::build_discovery_mode(None).unwrap(),
        Discoverability::PairedOnly,
        None,
        IdentityOptions {
            use_keychain: false,
            honor_env_secret: false,
            device_type: None,
            default_display_name: None,
        },
    )
    .await
    .expect("plain node start");
    assert_ne!(plain.device_info().endpoint_id, me.endpoint_id);
    assert_ne!(plain.device_info().device_type, "agent");

    plain.shutdown().await.unwrap();
    svc.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_applies_config_name_and_rename() {
    let dir = tempfile::tempdir().unwrap();
    let config = AgentConfig {
        name: Some("Build Box".into()),
        ..AgentConfig::default()
    };
    let svc = AgentService::start(AgentOptions {
        data_dir: dir.path().to_path_buf(),
        config,
        allow_agent_pairing: true,
        discoverable_override: false,
        approver: Arc::new(DenyAll),
    })
    .await
    .expect("agent start");
    assert_eq!(svc.identity().display_name, "Build Box");
    assert!(svc.allow_agent_pairing());
    assert_eq!(svc.rename("CI Runner").unwrap().display_name, "CI Runner");
    assert_eq!(svc.identity().display_name, "CI Runner");
    assert_eq!(
        svc.identity().endpoint_id,
        svc.node().device_info().endpoint_id
    );
    svc.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_start_rejects_invalid_authorized_peer() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = AgentConfig::default();
    config
        .pairing
        .authorized
        .push(agent::config::AuthorizedPeer {
            endpoint_id: "not-an-id".into(),
            relay_url: None,
            trust: false,
        });
    let res = AgentService::start(AgentOptions {
        data_dir: dir.path().to_path_buf(),
        config,
        allow_agent_pairing: false,
        discoverable_override: false,
        approver: Arc::new(DenyAll),
    })
    .await;
    assert!(res.is_err());
}
