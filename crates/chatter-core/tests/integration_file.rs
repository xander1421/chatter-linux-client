//! Integration tests for FileClient.
//!
//! Tests the Signal-style presigned attachment upload flow.

mod common;

use chatter_core::network::grpc::FileClient;

use common::*;

// ============================================================================
// Attachment URL Tests
// ============================================================================

#[tokio::test]
async fn test_get_attachment_upload_url() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut file_client = FileClient::new(pool.channel(), pool.auth_clone());

    // Request upload URL for a 1KB file
    let response = assert_ok(file_client.get_attachment_upload_url(1024).await);

    assert!(!response.upload_url.is_empty(), "Should get an upload URL");
    assert!(!response.cdn_key.is_empty(), "Should get a CDN key");
}
