//! Inbound invites to an `AgentService`. Binds real endpoints: run with `--test-threads=1`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent::approver::DenyAll;
use agent::config::{AgentConfig, AuthorizedPeer, DiscoverabilitySetting};
use agent::registry::{Direction, TransferRecord, TransferStatus};
use agent::service::{AgentOptions, AgentService};
use engine::{
    Discoverability, EventEmitter, IdentityOptions, NodeService, SendOptions, SendResult,
};

const DEADLINE: Duration = Duration::from_secs(90);

#[derive(Default)]
struct Capture {
    events: Mutex<Vec<(String, Option<String>)>>,
}

impl Capture {
    fn payloads(&self, name: &str) -> Vec<serde_json::Value> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|(n, _)| n == name)
            .filter_map(|(_, p)| p.as_deref().and_then(|p| serde_json::from_str(p).ok()))
            .collect()
    }
}

impl EventEmitter for Capture {
    fn emit_event(&self, name: &str) -> Result<(), String> {
        self.events.lock().unwrap().push((name.to_string(), None));
        Ok(())
    }

    fn emit_event_with_payload(&self, name: &str, payload: &str) -> Result<(), String> {
        self.events
            .lock()
            .unwrap()
            .push((name.to_string(), Some(payload.to_string())));
        Ok(())
    }
}

struct Sender {
    node: NodeService,
    events: Arc<Capture>,
    _dir: tempfile::TempDir,
}

impl Sender {
    fn id(&self) -> String {
        self.node.device_info().endpoint_id
    }
}

/// Raw keychain-free node standing in for the desktop app.
async fn spawn_sender(name: &str, discoverability: Discoverability) -> Sender {
    let dir = tempfile::tempdir().unwrap();
    let events = Arc::new(Capture::default());
    let node = NodeService::start_with(
        dir.path(),
        engine::build_relay_mode(None).unwrap().into(),
        engine::build_discovery_mode(None).unwrap(),
        discoverability,
        Some(events.clone()),
        IdentityOptions {
            use_keychain: false,
            honor_env_secret: false,
            device_type: None,
            default_display_name: None,
        },
    )
    .await
    .expect("sender start");
    node.set_device_display_name(name).unwrap();
    Sender {
        node,
        events,
        _dir: dir,
    }
}

async fn start_agent(dir: &Path, config: AgentConfig) -> Arc<AgentService> {
    AgentService::start(AgentOptions {
        data_dir: dir.to_path_buf(),
        config,
        allow_agent_pairing: false,
        discoverable_override: false,
        approver: Arc::new(DenyAll),
    })
    .await
    .expect("agent start")
}

fn authorizing(sender: &Sender) -> AgentConfig {
    let mut config = AgentConfig::default();
    config.pairing.authorized.push(AuthorizedPeer {
        endpoint_id: sender.id(),
        relay_url: None,
        trust: false,
    });
    config
}

/// Pairs via the pre-authorized path and optionally trusts the sender.
async fn pair(sender: &Sender, agent: &AgentService, trust: bool) {
    let ticket = agent.node().pairing_ticket().unwrap();
    let end = tokio::time::Instant::now() + DEADLINE;
    loop {
        match tokio::time::timeout(Duration::from_secs(30), sender.node.join_pairing(&ticket)).await
        {
            Ok(Ok(())) => break,
            other => assert!(
                tokio::time::Instant::now() < end,
                "join_pairing never succeeded: {other:?}"
            ),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let id = sender.id();
    wait_for("agent to store the sender", || {
        agent
            .list_devices()
            .unwrap()
            .iter()
            .any(|d| d.endpoint_id == id)
    })
    .await;
    if trust {
        agent.node().trust_paired(&id, true).unwrap();
    }
}

async fn wait_for(what: &str, check: impl Fn() -> bool) {
    let end = tokio::time::Instant::now() + DEADLINE;
    while !check() {
        assert!(tokio::time::Instant::now() < end, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn share(dir: &Path, name: &str, contents: &[u8]) -> SendResult {
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    engine::start_share_items(vec![path], SendOptions::default(), &None, None)
        .await
        .expect("share")
}

async fn invite(sender: &Sender, agent: &AgentService, ticket: &str, size: u64) {
    let to = agent.identity().endpoint_id;
    let end = tokio::time::Instant::now() + DEADLINE;
    loop {
        if let Ok(true) = sender.node.invite_paired_device(&to, ticket, 1, size).await {
            return;
        }
        assert!(tokio::time::Instant::now() < end, "invite never delivered");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn wait_single_inbound(agent: &AgentService) -> TransferRecord {
    wait_for("an inbound record", || {
        !agent
            .registry()
            .list(None, Some(Direction::Inbound))
            .is_empty()
    })
    .await;
    let recs = agent.registry().list(None, Some(Direction::Inbound));
    assert_eq!(recs.len(), 1, "{recs:?}");
    let (rec, running) = agent
        .registry()
        .wait_terminal(&recs[0].id, DEADLINE)
        .await
        .unwrap();
    assert!(!running, "transfer still running: {rec:?}");
    rec
}

fn receive_dir(agent: &AgentService) -> PathBuf {
    agent.config().receive_dir(agent.data_dir())
}

fn declined(sender: &Sender) -> bool {
    sender
        .events
        .payloads("paired-invite-response")
        .iter()
        .any(|p| p["response"] == "declined")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trusted_invite_is_received_under_sender_folder() {
    let sender = spawn_sender("Sender Box", Discoverability::PairedOnly).await;
    let agent_dir = tempfile::tempdir().unwrap();
    let agent = start_agent(agent_dir.path(), authorizing(&sender)).await;
    pair(&sender, &agent, true).await;

    let src = tempfile::tempdir().unwrap();
    let shared = share(src.path(), "hello.txt", b"hello agent").await;
    invite(&sender, &agent, &shared.ticket, 11).await;

    let rec = wait_single_inbound(&agent).await;
    assert_eq!(rec.status, TransferStatus::Received, "{rec:?}");
    let expected = receive_dir(&agent).join("Sender Box").join("hello.txt");
    assert_eq!(std::fs::read(&expected).unwrap(), b"hello agent");
    assert_eq!(rec.saved_paths, vec![expected]);
    assert_eq!(rec.file_names, vec!["hello.txt".to_string()]);
    assert_eq!(rec.bytes_done, 11);
    let peer = rec.peer.unwrap();
    assert_eq!(peer.endpoint_id.as_deref(), Some(sender.id().as_str()));
    assert!(agent.list_incoming().is_empty());

    drop(shared);
    agent.shutdown().await.unwrap();
    sender.node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn untrusted_invite_waits_and_accept_incoming_downloads() {
    let sender = spawn_sender("Laptop", Discoverability::PairedOnly).await;
    let agent_dir = tempfile::tempdir().unwrap();
    let agent = start_agent(agent_dir.path(), authorizing(&sender)).await;
    pair(&sender, &agent, false).await;

    let src = tempfile::tempdir().unwrap();
    let shared = share(src.path(), "notes.txt", b"pending").await;
    invite(&sender, &agent, &shared.ticket, 7).await;

    wait_for("a pending invite", || agent.list_incoming().len() == 1).await;
    // Held, not auto-accepted.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(agent
        .registry()
        .list(None, Some(Direction::Inbound))
        .is_empty());
    let pending = agent.list_incoming().remove(0);
    assert_eq!(pending.endpoint_id, sender.id());
    assert_eq!(pending.sender_name, "Laptop");
    assert_eq!(pending.file_count, 1);
    assert_eq!(pending.total_size, 7);

    let started = agent.accept_incoming(&pending.invite_id).await.unwrap();
    assert_eq!(started.status, TransferStatus::Transferring);
    assert!(agent.list_incoming().is_empty());
    assert!(agent.accept_incoming(&pending.invite_id).await.is_err());

    let rec = wait_single_inbound(&agent).await;
    assert_eq!(rec.id, started.id);
    assert_eq!(rec.status, TransferStatus::Received, "{rec:?}");
    let expected = receive_dir(&agent).join("Laptop").join("notes.txt");
    assert_eq!(std::fs::read(&expected).unwrap(), b"pending");
    assert_eq!(rec.saved_paths, vec![expected]);

    drop(shared);
    agent.shutdown().await.unwrap();
    sender.node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn untrusted_invite_decline_sends_declined() {
    let sender = spawn_sender("Laptop", Discoverability::PairedOnly).await;
    let agent_dir = tempfile::tempdir().unwrap();
    let agent = start_agent(agent_dir.path(), authorizing(&sender)).await;
    pair(&sender, &agent, false).await;

    let src = tempfile::tempdir().unwrap();
    let shared = share(src.path(), "notes.txt", b"nope").await;
    invite(&sender, &agent, &shared.ticket, 4).await;

    wait_for("a pending invite", || agent.list_incoming().len() == 1).await;
    let pending = agent.list_incoming().remove(0);
    agent.decline_incoming(&pending.invite_id).await.unwrap();
    assert!(agent.list_incoming().is_empty());
    assert!(agent.decline_incoming(&pending.invite_id).await.is_err());

    wait_for("the sender to see the decline", || declined(&sender)).await;
    assert!(agent
        .registry()
        .list(None, Some(Direction::Inbound))
        .is_empty());

    drop(shared);
    agent.shutdown().await.unwrap();
    sender.node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn untrusted_invite_expires_with_a_decline() {
    let sender = spawn_sender("Laptop", Discoverability::PairedOnly).await;
    let agent_dir = tempfile::tempdir().unwrap();
    let mut config = authorizing(&sender);
    config.invite_timeout_secs = 2;
    let agent = start_agent(agent_dir.path(), config).await;
    pair(&sender, &agent, false).await;

    let src = tempfile::tempdir().unwrap();
    let shared = share(src.path(), "notes.txt", b"late").await;
    invite(&sender, &agent, &shared.ticket, 4).await;

    wait_for("the sender to see the expiry decline", || declined(&sender)).await;
    assert!(agent.list_incoming().is_empty());

    drop(shared);
    agent.shutdown().await.unwrap();
    sender.node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hostile_sender_name_is_sanitized() {
    let sender = spawn_sender("../../etc", Discoverability::PairedOnly).await;
    let agent_dir = tempfile::tempdir().unwrap();
    let agent = start_agent(agent_dir.path(), authorizing(&sender)).await;
    pair(&sender, &agent, true).await;

    let src = tempfile::tempdir().unwrap();
    let shared = share(src.path(), "passwd", b"not really").await;
    invite(&sender, &agent, &shared.ticket, 10).await;

    let rec = wait_single_inbound(&agent).await;
    assert_eq!(rec.status, TransferStatus::Received, "{rec:?}");
    let root = std::fs::canonicalize(receive_dir(&agent)).unwrap();
    assert_eq!(rec.saved_paths.len(), 1);
    let saved = std::fs::canonicalize(&rec.saved_paths[0]).unwrap();
    assert!(saved.starts_with(&root), "{saved:?} escaped {root:?}");
    assert_eq!(saved.parent().unwrap().parent().unwrap(), root);
    assert_eq!(std::fs::read(&saved).unwrap(), b"not really");

    drop(shared);
    agent.shutdown().await.unwrap();
    sender.node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nearby_invite_declined_without_approval() {
    let sender = spawn_sender("Stranger", Discoverability::Everyone).await;
    let agent_dir = tempfile::tempdir().unwrap();
    let config = AgentConfig {
        discoverability: DiscoverabilitySetting::Everyone,
        ..AgentConfig::default()
    };
    let agent = start_agent(agent_dir.path(), config).await;
    let agent_id = agent.identity().endpoint_id;

    // Seeds Nearby without multicast, as the engine's nearby tests do.
    sender.node.inject_nearby_device_for_tests(&agent_id).await;
    let src = tempfile::tempdir().unwrap();
    let shared = share(src.path(), "hi.txt", b"hi").await;
    let delivered = sender
        .node
        .invite_nearby_device(&agent_id, &shared.ticket, 1, 2)
        .await
        .expect("nearby invite");
    assert!(delivered);

    wait_for("the sender to see the decline", || declined(&sender)).await;
    assert!(agent.list_devices().unwrap().is_empty());
    assert!(sender.node.list_paired().unwrap().is_empty());
    assert!(agent.list_incoming().is_empty());
    assert!(agent
        .registry()
        .list(None, Some(Direction::Inbound))
        .is_empty());

    drop(shared);
    agent.shutdown().await.unwrap();
    sender.node.shutdown().await.unwrap();
}
