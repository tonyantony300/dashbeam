mod common;

use std::time::Duration;

use common::spawn_node;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authorized_peer_pairs_without_open_host() {
    let (a, b) = tokio::join!(spawn_node("a"), spawn_node("b"));
    b.service
        .set_authorized_peers(vec![a.endpoint_id()])
        .await
        .unwrap();
    let ticket = b.service.pairing_ticket().unwrap();
    tokio::time::timeout(Duration::from_secs(90), a.service.join_pairing(&ticket))
        .await
        .expect("join timed out")
        .expect("authorized join");
    assert!(b
        .service
        .list_paired()
        .unwrap()
        .iter()
        .any(|d| d.endpoint_id == a.endpoint_id()));
    assert!(a
        .service
        .list_paired()
        .unwrap()
        .iter()
        .any(|d| d.endpoint_id == b.endpoint_id()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unlisted_peer_cannot_pair_without_open_host() {
    let (a, b) = tokio::join!(spawn_node("a"), spawn_node("b"));
    let ticket = b.service.pairing_ticket().unwrap();
    let res = tokio::time::timeout(Duration::from_secs(90), a.service.join_pairing(&ticket))
        .await
        .expect("join timed out");
    assert!(res.is_err());
    assert!(b.service.list_paired().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_authorized_peers_rejects_invalid_id() {
    let a = spawn_node("a").await;
    assert!(a
        .service
        .set_authorized_peers(vec!["not-an-id".into()])
        .await
        .is_err());
}
