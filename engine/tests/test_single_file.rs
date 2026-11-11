mod common;

use common::{MockEventEmitter, TestFixture};
use engine::{download, start_share, ReceiveOptions};

#[tokio::test]
async fn e2e_single_text_file_roundtrip() {
    let fixture = TestFixture::new();
    let source = fixture.create_file("hello.txt", b"Hello from DashBeam E2E test!");
    let recv_dir = fixture.output_dir();

    let sender_emitter = MockEventEmitter::new();
    let share = start_share(
        source,
        common::send_options_with_addresses(),
        Some(sender_emitter.clone()),
        None,
    )
    .await
    .expect("start_share should succeed");

    assert!(!share.ticket.is_empty(), "ticket should not be empty");
    assert!(share.size > 0, "shared size should be > 0");
    assert_eq!(share.entry_type, "file");

    let receiver_emitter = MockEventEmitter::new();
    let (_cancel_tx, cancel_rx) = common::no_cancel();
    let result = download(
        share.ticket.clone(),
        ReceiveOptions {
            output_dir: Some(recv_dir.clone()),
            ..Default::default()
        },
        Some(receiver_emitter.clone()),
        cancel_rx,
    )
    .await
    .expect("download should succeed");

    assert!(!result.message.is_empty());

    let received = std::fs::read(recv_dir.join("hello.txt")).expect("received file should exist");
    assert_eq!(
        received, b"Hello from DashBeam E2E test!",
        "file content should match exactly"
    );

    assert!(
        receiver_emitter.has_event("receive-started"),
        "should emit receive-started"
    );
    assert!(
        receiver_emitter.has_event("receive-completed"),
        "should emit receive-completed"
    );

    drop(share);
}

#[tokio::test]
async fn e2e_binary_file_roundtrip() {
    let fixture = TestFixture::new();
    let binary_data: Vec<u8> = (0..10_000u32).map(|i| (i % 256) as u8).collect();
    let source = fixture.create_file("data.bin", &binary_data);
    let recv_dir = fixture.output_dir();

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

    let received = std::fs::read(recv_dir.join("data.bin")).expect("received file should exist");
    assert_eq!(received.len(), binary_data.len(), "file size should match");
    assert_eq!(received, binary_data, "binary content should match exactly");

    drop(share);
}

#[tokio::test]
async fn e2e_empty_file_roundtrip() {
    let fixture = TestFixture::new();
    let source = fixture.create_file("empty.txt", b"");
    let recv_dir = fixture.output_dir();

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

    let received = std::fs::read(recv_dir.join("empty.txt")).expect("received file should exist");
    assert_eq!(received.len(), 0, "empty file should remain 0 bytes");

    drop(share);
}
