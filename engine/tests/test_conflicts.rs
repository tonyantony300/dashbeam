mod common;

use common::{MockEventEmitter, TestFixture};
use engine::{download, start_share, ReceiveOptions};

#[tokio::test]
async fn e2e_filename_conflict_resolved() {
    let fixture = TestFixture::new();
    let source = fixture.create_file("report.txt", b"new version of report");
    let recv_dir = fixture.output_dir();

    std::fs::write(recv_dir.join("report.txt"), b"old version of report")
        .expect("should create conflicting file");

    let receiver_emitter = MockEventEmitter::new();

    let share = start_share(source, common::send_options_with_addresses(), None, None)
        .await
        .expect("start_share should succeed");

    let (_cancel_tx, cancel_rx) = common::no_cancel();
    download(
        share.ticket.clone(),
        ReceiveOptions {
            output_dir: Some(recv_dir.clone()),
            ..Default::default()
        },
        Some(receiver_emitter.clone()),
        cancel_rx,
    )
    .await
    .expect("download should succeed even with conflict");

    let renamed = std::fs::read_to_string(recv_dir.join("report (1).txt"))
        .expect("renamed file should exist");
    assert_eq!(renamed, "new version of report");

    let original = std::fs::read_to_string(recv_dir.join("report.txt"))
        .expect("original file should still exist");
    assert_eq!(
        original, "old version of report",
        "original file must NOT be overwritten during conflict resolution"
    );

    assert!(
        receiver_emitter.has_event("receive-conflicts"),
        "should emit receive-conflicts event"
    );

    drop(share);
}

#[tokio::test]
async fn e2e_original_file_preserved() {
    let fixture = TestFixture::new();
    let source = fixture.create_file("keep_me.txt", b"incoming data");
    let recv_dir = fixture.output_dir();

    std::fs::write(recv_dir.join("keep_me.txt"), b"original data, do not touch")
        .expect("should create original file");

    let share = start_share(source, common::send_options_with_addresses(), None, None)
        .await
        .expect("start_share should succeed");

    let (_cancel_tx, cancel_rx) = common::no_cancel();
    download(
        share.ticket.clone(),
        ReceiveOptions {
            output_dir: Some(recv_dir.clone()),
            ..Default::default()
        },
        None,
        cancel_rx,
    )
    .await
    .expect("download should succeed");

    let original = std::fs::read_to_string(recv_dir.join("keep_me.txt")).unwrap();
    assert_eq!(
        original, "original data, do not touch",
        "original file must NOT be overwritten"
    );

    let renamed = std::fs::read_to_string(recv_dir.join("keep_me (1).txt"))
        .expect("incoming file should have been saved under a conflict-resolved name");
    assert_eq!(
        renamed, "incoming data",
        "incoming content must be preserved"
    );

    drop(share);
}
