mod common;

use common::TestFixture;
use engine::{fetch_metadata, start_share, start_share_items, FileMetadata, ReceiveOptions};

#[tokio::test]
async fn e2e_metadata_preview() {
    let fixture = TestFixture::new();
    let content = b"preview content here";
    let source = fixture.create_file("preview_test.txt", content);

    let metadata = FileMetadata {
        file_name: "preview_test.txt".into(),
        item_count: 1,
        size: content.len() as u64,
        thumbnail: Some("data:image/png;base64,dGVzdA==".into()),
        mime_type: Some("text/plain".into()),
        items: None,
    };

    let share = start_share(
        source,
        common::send_options_with_addresses(),
        None,
        Some(metadata.clone()),
    )
    .await
    .expect("start_share should succeed");

    let fetched = fetch_metadata(share.ticket.clone(), ReceiveOptions::default())
        .await
        .expect("fetch_metadata should succeed");

    assert_eq!(fetched.file_name, "preview_test.txt");
    assert_eq!(fetched.size, content.len() as u64);
    assert_eq!(fetched.mime_type, Some("text/plain".into()));
    assert_eq!(
        fetched.thumbnail,
        Some("data:image/png;base64,dGVzdA==".into())
    );
    assert_eq!(fetched.item_count, 1);

    drop(share);
}

#[tokio::test]
async fn e2e_metadata_multi_item() {
    let fixture = TestFixture::new();
    let content_a = b"aaa";
    let content_b = b"bbb";
    let file_a = fixture.create_file("a.txt", content_a);
    let file_b = fixture.create_file("b.txt", content_b);

    let metadata = FileMetadata {
        file_name: "2 items".into(),
        item_count: 2,
        size: (content_a.len() + content_b.len()) as u64,
        thumbnail: None,
        mime_type: None,
        items: None,
    };

    let share = start_share_items(
        vec![file_a, file_b],
        common::send_options_with_addresses(),
        &None,
        Some(metadata),
    )
    .await
    .expect("start_share_items should succeed");

    let fetched = fetch_metadata(share.ticket.clone(), ReceiveOptions::default())
        .await
        .expect("fetch_metadata should succeed");

    assert_eq!(fetched.file_name, "2 items");
    assert_eq!(fetched.item_count, 2);
    assert_eq!(fetched.size, (content_a.len() + content_b.len()) as u64);
    assert_eq!(
        fetched.thumbnail, None,
        "multi-item share should have no thumbnail"
    );
    assert_eq!(
        fetched.mime_type, None,
        "multi-item share should have no mime_type"
    );

    drop(share);
}
