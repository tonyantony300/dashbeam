mod common;

use common::TestFixture;
use engine::{download, start_share_items, ReceiveOptions};

/// A share session records how many peers pulled the whole payload, so a
/// broadcast history row can say "shared with N devices" rather than
/// attributing one peer's timings to the session.
#[tokio::test]
async fn share_counts_every_peer_that_completed_a_full_pull() {
    let fixture = TestFixture::new();
    let file = fixture.create_file("broadcast.bin", &vec![0xCD; 4096]);

    let share = start_share_items(
        vec![file],
        common::send_options_with_addresses(),
        &None,
        None,
    )
    .await
    .expect("start_share_items should succeed");

    assert_eq!(share.completed_peers(), 0, "nobody has pulled yet");

    for _ in 0..2 {
        let recv_dir = fixture.output_dir();
        let (_cancel_tx, cancel_rx) = common::no_cancel();
        download(
            share.ticket.clone(),
            ReceiveOptions {
                output_dir: Some(recv_dir),
                ..Default::default()
            },
            None,
            cancel_rx,
        )
        .await
        .expect("download should succeed");
    }

    assert_eq!(share.completed_peers(), 2);

    drop(share);
}
