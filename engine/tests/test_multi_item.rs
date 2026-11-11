mod common;

use common::TestFixture;
use engine::{download, start_share_items, ReceiveOptions};

#[tokio::test]
async fn e2e_multi_file_roundtrip() {
    let fixture = TestFixture::new();
    let file_a = fixture.create_file("doc.pdf", &vec![0xAA; 5000]);
    let file_b = fixture.create_file("photo.jpg", &vec![0xBB; 3000]);
    let file_c = fixture.create_file("notes.txt", b"some notes here");
    let recv_dir = fixture.output_dir();

    let share = start_share_items(
        vec![file_a, file_b, file_c],
        common::send_options_with_addresses(),
        &None,
        None,
    )
    .await
    .expect("start_share_items should succeed");

    assert_eq!(share.entry_type, "collection");

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

    assert_eq!(
        std::fs::read(recv_dir.join("doc.pdf")).unwrap(),
        vec![0xAA; 5000]
    );
    assert_eq!(
        std::fs::read(recv_dir.join("photo.jpg")).unwrap(),
        vec![0xBB; 3000]
    );
    assert_eq!(
        std::fs::read(recv_dir.join("notes.txt")).unwrap(),
        b"some notes here"
    );

    drop(share);
}

#[tokio::test]
async fn e2e_mixed_files_and_dirs() {
    let fixture = TestFixture::new();
    let single_file = fixture.create_file("standalone.txt", b"I am standalone");
    let dir = fixture.create_dir_with_files(
        "folder",
        &[
            ("inside.txt", b"I am inside a folder"),
            ("sub/deep.txt", b"I am deep inside"),
        ],
    );
    let recv_dir = fixture.output_dir();

    let share = start_share_items(
        vec![single_file, dir],
        common::send_options_with_addresses(),
        &None,
        None,
    )
    .await
    .expect("start_share_items should succeed");

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

    assert_eq!(
        std::fs::read(recv_dir.join("standalone.txt")).unwrap(),
        b"I am standalone"
    );
    assert_eq!(
        std::fs::read(recv_dir.join("folder/inside.txt")).unwrap(),
        b"I am inside a folder"
    );
    assert_eq!(
        std::fs::read(recv_dir.join("folder/sub/deep.txt")).unwrap(),
        b"I am deep inside"
    );

    drop(share);
}
