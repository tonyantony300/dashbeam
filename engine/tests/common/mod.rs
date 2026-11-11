#![allow(dead_code, unused_imports)]

use engine::{
    DeviceInfo, Discoverability, DiscoveryModeOption, EventEmitter, NearbyDevice, NodeService,
};
use iroh::endpoint::RelayMode;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct MockEvent {
    pub name: String,
    pub payload: Option<String>,
}

#[derive(Debug, Default)]
pub struct MockEventEmitter {
    events: Mutex<Vec<MockEvent>>,
}

impl MockEventEmitter {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Returns a clone of all captured events.
    pub fn events(&self) -> Vec<MockEvent> {
        self.events.lock().unwrap().clone()
    }

    /// Returns true if any event with the given name was emitted.
    pub fn has_event(&self, name: &str) -> bool {
        self.events.lock().unwrap().iter().any(|e| e.name == name)
    }

    /// Returns all event names in order.
    pub fn event_names(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.name.clone())
            .collect()
    }

    /// Returns all events matching the given name.
    pub fn events_with_name(&self, name: &str) -> Vec<MockEvent> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.name == name)
            .cloned()
            .collect()
    }
}

impl EventEmitter for MockEventEmitter {
    fn emit_event(&self, event_name: &str) -> Result<(), String> {
        self.events.lock().unwrap().push(MockEvent {
            name: event_name.to_string(),
            payload: None,
        });
        Ok(())
    }

    fn emit_event_with_payload(&self, event_name: &str, payload: &str) -> Result<(), String> {
        self.events.lock().unwrap().push(MockEvent {
            name: event_name.to_string(),
            payload: Some(payload.to_string()),
        });
        Ok(())
    }
}

/// Returns a cancel sender/receiver pair where the sender is never triggered.
/// Pass the receiver to [`engine::download`] for tests that don't need cancellation.
/// Keep the returned sender alive (binding it with `_`) until after `download` returns.
pub fn no_cancel() -> (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
) {
    tokio::sync::oneshot::channel::<()>()
}

/// Helper to manage temp directories and files for E2E tests.
pub struct TestFixture {
    pub dir: tempfile::TempDir,
}

impl TestFixture {
    pub fn new() -> Self {
        Self {
            dir: tempfile::TempDir::new().expect("failed to create temp dir"),
        }
    }

    /// Create a file with the given content, returns absolute path.
    pub fn create_file(&self, name: &str, content: &[u8]) -> PathBuf {
        let path = self.dir.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("failed to create parent dirs");
        }
        std::fs::write(&path, content).expect("failed to write file");
        path
    }

    /// Create a large file filled with a deterministic pattern.
    pub fn create_large_file(&self, name: &str, size: usize) -> PathBuf {
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        self.create_file(name, &data)
    }

    /// Create a directory with multiple files.
    /// `files` is a slice of (relative_path, content) tuples.
    pub fn create_dir_with_files(&self, dir_name: &str, files: &[(&str, &[u8])]) -> PathBuf {
        let dir_path = self.dir.path().join(dir_name);
        for (rel_path, content) in files {
            let file_path = dir_path.join(rel_path);
            if let Some(parent) = file_path.parent() {
                std::fs::create_dir_all(parent).expect("failed to create parent dirs");
            }
            std::fs::write(&file_path, content).expect("failed to write file");
        }
        dir_path
    }

    /// Returns a fresh output directory for receiving files.
    pub fn output_dir(&self) -> PathBuf {
        let out = self.dir.path().join("received");
        std::fs::create_dir_all(&out).expect("failed to create output dir");
        out
    }

    /// Returns a fresh named output directory for receiving files.
    /// Use this when a single test needs multiple independent receive directories.
    pub fn output_dir_named(&self, name: &str) -> PathBuf {
        let out = self.dir.path().join(name);
        std::fs::create_dir_all(&out).expect("failed to create output dir");
        out
    }
}

/// A temp file with fixed contents, keeping its temp dir alive with it.
pub struct TempFileWithContents {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl TempFileWithContents {
    pub fn path_string(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }
}

/// Temp file containing `contents`; `path_string()` feeds a send/share call.
pub async fn temp_file_with_contents(contents: &str) -> TempFileWithContents {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("nearby-invite.txt");
    tokio::fs::write(&path, contents)
        .await
        .expect("write temp file");
    TempFileWithContents { _dir: dir, path }
}

/// Poll `check` every 500ms until it returns true or `deadline` elapses.
pub async fn wait_until(what: &str, deadline: std::time::Duration, check: impl Fn() -> bool) {
    let end = tokio::time::Instant::now() + deadline;
    while !check() {
        assert!(
            tokio::time::Instant::now() < end,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

const NODE_START_TIMEOUT: Duration = Duration::from_secs(60);
const PAIR_JOIN_DEADLINE: Duration = Duration::from_secs(90);
const PAIR_SETTLE_DEADLINE: Duration = Duration::from_secs(30);

/// A running [`NodeService`] with a fixed display name, for E2E tests that
/// bind real endpoints. Keeps its temp data dir alive for the node's lifetime.
pub struct TestNode {
    pub service: NodeService,
    /// The emitter this node started with, for `events.has_event(...)`.
    pub events: Arc<MockEventEmitter>,
    _dir: tempfile::TempDir,
}

impl TestNode {
    pub fn endpoint_id(&self) -> String {
        self.service.device_info().endpoint_id
    }

    pub async fn set_discoverability(&self, setting: Discoverability) {
        self.service
            .set_discoverability(setting)
            .await
            .expect("set_discoverability");
    }

    pub async fn probe_identity(&self, endpoint_id: &str) -> anyhow::Result<DeviceInfo> {
        self.service.probe_identity(endpoint_id).await
    }
}

impl std::ops::Deref for TestNode {
    type Target = NodeService;

    fn deref(&self) -> &NodeService {
        &self.service
    }
}

/// Start a node with a fixed display name.
pub async fn spawn_node(display_name: &str) -> TestNode {
    let dir = tempfile::tempdir().expect("node temp dir");
    let emitter = MockEventEmitter::new();
    let service = tokio::time::timeout(
        NODE_START_TIMEOUT,
        NodeService::start(
            dir.path(),
            RelayMode::Default,
            DiscoveryModeOption::Default,
            Discoverability::default(),
            Some(emitter.clone()),
        ),
    )
    .await
    .expect("node start timed out")
    .expect("node start failed");
    service
        .set_device_display_name(display_name)
        .expect("set display name");

    TestNode {
        service,
        events: emitter,
        _dir: dir,
    }
}

/// Start a node for lan-discovery E2E tests. `Everyone` by default, so mDNS
/// starts with the node; relay is disabled so the only route between the two is
/// the addresses mDNS supplies.
pub async fn spawn_node_with_lan_discovery(display_name: &str) -> TestNode {
    let dir = tempfile::tempdir().expect("node temp dir");
    let emitter = MockEventEmitter::new();
    let service = tokio::time::timeout(
        NODE_START_TIMEOUT,
        NodeService::start(
            dir.path(),
            RelayMode::Disabled,
            DiscoveryModeOption::Default,
            Discoverability::default(),
            Some(emitter.clone()),
        ),
    )
    .await
    .expect("node start timed out")
    .expect("node start failed");
    service
        .set_device_display_name(display_name)
        .expect("set display name");

    TestNode {
        service,
        events: emitter,
        _dir: dir,
    }
}

/// Polls `node`'s Nearby list until `endpoint_id` appears *and* its identity
/// probe finished, or `deadline` elapses. Waiting for `identified` avoids
/// racing the background probe; the last observation is returned on timeout.
pub async fn wait_for_nearby(
    node: &TestNode,
    endpoint_id: &str,
    deadline: Duration,
) -> Option<NearbyDevice> {
    let end = tokio::time::Instant::now() + deadline;
    loop {
        let found = node
            .service
            .list_nearby()
            .await
            .into_iter()
            .find(|d| d.endpoint_id.eq_ignore_ascii_case(endpoint_id));
        if matches!(&found, Some(device) if device.identified) {
            return found;
        }
        if tokio::time::Instant::now() >= end {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Polls `node`'s Nearby list until `endpoint_id` is gone, or `deadline`
/// elapses. mDNS sends an explicit goodbye, so this resolves in seconds.
pub async fn wait_until_absent(node: &TestNode, endpoint_id: &str, deadline: Duration) -> bool {
    let end = tokio::time::Instant::now() + deadline;
    loop {
        let present = node
            .service
            .list_nearby()
            .await
            .into_iter()
            .any(|d| d.endpoint_id.eq_ignore_ascii_case(endpoint_id));
        if !present {
            return true;
        }
        if tokio::time::Instant::now() >= end {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Two nodes already paired with each other.
pub async fn spawn_paired_nodes() -> (TestNode, TestNode) {
    let host = spawn_node("alice").await;
    let joiner = spawn_node("bob").await;

    let ticket = host
        .start_pairing_host(Some(300))
        .await
        .expect("open pairing window");

    let end = tokio::time::Instant::now() + PAIR_JOIN_DEADLINE;
    loop {
        match tokio::time::timeout(Duration::from_secs(30), joiner.join_pairing(&ticket)).await {
            Ok(Ok(())) => break,
            Ok(Err(err)) => {
                assert!(
                    tokio::time::Instant::now() < end,
                    "join_pairing did not succeed within {PAIR_JOIN_DEADLINE:?}: {err:#}"
                );
            }
            Err(_) => {
                assert!(
                    tokio::time::Instant::now() < end,
                    "join_pairing did not succeed within {PAIR_JOIN_DEADLINE:?}: last attempt hung"
                );
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // The host finishes its side of the handshake asynchronously.
    let joiner_id = joiner.endpoint_id();
    wait_until(
        "host to store the joiner as paired",
        PAIR_SETTLE_DEADLINE,
        || {
            host.list_paired()
                .expect("list_paired")
                .into_iter()
                .any(|d| d.endpoint_id.eq_ignore_ascii_case(&joiner_id))
        },
    )
    .await;

    (host, joiner)
}

/// Same-machine transfer tests should carry the provider's addresses in the
/// ticket. ID-only production defaults require public DNS publication and
/// lookup to complete before a receiver can connect, which is unrelated to
/// event, cleanup, metadata, and payload assertions in these tests.
pub fn send_options_with_addresses() -> engine::SendOptions {
    engine::SendOptions {
        ticket_type: engine::AddrInfoOptions::RelayAndAddresses,
        ..Default::default()
    }
}
