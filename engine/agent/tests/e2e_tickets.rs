//! One-time ticket shares between two `AgentService`s. Binds real endpoints:
//! run with `--test-threads=1`.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use agent::approver::DenyAll;
use agent::config::AgentConfig;
use agent::registry::{Direction, TransferKind, TransferRecord, TransferStatus};
use agent::service::{AgentOptions, AgentService};

const WAIT: Duration = Duration::from_secs(90);

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

fn write(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    path
}

async fn wait_terminal(svc: &AgentService, id: &str) -> TransferRecord {
    let (rec, running) = svc.registry().wait_terminal(id, WAIT).await.unwrap();
    assert!(!running, "transfer still running: {rec:?}");
    rec
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ticket_is_one_time() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = start_agent(da.path(), AgentConfig::default()).await;
    let b = start_agent(db.path(), AgentConfig::default()).await;
    let src = tempfile::tempdir().unwrap();
    let file = write(src.path(), "once.txt", b"only once");

    let shared = a
        .share_ticket(vec![file], src.path().to_path_buf(), None)
        .await
        .unwrap();
    assert_eq!(shared.status, TransferStatus::WaitingForDownload);
    assert_eq!(shared.kind, TransferKind::Ticket);
    assert_eq!(shared.direction, Direction::Outbound);
    let ticket = shared.ticket.clone().expect("ticket");
    assert!(shared.expires_at_ms.is_some());

    let first = b.receive_ticket(&ticket).await.unwrap();
    assert_eq!(first.status, TransferStatus::Received, "{first:?}");
    assert_eq!(first.kind, TransferKind::Ticket);
    assert_eq!(first.saved_paths.len(), 1);
    assert!(first.saved_paths[0].starts_with(db.path().join("received").join("tickets")));
    assert_eq!(std::fs::read(&first.saved_paths[0]).unwrap(), b"only once");

    let sent = wait_terminal(&a, &shared.id).await;
    assert_eq!(sent.status, TransferStatus::Delivered, "{sent:?}");

    let second = tokio::time::timeout(Duration::from_secs(60), b.receive_ticket(&ticket))
        .await
        .expect("second receive must fail within 60 s")
        .unwrap();
    assert_eq!(second.status, TransferStatus::Failed, "{second:?}");
    assert!(second.error.is_some());

    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unused_ticket_expires() {
    let da = tempfile::tempdir().unwrap();
    let a = start_agent(da.path(), AgentConfig::default()).await;
    let src = tempfile::tempdir().unwrap();
    let file = write(src.path(), "x.txt", b"x");

    let shared = a
        .share_ticket(vec![file], src.path().to_path_buf(), Some(5))
        .await
        .unwrap();
    let done = wait_terminal(&a, &shared.id).await;
    assert_eq!(done.status, TransferStatus::Expired, "{done:?}");
    a.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_ticket_share() {
    let da = tempfile::tempdir().unwrap();
    let a = start_agent(da.path(), AgentConfig::default()).await;
    let src = tempfile::tempdir().unwrap();
    let file = write(src.path(), "x.txt", b"x");

    let shared = a
        .share_ticket(vec![file], src.path().to_path_buf(), None)
        .await
        .unwrap();
    let done = a.cancel_transfer(&shared.id).await.unwrap();
    assert_eq!(done.status, TransferStatus::Cancelled, "{done:?}");
    a.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn size_cap_rejects_before_download() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = start_agent(da.path(), AgentConfig::default()).await;
    let b = start_agent(
        db.path(),
        AgentConfig {
            max_ticket_receive_bytes: Some(10),
            ..AgentConfig::default()
        },
    )
    .await;
    let src = tempfile::tempdir().unwrap();
    let file = write(src.path(), "big.bin", &[7u8; 1024]);

    let shared = a
        .share_ticket(vec![file], src.path().to_path_buf(), None)
        .await
        .unwrap();
    let err = b
        .receive_ticket(shared.ticket.as_deref().unwrap())
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("ticket payload of 1024 bytes exceeds max_ticket_receive_bytes (10)"),
        "{msg}"
    );
    assert!(!db.path().join("received").join("tickets").exists());

    // The share is untouched by the rejected probe.
    assert_eq!(
        a.registry().get(&shared.id).unwrap().status,
        TransferStatus::WaitingForDownload
    );
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ticket_is_sendme_compatible() {
    let da = tempfile::tempdir().unwrap();
    let a = start_agent(da.path(), AgentConfig::default()).await;
    let src = tempfile::tempdir().unwrap();
    let file = write(src.path(), "x.txt", b"x");

    let shared = a
        .share_ticket(vec![file], src.path().to_path_buf(), None)
        .await
        .unwrap();
    let ticket = shared.ticket.unwrap();
    iroh_blobs::ticket::BlobTicket::from_str(&ticket).expect("parses as a sendme BlobTicket");
    a.shutdown().await.unwrap();
}
