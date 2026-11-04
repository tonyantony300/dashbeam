//! Pre-approved pairing and the pairing API. Binds real endpoints: run with
//! `--test-threads=1`.
//!
//! Each agent must know the other's endpoint id before start, so every data dir
//! is started and stopped once to learn its id (identity persists in
//! `identity.key`), then both are restarted with configs listing each other.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agent::approver::DenyAll;
use agent::config::{AgentConfig, AuthorizedPeer, PairingConfig};
use agent::service::{AgentOptions, AgentService};

const WAIT: Duration = Duration::from_secs(60);

async fn start_agent(dir: &Path, config: AgentConfig, allow: bool) -> Arc<AgentService> {
    AgentService::start(AgentOptions {
        data_dir: dir.to_path_buf(),
        config,
        allow_agent_pairing: allow,
        discoverable_override: false,
        approver: Arc::new(DenyAll),
    })
    .await
    .expect("agent start")
}

async fn learn_id(dir: &Path) -> String {
    let svc = start_agent(dir, AgentConfig::default(), false).await;
    let id = svc.identity().endpoint_id;
    svc.shutdown().await.unwrap();
    id
}

fn lists(peer: &str, trust: bool) -> AgentConfig {
    AgentConfig {
        pairing: PairingConfig {
            authorized: vec![AuthorizedPeer {
                endpoint_id: peer.to_string(),
                relay_url: None,
                trust,
            }],
        },
        ..AgentConfig::default()
    }
}

async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !cond() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn paired(svc: &AgentService, id: &str) -> Option<engine::PairedDeviceInfo> {
    svc.list_devices()
        .unwrap()
        .into_iter()
        .find(|d| d.endpoint_id == id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mutually_authorized_agents_pair_at_startup() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (id_a, id_b) = (learn_id(da.path()).await, learn_id(db.path()).await);
    let a = start_agent(da.path(), lists(&id_b, false), false).await;
    let b = start_agent(db.path(), lists(&id_a, false), false).await;

    wait_for("both paired", || {
        paired(&a, &id_b).is_some() && paired(&b, &id_a).is_some()
    })
    .await;
    assert!(!paired(&a, &id_b).unwrap().trusted);

    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authorized_trust_flag_applies() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (id_a, id_b) = (learn_id(da.path()).await, learn_id(db.path()).await);
    let a = start_agent(da.path(), lists(&id_b, true), false).await;
    let b = start_agent(db.path(), lists(&id_a, false), false).await;

    wait_for("A trusts B", || {
        paired(&a, &id_b).is_some_and(|d| d.trusted)
    })
    .await;
    wait_for("B paired", || paired(&b, &id_a).is_some()).await;
    assert!(!paired(&b, &id_a).unwrap().trusted);

    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_wrappers_refuse_without_flag() {
    let dir = tempfile::tempdir().unwrap();
    let svc = start_agent(dir.path(), AgentConfig::default(), false).await;
    let want = "agent pairing is disabled; start the node with --dangerously-allow-agent-pairing";
    let e = svc.agent_pair_nearby("anyone").await.unwrap_err();
    assert_eq!(e.to_string(), want);
    let e = svc.agent_trust("anyone", true).await.unwrap_err();
    assert_eq!(e.to_string(), want);
    svc.shutdown().await.unwrap();

    // With the flag the gate opens and the call reaches device resolution.
    let svc = start_agent(dir.path(), AgentConfig::default(), true).await;
    let e = svc.agent_trust("anyone", true).await.unwrap_err();
    assert!(e.to_string().contains("anyone"), "{e}");
    assert_ne!(e.to_string(), want);
    svc.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manual_untrust_survives_later_pairings() {
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let (id_a, id_b) = (
        learn_id(dirs[0].path()).await,
        learn_id(dirs[1].path()).await,
    );
    let a = start_agent(dirs[0].path(), lists(&id_b, true), false).await;
    let b = start_agent(dirs[1].path(), lists(&id_a, false), false).await;
    wait_for("A trusts B", || {
        paired(&a, &id_b).is_some_and(|d| d.trusted)
    })
    .await;

    a.trust(&id_b, false).unwrap();

    // An unrelated pairing fires `device-paired` on A again.
    let c = start_agent(dirs[2].path(), AgentConfig::default(), false).await;
    let code = a.host_pairing(Some(60)).await.unwrap();
    c.join(&code).await.unwrap();
    let id_c = c.identity().endpoint_id;
    wait_for("A pairs C", || paired(&a, &id_c).is_some()).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!paired(&a, &id_b).unwrap().trusted, "untrust was reverted");

    for s in [&a, &b, &c] {
        s.shutdown().await.unwrap();
    }
}
