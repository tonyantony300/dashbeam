//! Socket server over an in-process `AgentService`. Binds real endpoints: run
//! with `--test-threads=1`.

use std::sync::Arc;

use agent::approver::DenyAll;
use agent::config::AgentConfig;
use agent::service::{AgentOptions, AgentService};
use dashbeam_cli::ipc::{self, Client, NodeInfo, Request, StartedBy};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

struct Fixture {
    svc: Arc<AgentService>,
    stop: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
    dir: tempfile::TempDir,
}

impl Fixture {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let svc = AgentService::start(AgentOptions {
            data_dir: dir.path().to_path_buf(),
            config: AgentConfig::default(),
            allow_agent_pairing: false,
            discoverable_override: false,
            approver: Arc::new(DenyAll),
        })
        .await
        .expect("agent start");
        let server = ipc::Server::bind(svc.clone(), StartedBy::Hand).expect("bind");
        let (tx, rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(server.run(async move {
            let _ = rx.await;
        }));
        Self {
            svc,
            stop: Some(tx),
            server: Some(handle),
            dir,
        }
    }

    async fn stop(mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.server.take() {
            h.await.unwrap().unwrap();
        }
        self.svc.shutdown().await.unwrap();
        assert!(
            !agent::paths::socket_path(self.dir.path()).exists(),
            "socket removed on stop"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hello_version_mismatch_errors_with_stop_hint() {
    let fx = Fixture::start().await;
    let sock = agent::paths::socket_path(fx.dir.path());
    let stream = ipc::transport::connect(&sock).await.unwrap();
    let (r, mut w) = ipc::transport::split(stream);
    w.write_all(b"{\"hello\":{\"version\":999}}\n")
        .await
        .unwrap();
    let mut line = String::new();
    BufReader::new(r).read_line(&mut line).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    let err = v["error"].as_str().expect("error reply");
    assert!(err.contains("another version"), "{err}");
    assert!(err.contains("protocol 1"), "{err}");
    assert!(err.contains("run `dashbeam stop`"), "{err}");
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn request_roundtrip_identity() {
    let fx = Fixture::start().await;
    let mut client = Client::connect(fx.dir.path()).await.expect("connect");
    let hello: &NodeInfo = client.hello();
    assert_eq!(hello.version, ipc::PROTOCOL_VERSION);
    assert_eq!(hello.started_by, StartedBy::Hand);
    assert!(!hello.agent_pairing_allowed);
    assert_eq!(hello.pid, std::process::id());
    assert_eq!(hello.node.endpoint_id, fx.svc.identity().endpoint_id);

    let me: engine::DeviceInfo = client.call(Request::Identity).await.unwrap();
    assert_eq!(me.endpoint_id, fx.svc.identity().endpoint_id);
    assert_eq!(me.device_type, "agent");

    // Errors come back as `{"error":…}` and the connection stays usable.
    let err = client
        .call::<serde_json::Value>(Request::GetTransfer { id: "nope".into() })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("nope"), "{err}");
    let devices: Vec<engine::PairedDeviceInfo> = client.call(Request::ListDevices).await.unwrap();
    assert!(devices.is_empty());

    // The ungated pairing code works for the human CLI; the gated agent op refuses.
    let code: String = client.call(Request::PairingCode).await.unwrap();
    assert!(!code.is_empty());
    let gated = client
        .call::<serde_json::Value>(Request::AgentTrust {
            device: "x".into(),
            trusted: true,
        })
        .await
        .unwrap_err();
    assert!(
        gated
            .to_string()
            .contains("dangerously-allow-agent-pairing"),
        "{gated}"
    );
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relative_cwd_is_rejected() {
    let fx = Fixture::start().await;
    let mut client = Client::connect(fx.dir.path()).await.unwrap();
    let err = client
        .call::<serde_json::Value>(Request::ShareTicket {
            paths: vec!["a.txt".into()],
            cwd: "relative/dir".into(),
            ttl_secs: None,
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("absolute"), "{err}");
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_request_stops_server() {
    let mut fx = Fixture::start().await;
    let mut client = Client::connect(fx.dir.path()).await.unwrap();
    let _: serde_json::Value = client.call(Request::Shutdown).await.unwrap();
    // The server returns on its own; no external stop signal needed.
    let h = fx.server.take().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), h)
        .await
        .expect("server exits after Shutdown")
        .unwrap()
        .unwrap();
    fx.stop().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn socket_is_0600() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::start().await;
    let sock = agent::paths::socket_path(fx.dir.path());
    let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "socket mode {mode:o}");
    fx.stop().await;
}

/// A marker that doesn't name a local dashbeam pipe (garbled, or pointing at
/// another host) reads as "no node", so the starter replaces it.
#[cfg(windows)]
#[tokio::test]
async fn foreign_pipe_marker_is_not_running() {
    let dir = tempfile::tempdir().unwrap();
    let sock = agent::paths::socket_path(dir.path());
    for marker in ["", "garbage", r"\\evil\pipe\dashbeam-0123abcd"] {
        std::fs::write(&sock, marker).unwrap();
        let err = Client::connect(dir.path()).await.err().unwrap();
        assert!(ipc::is_not_running(&err), "{marker:?}: {err}");
    }
}

#[test]
fn request_wire_shape() {
    let v = serde_json::to_value(Request::SendToDevice {
        device: "Mac".into(),
        paths: vec!["/a".into()],
        cwd: "/".into(),
    })
    .unwrap();
    assert_eq!(v["op"], "send_to_device");
    assert_eq!(v["device"], "Mac");
    assert_eq!(
        serde_json::to_value(Request::Shutdown).unwrap(),
        serde_json::json!({"op": "shutdown"})
    );
}

#[cfg(unix)]
#[tokio::test]
async fn overlong_socket_path_errors_clearly() {
    let long = std::path::PathBuf::from(format!("/tmp/{}", "d".repeat(120)));
    let err = ipc::check_socket_path(&long).unwrap_err().to_string();
    assert!(err.contains("too long"), "{err}");
    let err = Client::connect(&long).await.err().unwrap();
    assert!(!ipc::is_not_running(&err));
    assert!(err.to_string().contains("shorter --data-dir"), "{err}");
}
