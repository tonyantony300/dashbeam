//! Both ends of a transfer must report the same bytes against the same total.
//!
//! Regression cover for #166: sender showed 150% of a 7 GB share, receiver 66%.

mod common;

use common::{MockEventEmitter, TestFixture};
use engine::{download, start_share, ReceiveOptions};

/// `<bytes>:<total>:<speed x1000>`
fn parse_progress(payload: &str) -> (u64, u64, f64) {
    let parts: Vec<&str> = payload.split(':').collect();
    assert_eq!(parts.len(), 3, "unexpected progress payload: {payload}");
    (
        parts[0].parse().expect("bytes"),
        parts[1].parse().expect("total"),
        parts[2].parse::<i64>().expect("speed") as f64 / 1000.0,
    )
}

fn progress_samples(emitter: &MockEventEmitter, name: &str) -> Vec<(u64, u64, f64)> {
    emitter
        .events_with_name(name)
        .into_iter()
        .filter_map(|event| event.payload)
        .map(|payload| parse_progress(&payload))
        .collect()
}

async fn wait_for_event(emitter: &MockEventEmitter, name: &str) {
    tokio::time::timeout(tokio::time::Duration::from_secs(10), async {
        loop {
            if emitter.has_event(name) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {name}"));
}

#[tokio::test]
async fn e2e_sender_and_receiver_report_the_same_transfer_size() {
    let fixture = TestFixture::new();
    let source_dir = fixture.create_dir_with_files(
        "share_me",
        &[
            ("one.bin", &vec![7u8; 900_000]),
            ("two.bin", &vec![9u8; 1_500_000]),
            ("sub/three.bin", &vec![3u8; 600_000]),
        ],
    );
    let expected_payload_bytes = 900_000 + 1_500_000 + 600_000;
    let recv_dir = fixture.output_dir();

    let sender_emitter = MockEventEmitter::new();
    let receiver_emitter = MockEventEmitter::new();

    let share = start_share(
        source_dir,
        common::send_options_with_addresses(),
        Some(sender_emitter.clone()),
        None,
    )
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
    .expect("download should succeed");

    wait_for_event(&sender_emitter, "transfer-completed").await;

    let sent = progress_samples(&sender_emitter, "transfer-progress");
    let received = progress_samples(&receiver_emitter, "receive-progress");

    assert!(!sent.is_empty(), "sender should report progress");
    assert!(!received.is_empty(), "receiver should report progress");

    for (bytes, total, _) in sent.iter().chain(received.iter()) {
        assert_eq!(
            *total, expected_payload_bytes,
            "both ends must measure against the shared file bytes"
        );
        assert!(
            bytes <= total,
            "progress {bytes} of {total} exceeds the transfer size"
        );
    }

    let (sender_final, _, _) = *sent.last().expect("sender progress");
    let (receiver_final, _, _) = *received.last().expect("receiver progress");
    assert_eq!(sender_final, expected_payload_bytes);
    assert_eq!(receiver_final, expected_payload_bytes);

    drop(share);
}

#[tokio::test]
async fn e2e_progress_never_moves_backwards() {
    let fixture = TestFixture::new();
    let source = fixture.create_large_file("steady.bin", 3_000_000);
    let recv_dir = fixture.output_dir();

    let sender_emitter = MockEventEmitter::new();
    let receiver_emitter = MockEventEmitter::new();

    let share = start_share(
        source,
        common::send_options_with_addresses(),
        Some(sender_emitter.clone()),
        None,
    )
    .await
    .expect("start_share should succeed");

    let (_cancel_tx, cancel_rx) = common::no_cancel();
    download(
        share.ticket.clone(),
        ReceiveOptions {
            output_dir: Some(recv_dir),
            ..Default::default()
        },
        Some(receiver_emitter.clone()),
        cancel_rx,
    )
    .await
    .expect("download should succeed");

    wait_for_event(&sender_emitter, "transfer-completed").await;

    for name in ["transfer-progress", "receive-progress"] {
        let emitter = if name == "transfer-progress" {
            &sender_emitter
        } else {
            &receiver_emitter
        };
        let samples = progress_samples(emitter, name);
        let mut previous = 0u64;
        for (bytes, _, _) in samples {
            assert!(
                bytes >= previous,
                "{name} went backwards: {previous} then {bytes}"
            );
            previous = bytes;
        }
    }

    drop(share);
}

#[tokio::test]
async fn e2e_completion_events_carry_a_wire_duration() {
    let fixture = TestFixture::new();
    let source = fixture.create_large_file("timed.bin", 2_000_000);
    let recv_dir = fixture.output_dir();

    let sender_emitter = MockEventEmitter::new();
    let receiver_emitter = MockEventEmitter::new();

    let share = start_share(
        source,
        common::send_options_with_addresses(),
        Some(sender_emitter.clone()),
        None,
    )
    .await
    .expect("start_share should succeed");

    let (_cancel_tx, cancel_rx) = common::no_cancel();
    download(
        share.ticket.clone(),
        ReceiveOptions {
            output_dir: Some(recv_dir),
            ..Default::default()
        },
        Some(receiver_emitter.clone()),
        cancel_rx,
    )
    .await
    .expect("download should succeed");

    wait_for_event(&sender_emitter, "transfer-completed").await;

    let sender_payload = sender_emitter
        .events_with_name("transfer-completed")
        .into_iter()
        .find_map(|event| event.payload)
        .expect("transfer-completed should carry a payload");
    let sender_json: serde_json::Value =
        serde_json::from_str(&sender_payload).expect("transfer-completed payload should be JSON");
    assert!(sender_json["durationMs"].is_u64());
    assert_eq!(sender_json["bytes"].as_u64(), Some(2_000_000));

    let receiver_payload = receiver_emitter
        .events_with_name("receive-completed")
        .into_iter()
        .find_map(|event| event.payload)
        .expect("receive-completed should carry a payload");
    let receiver_json: serde_json::Value =
        serde_json::from_str(&receiver_payload).expect("receive-completed payload should be JSON");
    assert!(receiver_json["durationMs"].is_u64());
    assert!(
        receiver_json["exportMs"].is_u64(),
        "disk export time is reported separately from wire time"
    );
    assert_eq!(receiver_json["bytes"].as_u64(), Some(2_000_000));

    drop(share);
}

#[tokio::test]
async fn e2e_a_small_fast_transfer_still_reports_a_duration() {
    // A single-write payload used to report 0ms, which the success screen
    // renders as "NA" for duration and average speed.
    let fixture = TestFixture::new();
    let source = fixture.create_file("tiny.txt", b"small enough to go out in one write");
    let recv_dir = fixture.output_dir();

    let sender_emitter = MockEventEmitter::new();
    let receiver_emitter = MockEventEmitter::new();

    let share = start_share(
        source,
        common::send_options_with_addresses(),
        Some(sender_emitter.clone()),
        None,
    )
    .await
    .expect("start_share should succeed");

    let (_cancel_tx, cancel_rx) = common::no_cancel();
    download(
        share.ticket.clone(),
        ReceiveOptions {
            output_dir: Some(recv_dir),
            ..Default::default()
        },
        Some(receiver_emitter.clone()),
        cancel_rx,
    )
    .await
    .expect("download should succeed");

    wait_for_event(&sender_emitter, "transfer-completed").await;

    for (emitter, event_name) in [
        (&sender_emitter, "transfer-completed"),
        (&receiver_emitter, "receive-completed"),
    ] {
        let payload = emitter
            .events_with_name(event_name)
            .into_iter()
            .find_map(|event| event.payload)
            .unwrap_or_else(|| panic!("{event_name} should carry a payload"));
        let json: serde_json::Value =
            serde_json::from_str(&payload).expect("completion payload should be JSON");
        let duration = json["durationMs"].as_u64().expect("durationMs");
        assert!(
            duration > 0,
            "{event_name} reported {duration}ms, which the UI shows as NA"
        );
    }

    drop(share);
}
