mod common;

use common::{MockEventEmitter, TestFixture};
use engine::{download, start_share, ReceiveOptions};
use std::str::FromStr;

/// Where the receiver keeps blobs for this ticket — the path comes from its hash.
fn receiver_temp_dir(ticket: &str) -> std::path::PathBuf {
    let parsed = iroh_blobs::ticket::BlobTicket::from_str(ticket).unwrap();
    native::storage::temp_dir().join(format!(
        ".dashbeam-recv-{}",
        data_encoding::HEXLOWER.encode(parsed.hash().as_bytes())
    ))
}

/// Add up the size of every file under `path`.
fn dir_size(path: &std::path::Path) -> u64 {
    let mut total = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                total += dir_size(&p);
            } else if let Ok(meta) = std::fs::metadata(&p) {
                total += meta.len();
            }
        }
    }
    total
}

/// Bytes-so-far from the latest `receive-progress` event (payload is `bytes:total:speed`).
fn latest_progress_bytes(emitter: &MockEventEmitter) -> u64 {
    emitter
        .events_with_name("receive-progress")
        .into_iter()
        .rev()
        .find_map(|e| e.payload)
        .and_then(|p| p.split(':').next().and_then(|s| s.parse().ok()))
        .unwrap_or(0)
}

/// Wait for `path` to disappear. Cleanup runs on a background thread, so it
/// won't always be gone the moment we drop.
async fn wait_until_gone(path: &std::path::Path) -> bool {
    for _ in 0..200 {
        if !path.exists() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    !path.exists()
}

#[tokio::test]
async fn e2e_sender_temp_dir_cleanup() {
    let fixture = TestFixture::new();
    let source = fixture.create_file("dummy.txt", b"dummy content");

    let share = start_share(source, common::send_options_with_addresses(), None, None)
        .await
        .expect("start_share should succeed");

    // Capture the temp directory path created by start_share
    let temp_dir_path = share.blobs_data_dir.path().to_path_buf();

    // Verify it exists while share is active
    assert!(
        temp_dir_path.exists(),
        "Temp dir should exist while share is active"
    );

    // Dropping the share kicks off cleanup.
    drop(share);

    // Verify it is deleted (cleanup runs on a blocking task).
    assert!(
        wait_until_gone(&temp_dir_path).await,
        "Temp dir should be deleted after SendResult is dropped"
    );
}

#[tokio::test]
async fn e2e_receiver_temp_dir_preserved_on_failure() {
    let fixture = TestFixture::new();
    let recv_dir = fixture.output_dir();

    // Start a real share to get a valid ticket, then drop the sender so the
    // download fails when it tries to connect.
    let source = fixture.create_file("fail.txt", b"will fail");
    let share = start_share(source, common::send_options_with_addresses(), None, None)
        .await
        .unwrap();
    let ticket = share.ticket.clone();
    let expected_path = receiver_temp_dir(&ticket);

    // Drop the sender immediately so the receiver cannot connect
    drop(share);

    let mut options = ReceiveOptions::default();
    options.output_dir = Some(recv_dir);

    // The download should fail because the sender is gone
    let (_cancel_tx, cancel_rx) = common::no_cancel();
    let result = download(ticket, options, None, cancel_rx).await;
    assert!(
        result.is_err(),
        "Download should fail since sender was dropped"
    );

    // Should still be here after the failure so the next try can pick up where it left off.
    assert!(
        expected_path.exists(),
        "Receiver temp dir should be preserved on failure for resumability"
    );

    let _ = std::fs::remove_dir_all(&expected_path);
}

#[tokio::test]
async fn e2e_receiver_temp_dir_removed_on_success() {
    let fixture = TestFixture::new();
    let source = fixture.create_file("success.txt", b"completed transfer payload");
    let recv_dir = fixture.output_dir();

    let share = start_share(source, common::send_options_with_addresses(), None, None)
        .await
        .unwrap();
    let ticket = share.ticket.clone();
    let expected_path = receiver_temp_dir(&ticket);

    let (_cancel_tx, cancel_rx) = common::no_cancel();
    let result = download(
        ticket,
        ReceiveOptions {
            output_dir: Some(recv_dir.clone()),
            ..Default::default()
        },
        None,
        cancel_rx,
    )
    .await
    .expect("download should succeed");
    assert!(!result.message.is_empty());

    // File arrived intact.
    assert_eq!(
        std::fs::read(recv_dir.join("success.txt")).unwrap(),
        b"completed transfer payload"
    );

    // Once the download finishes cleanly, the temp store is gone.
    assert!(
        wait_until_gone(&expected_path).await,
        "Receiver temp dir should be removed after a successful download"
    );

    drop(share);
}

/// Cancelling via the cancel channel preserves the partial store so the same
/// ticket can be resumed in the same session.
#[tokio::test]
async fn e2e_cancel_preserves_partial_store() {
    let fixture = TestFixture::new();
    let source = fixture.create_file("cancel_test.txt", b"content to be cancelled");

    let share = start_share(source, common::send_options_with_addresses(), None, None)
        .await
        .unwrap();
    let ticket = share.ticket.clone();
    let expected_path = receiver_temp_dir(&ticket);
    let _ = std::fs::remove_dir_all(&expected_path); // start clean

    // Send cancel immediately — the store is created before the select! runs,
    // so the partial dir will exist regardless of how fast the cancel fires.
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
    cancel_tx.send(()).unwrap();

    let result = download(
        ticket.clone(),
        ReceiveOptions {
            output_dir: Some(fixture.output_dir()),
            ..Default::default()
        },
        None,
        cancel_rx,
    )
    .await;

    assert!(result.is_err(), "cancelled download should return an error");
    assert!(
        result.unwrap_err().to_string().contains("cancelled"),
        "error message should indicate cancellation"
    );

    // Partial store must survive so the user can resume by re-entering the same ticket.
    assert!(
        expected_path.exists(),
        "partial recv store should be preserved after cancel for same-session resume"
    );

    let _ = std::fs::remove_dir_all(&expected_path);
    drop(share);
}

/// After a cancel, resuming with the same ticket completes the transfer successfully.
#[tokio::test]
async fn e2e_same_ticket_resumes_after_cancel() {
    let fixture = TestFixture::new();
    let source = fixture.create_file("resume.txt", b"resume after cancel content");
    let recv_dir = fixture.output_dir();

    let share = start_share(source, common::send_options_with_addresses(), None, None)
        .await
        .unwrap();
    let ticket = share.ticket.clone();
    let partial_path = receiver_temp_dir(&ticket);
    let _ = std::fs::remove_dir_all(&partial_path);

    // First attempt: cancel immediately.
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
    cancel_tx.send(()).unwrap();
    let _ = download(
        ticket.clone(),
        ReceiveOptions {
            output_dir: Some(recv_dir.clone()),
            ..Default::default()
        },
        None,
        cancel_rx,
    )
    .await;

    assert!(
        partial_path.exists(),
        "partial store must exist before retry"
    );

    // Second attempt with same ticket: must succeed and produce the correct file.
    let (_cancel_tx2, cancel_rx2) = common::no_cancel();
    download(
        ticket,
        ReceiveOptions {
            output_dir: Some(recv_dir.clone()),
            ..Default::default()
        },
        None,
        cancel_rx2,
    )
    .await
    .expect("retry with same ticket should succeed");

    assert_eq!(
        std::fs::read(recv_dir.join("resume.txt")).unwrap(),
        b"resume after cancel content"
    );

    // Temp store cleaned up after successful completion.
    assert!(
        wait_until_gone(&partial_path).await,
        "partial store should be removed after successful completion"
    );

    drop(share);
}

/// Interrupt a real transfer mid-flight, then retry and confirm it resumes from
/// the preserved partial store instead of starting over.
///
/// Timing-dependent (localhost transfers are fast), so it's not run in CI. Run with:
///   cargo test --test test_cleanup e2e_receiver_resumes_partial_download -- --ignored --nocapture
#[tokio::test]
#[ignore = "timing-dependent; run manually"]
async fn e2e_receiver_resumes_partial_download() {
    let fixture = TestFixture::new();
    // Large enough that the transfer can be interrupted before it completes.
    let size = 128 * 1024 * 1024; // 128 MiB
    let source = fixture.create_large_file("big.bin", size);
    let recv_dir = fixture.output_dir();

    let share = start_share(
        source.clone(),
        common::send_options_with_addresses(),
        None,
        None,
    )
    .await
    .unwrap();
    let ticket = share.ticket.clone();
    let expected_path = receiver_temp_dir(&ticket);
    // Start clean so the partial-size assertion is meaningful.
    let _ = std::fs::remove_dir_all(&expected_path);

    // Receive in the background and watch progress so we can cut the sender mid-transfer.
    let emitter = MockEventEmitter::new();
    let (_cancel_tx, cancel_rx) = common::no_cancel();
    let recv_task = tokio::spawn(download(
        ticket.clone(),
        ReceiveOptions {
            output_dir: Some(recv_dir.clone()),
            ..Default::default()
        },
        Some(emitter.clone()),
        cancel_rx,
    ));

    // Wait until a few MiB have transferred.
    let mut transferred = 0u64;
    for _ in 0..200 {
        if recv_task.is_finished() {
            break;
        }
        transferred = latest_progress_bytes(&emitter);
        if transferred > 4 * 1024 * 1024 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        transferred > 0 && !recv_task.is_finished(),
        "transfer finished before it could be interrupted; rerun with a larger file"
    );

    // Kill the sender mid-transfer -> the receiver's connection breaks.
    drop(share);

    let first = recv_task.await.expect("receive task panicked");
    assert!(first.is_err(), "interrupted attempt should fail");

    // Partial progress survived on disk.
    assert!(
        expected_path.exists(),
        "partial dir should survive the failure"
    );
    let partial = dir_size(&expected_path);
    assert!(
        partial > 0,
        "some partial bytes should be saved (got {partial})"
    );

    // Re-share the same content. The node id (and thus the full ticket string)
    // differs per session, but the content hash — which keys the temp dir — is
    // the same, so the retry targets the preserved partial store.
    let share2 = start_share(source, common::send_options_with_addresses(), None, None)
        .await
        .unwrap();
    assert_eq!(
        receiver_temp_dir(&share2.ticket),
        expected_path,
        "re-share must map to the same partial-download dir"
    );

    // Retry completes by resuming from the saved progress.
    let (_cancel_tx2, cancel_rx2) = common::no_cancel();
    download(
        share2.ticket.clone(),
        ReceiveOptions {
            output_dir: Some(recv_dir.clone()),
            ..Default::default()
        },
        None,
        cancel_rx2,
    )
    .await
    .expect("retry should resume and complete");

    // Final file is correct and the temp store is cleaned up on success.
    let received = std::fs::metadata(recv_dir.join("big.bin")).unwrap().len();
    assert_eq!(received as usize, size, "final file size should match");
    assert!(
        wait_until_gone(&expected_path).await,
        "temp dir should be removed after successful completion"
    );

    drop(share2);
}
