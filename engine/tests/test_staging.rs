//! Receives are staged inside the destination folder, not the system temp dir,
//! so a transfer only ever needs space on the drive the user picked.

mod common;

use common::TestFixture;
use engine::{download, start_share, AddrInfoOptions, ReceiveOptions, SendOptions};
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Bigger than the store's inline threshold, so the blob is a real data file
/// that export has to move or copy.
const BIG: usize = 1024 * 1024;

fn hash_hex(ticket: &str) -> String {
    let parsed = iroh_blobs::ticket::BlobTicket::from_str(ticket).unwrap();
    data_encoding::HEXLOWER.encode(parsed.hash().as_bytes())
}

/// Every entry directly inside `dir`, by name.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names
}

/// A ticket that carries its own addresses, so these tests do not depend on
/// the public DNS discovery service being reachable.
fn send_options() -> SendOptions {
    SendOptions {
        ticket_type: AddrInfoOptions::RelayAndAddresses,
        ..Default::default()
    }
}

async fn wait_until_gone(path: &Path) -> bool {
    for _ in 0..200 {
        if !path.exists() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    !path.exists()
}

async fn receive(ticket: &str, output_dir: &Path) -> anyhow::Result<()> {
    let (_cancel_tx, cancel_rx) = common::no_cancel();
    download(
        ticket.to_string(),
        ReceiveOptions {
            output_dir: Some(output_dir.to_path_buf()),
            ..Default::default()
        },
        None,
        cancel_rx,
    )
    .await
    .map(|_| ())
}

#[tokio::test]
async fn e2e_partial_store_lives_in_the_destination_not_temp() {
    let fixture = TestFixture::new();
    let source = fixture.create_large_file("big.bin", BIG);
    let recv_dir = fixture.output_dir();

    let share = start_share(source, send_options(), None, None)
        .await
        .unwrap();
    let hash = hash_hex(&share.ticket);
    let staged = native::storage::recv_store_dir(&hash, &recv_dir);
    let in_temp = native::storage::temp_recv_store_dir(&hash);
    let _ = std::fs::remove_dir_all(&in_temp);

    // Cancel at once: the store is opened before the cancel is observed.
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
    cancel_tx.send(()).unwrap();
    let result = download(
        share.ticket.clone(),
        ReceiveOptions {
            output_dir: Some(recv_dir.clone()),
            ..Default::default()
        },
        None,
        cancel_rx,
    )
    .await;
    assert!(result.is_err());

    assert_eq!(staged.parent(), Some(recv_dir.as_path()));
    assert!(
        staged.is_dir(),
        "partial store should sit in the destination"
    );
    assert!(!in_temp.exists(), "nothing may be staged in the temp dir");

    let _ = std::fs::remove_dir_all(&staged);
    drop(share);
}

#[tokio::test]
async fn e2e_finished_receive_leaves_only_the_files_behind() {
    let fixture = TestFixture::new();
    let source = fixture.create_large_file("big.bin", BIG);
    let expected = std::fs::read(&source).unwrap();
    let recv_dir = fixture.output_dir();

    let share = start_share(source, send_options(), None, None)
        .await
        .unwrap();
    let hash = hash_hex(&share.ticket);
    let staged = native::storage::recv_store_dir(&hash, &recv_dir);
    let in_temp = native::storage::temp_recv_store_dir(&hash);
    let _ = std::fs::remove_dir_all(&in_temp);

    receive(&share.ticket, &recv_dir).await.expect("download");

    assert_eq!(std::fs::read(recv_dir.join("big.bin")).unwrap(), expected);
    assert!(
        wait_until_gone(&staged).await,
        "staging dir should be removed"
    );
    assert_eq!(entries(&recv_dir), vec!["big.bin".to_string()]);
    assert!(!in_temp.exists());

    drop(share);
}

/// The second copy of a blob cannot be moved out of the store (the first move
/// already took the data file), so it takes the copy path.
#[tokio::test]
async fn e2e_identical_files_in_one_transfer_all_arrive() {
    let fixture = TestFixture::new();
    let payload: Vec<u8> = (0..BIG).map(|i| (i % 241) as u8).collect();
    let source = fixture.create_dir_with_files(
        "twins",
        &[
            ("a.bin", payload.as_slice()),
            ("nested/b.bin", payload.as_slice()),
            ("small.txt", b"tiny"),
        ],
    );
    let recv_dir = fixture.output_dir();

    let share = start_share(source, send_options(), None, None)
        .await
        .unwrap();
    receive(&share.ticket, &recv_dir).await.expect("download");

    let root = recv_dir.join("twins");
    assert_eq!(std::fs::read(root.join("a.bin")).unwrap(), payload);
    assert_eq!(std::fs::read(root.join("nested/b.bin")).unwrap(), payload);
    assert_eq!(std::fs::read(root.join("small.txt")).unwrap(), b"tiny");

    drop(share);
}

/// A destination that cannot host the store must not fail the transfer.
#[tokio::test]
async fn e2e_falls_back_to_temp_when_the_destination_cannot_host_the_store() {
    let fixture = TestFixture::new();
    let source = fixture.create_large_file("big.bin", BIG);
    let expected = std::fs::read(&source).unwrap();
    let recv_dir = fixture.output_dir();

    let share = start_share(source, send_options(), None, None)
        .await
        .unwrap();
    let hash = hash_hex(&share.ticket);
    // A plain file where the store directory would go makes opening it fail.
    let blocker: PathBuf = native::storage::recv_store_dir(&hash, &recv_dir);
    std::fs::write(&blocker, b"not a directory").unwrap();
    let in_temp = native::storage::temp_recv_store_dir(&hash);

    receive(&share.ticket, &recv_dir).await.expect("download");

    assert_eq!(std::fs::read(recv_dir.join("big.bin")).unwrap(), expected);
    assert_eq!(
        std::fs::read(&blocker).unwrap(),
        b"not a directory",
        "whatever was already there is left alone"
    );
    assert!(
        wait_until_gone(&in_temp).await,
        "temp store should be removed"
    );

    drop(share);
}
