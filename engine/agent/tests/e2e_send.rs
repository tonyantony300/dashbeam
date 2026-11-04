//! Outbound paired sends between two `AgentService`s. Binds real endpoints:
//! run with `--test-threads=1`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent::approver::DenyAll;
use agent::config::AgentConfig;
use agent::registry::{Direction, TransferKind, TransferRecord, TransferStatus};
use agent::service::{AgentOptions, AgentService};

const DEADLINE: Duration = Duration::from_secs(90);

struct Pair {
    a: Arc<AgentService>,
    b: Arc<AgentService>,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

impl Pair {
    fn a_id(&self) -> String {
        self.a.identity().endpoint_id
    }

    fn b_id(&self) -> String {
        self.b.identity().endpoint_id
    }

    async fn shutdown(self) {
        self.a.shutdown().await.unwrap();
        self.b.shutdown().await.unwrap();
    }
}

fn named(name: &str, invite_timeout_secs: u64) -> AgentConfig {
    AgentConfig {
        name: Some(name.to_string()),
        invite_timeout_secs,
        ..AgentConfig::default()
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

/// A ("Alpha") and B ("Bravo") paired through the pre-authorized path;
/// `b_trusts_a` makes B auto-accept A's invites.
async fn paired(a_cfg: AgentConfig, b_cfg: AgentConfig, b_trusts_a: bool) -> Pair {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = start_agent(da.path(), a_cfg).await;
    let b = start_agent(db.path(), b_cfg).await;
    let (a_id, b_id) = (a.identity().endpoint_id, b.identity().endpoint_id);
    a.node()
        .set_authorized_peers(vec![b_id.clone()])
        .await
        .unwrap();
    b.node()
        .set_authorized_peers(vec![a_id.clone()])
        .await
        .unwrap();

    let ticket = b.node().pairing_ticket().unwrap();
    let end = tokio::time::Instant::now() + DEADLINE;
    loop {
        match tokio::time::timeout(Duration::from_secs(30), a.node().join_pairing(&ticket)).await {
            Ok(Ok(())) => break,
            other => assert!(
                tokio::time::Instant::now() < end,
                "join_pairing never succeeded: {other:?}"
            ),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    wait_for("both sides to store the pairing", || {
        knows(&a, &b_id) && knows(&b, &a_id)
    })
    .await;
    if b_trusts_a {
        b.node().trust_paired(&a_id, true).unwrap();
    }
    Pair {
        a,
        b,
        _dirs: (da, db),
    }
}

fn knows(svc: &AgentService, id: &str) -> bool {
    svc.list_devices()
        .unwrap()
        .iter()
        .any(|d| d.endpoint_id == id)
}

async fn wait_for(what: &str, check: impl Fn() -> bool) {
    let end = tokio::time::Instant::now() + DEADLINE;
    while !check() {
        assert!(tokio::time::Instant::now() < end, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn wait_terminal(svc: &AgentService, id: &str, timeout: Duration) -> TransferRecord {
    let (rec, running) = svc.registry().wait_terminal(id, timeout).await.unwrap();
    assert!(!running, "transfer still running: {rec:?}");
    rec
}

fn status(svc: &AgentService, id: &str) -> TransferStatus {
    svc.registry().get(id).unwrap().status
}

fn write(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    path
}

fn inbound(svc: &AgentService) -> Vec<TransferRecord> {
    svc.registry().list(None, Some(Direction::Inbound))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trusted_send_is_delivered() {
    let p = paired(named("Alpha", 600), named("Bravo", 600), true).await;
    let src = tempfile::tempdir().unwrap();
    write(src.path(), "hello.txt", b"hello bravo");

    // Relative path: resolved against the caller's cwd.
    let rec =
        p.a.send_to_device(
            "bravo",
            vec![PathBuf::from("hello.txt")],
            src.path().to_path_buf(),
        )
        .await
        .unwrap();
    assert_eq!(rec.status, TransferStatus::Starting, "{rec:?}");
    assert_eq!(rec.direction, Direction::Outbound);
    assert_eq!(rec.kind, TransferKind::Paired);
    assert_eq!(rec.file_names, vec!["hello.txt".to_string()]);
    assert_eq!(rec.total_bytes, 11);
    let peer = rec.peer.clone().unwrap();
    assert_eq!(peer.endpoint_id.as_deref(), Some(p.b_id().as_str()));
    assert_eq!(peer.name.as_deref(), Some("Bravo"));

    let done = wait_terminal(&p.a, &rec.id, DEADLINE).await;
    assert_eq!(done.status, TransferStatus::Delivered, "{done:?}");
    assert_eq!(done.bytes_done, 11);
    assert!(done.error.is_none());

    wait_for("B to record the receive", || {
        inbound(&p.b).iter().any(|r| r.status.is_terminal())
    })
    .await;
    let got = inbound(&p.b);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].status, TransferStatus::Received, "{got:?}");
    let saved =
        p.b.config()
            .receive_dir(p.b.data_dir())
            .join("Alpha")
            .join("hello.txt");
    assert_eq!(std::fs::read(saved).unwrap(), b"hello bravo");

    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn untrusted_send_declined() {
    let p = paired(named("Alpha", 600), named("Bravo", 600), false).await;
    let src = tempfile::tempdir().unwrap();
    let file = write(src.path(), "notes.txt", b"notes");

    let rec =
        p.a.send_to_device(&p.b_id(), vec![file], src.path().to_path_buf())
            .await
            .unwrap();
    wait_for("B to hold the invite", || p.b.list_incoming().len() == 1).await;
    wait_for("A to wait for the answer", || {
        status(&p.a, &rec.id) == TransferStatus::WaitingForAccept
    })
    .await;
    let invite = p.b.list_incoming().remove(0);
    assert_eq!(invite.endpoint_id, p.a_id());
    assert_eq!((invite.file_count, invite.total_size), (1, 5));
    p.b.decline_incoming(&invite.invite_id).await.unwrap();

    let done = wait_terminal(&p.a, &rec.id, DEADLINE).await;
    assert_eq!(done.status, TransferStatus::Declined, "{done:?}");
    assert!(inbound(&p.b).is_empty());

    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn untrusted_send_expires() {
    // B outlasts A, so A's expiry is its own and B's later decline is stale.
    let a_timeout = 5;
    let p = paired(named("Alpha", a_timeout), named("Bravo", 12), false).await;
    let src = tempfile::tempdir().unwrap();
    let file = write(src.path(), "late.txt", b"late");

    let rec =
        p.a.send_to_device("Bravo", vec![file], src.path().to_path_buf())
            .await
            .unwrap();
    wait_for("A to wait for the answer", || {
        status(&p.a, &rec.id) == TransferStatus::WaitingForAccept
    })
    .await;
    let waiting = p.a.registry().get(&rec.id).unwrap();
    assert!(waiting.expires_at_ms.is_some());

    let done = wait_terminal(&p.a, &rec.id, Duration::from_secs(a_timeout + 5)).await;
    assert_eq!(done.status, TransferStatus::Expired, "{done:?}");

    wait_for("B's own expiry to clear the invite", || {
        p.b.list_incoming().is_empty()
    })
    .await;
    // B's late decline must not touch A's terminal record.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let after = p.a.registry().get(&rec.id).unwrap();
    assert_eq!(after.status, TransferStatus::Expired);
    assert_eq!(after.seq, done.seq);

    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_send_to_same_device_queues_and_each_gets_own_outcome() {
    let p = paired(named("Alpha", 600), named("Bravo", 600), false).await;
    let src = tempfile::tempdir().unwrap();
    let one = write(src.path(), "one.txt", b"first");
    let two = write(src.path(), "two.txt", b"second!");
    let cwd = src.path().to_path_buf();

    let first =
        p.a.send_to_device("Bravo", vec![one], cwd.clone())
            .await
            .unwrap();
    let second = p.a.send_to_device("Bravo", vec![two], cwd).await.unwrap();
    assert_eq!(first.status, TransferStatus::Starting);
    assert_eq!(second.status, TransferStatus::Queued);

    wait_for("B to hold the first invite", || {
        p.b.list_incoming().len() == 1
    })
    .await;
    let invite = p.b.list_incoming().remove(0);
    assert_eq!(invite.total_size, 5);
    assert_eq!(status(&p.a, &second.id), TransferStatus::Queued);
    p.b.decline_incoming(&invite.invite_id).await.unwrap();

    let done = wait_terminal(&p.a, &first.id, DEADLINE).await;
    assert_eq!(done.status, TransferStatus::Declined, "{done:?}");

    wait_for("B to hold the second invite", || {
        p.b.list_incoming()
            .first()
            .is_some_and(|i| i.invite_id != invite.invite_id)
    })
    .await;
    let invite2 = p.b.list_incoming().remove(0);
    assert_eq!(invite2.total_size, 7);
    p.b.accept_incoming(&invite2.invite_id).await.unwrap();

    let done2 = wait_terminal(&p.a, &second.id, DEADLINE).await;
    assert_eq!(done2.status, TransferStatus::Delivered, "{done2:?}");
    // The first record kept its own outcome.
    assert_eq!(status(&p.a, &first.id), TransferStatus::Declined);

    wait_for("B to finish the receive", || {
        inbound(&p.b).iter().any(|r| r.status.is_terminal())
    })
    .await;
    let got = inbound(&p.b);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].status, TransferStatus::Received);

    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_device_errors_with_candidates() {
    let p = paired(named("Alpha", 600), named("Bravo", 600), false).await;
    let src = tempfile::tempdir().unwrap();
    let file = write(src.path(), "a.txt", b"a");
    let cwd = src.path().to_path_buf();

    let err =
        p.a.send_to_device("Charlie", vec![file.clone()], cwd.clone())
            .await
            .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("Charlie"), "{msg}");
    assert!(msg.contains("Bravo"), "{msg}");
    assert!(msg.contains(&p.b_id()), "{msg}");

    // Review Focus 1: a missing path is named before anything is shared.
    let err =
        p.a.send_to_device("Bravo", vec![PathBuf::from("missing.txt")], cwd.clone())
            .await
            .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("path not found"), "{msg}");
    assert!(
        msg.contains(&cwd.join("missing.txt").display().to_string()),
        "{msg}"
    );

    assert!(p.a.registry().list(None, None).is_empty());
    assert!(p.b.list_incoming().is_empty());

    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_stops_queued_and_running_sends() {
    let p = paired(named("Alpha", 600), named("Bravo", 600), false).await;
    let src = tempfile::tempdir().unwrap();
    let one = write(src.path(), "one.txt", b"one");
    let two = write(src.path(), "two.txt", b"two");
    let cwd = src.path().to_path_buf();

    let first =
        p.a.send_to_device("Bravo", vec![one.clone()], cwd.clone())
            .await
            .unwrap();
    let second =
        p.a.send_to_device("Bravo", vec![two], cwd.clone())
            .await
            .unwrap();
    assert_eq!(second.status, TransferStatus::Queued);

    let cancelled = p.a.cancel_transfer(&second.id).await.unwrap();
    assert_eq!(cancelled.status, TransferStatus::Cancelled);

    wait_for("A to wait for the answer", || {
        status(&p.a, &first.id) == TransferStatus::WaitingForAccept
    })
    .await;
    let cancelled = p.a.cancel_transfer(&first.id).await.unwrap();
    assert_eq!(cancelled.status, TransferStatus::Cancelled, "{cancelled:?}");

    // Terminal records come back unchanged.
    let again = p.a.cancel_transfer(&first.id).await.unwrap();
    assert_eq!(again.seq, cancelled.seq);
    assert!(p.a.cancel_transfer("no-such-id").await.is_err());

    // The device's queue is free again.
    let third = p.a.send_to_device("Bravo", vec![one], cwd).await.unwrap();
    assert_eq!(third.status, TransferStatus::Starting, "{third:?}");
    p.a.cancel_transfer(&third.id).await.unwrap();

    p.shutdown().await;
}
