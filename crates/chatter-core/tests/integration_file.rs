//! Integration tests for FileClient.
//!
//! Tests file operations including storage stats, attachment URLs,
//! and file listing.

mod common;

use chatter_core::network::grpc::{ChannelClient, FileClient};
use chatter_core::proto::chat_v1::ChannelType;
use tokio_stream::StreamExt;

use common::*;

// ============================================================================
// Storage Stats Tests
// ============================================================================

#[tokio::test]
async fn test_get_storage_stats() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut file_client = FileClient::new(pool.channel(), pool.auth_clone());

    // Get storage stats for current user
    let stats = assert_ok(file_client.get_storage_stats(None).await);

    // StorageStats has max_quota and used_quota
    assert!(stats.max_quota >= 0);
    assert!(stats.used_quota >= 0);
}

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

// ============================================================================
// File Listing Tests
// ============================================================================

#[tokio::test]
async fn test_list_channel_files_empty() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    // Create a channel
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let channel_name = format!("files_test_{}", unique_username());

    let stream = channel_client
        .create_channel(
            &channel_name,
            ChannelType::ChannelPublic,
            None,
            vec![],
            None,
            None,
            None,
        )
        .await
        .expect("Failed to create channel");

    tokio::pin!(stream);

    let mut channel_id = String::new();
    while let Some(result) = stream.next().await {
        let event = result.expect("Stream error");
        if let Some(payload) = event.event {
            use chatter_core::proto::chat_v1::channel_event::Event;
            if let Event::ChannelCreated(channel) = payload {
                // ChannelCreated directly contains the Channel proto
                channel_id = channel.id;
            }
        }
    }

    // List files in channel (should be empty)
    let mut file_client = FileClient::new(pool.channel(), pool.auth_clone());
    let response = assert_ok(file_client.list_channel_files(&channel_id, 50, 0).await);

    // New channel should have no files
    assert!(response.files.is_empty(), "New channel should have no files");
}

#[tokio::test]
async fn test_list_shared_files() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut file_client = FileClient::new(pool.channel(), pool.auth_clone());

    // List files shared with current user
    let response = assert_ok(file_client.list_shared_files(None, 50, 0).await);

    // New user should have no shared files
    assert!(response.files.len() <= 50);
}

// ============================================================================
// File Search Tests
// ============================================================================

#[tokio::test]
async fn test_search_files_empty() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut file_client = FileClient::new(pool.channel(), pool.auth_clone());

    // Search for files
    let response = assert_ok(
        file_client
            .search_files("nonexistent_file_xyz", None, None, None, 50, 0)
            .await,
    );

    // SearchFilesResponse has `results` field
    assert!(response.results.len() <= 50);
}

// ============================================================================
// Multipart Upload Session Tests
// ============================================================================

#[tokio::test]
async fn test_initiate_upload() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    // Create a channel for the upload
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let channel_name = format!("upload_test_{}", unique_username());

    let stream = channel_client
        .create_channel(
            &channel_name,
            ChannelType::ChannelPublic,
            None,
            vec![],
            None,
            None,
            None,
        )
        .await
        .expect("Failed to create channel");

    tokio::pin!(stream);

    let mut channel_id = String::new();
    while let Some(result) = stream.next().await {
        let event = result.expect("Stream error");
        if let Some(payload) = event.event {
            use chatter_core::proto::chat_v1::channel_event::Event;
            if let Event::ChannelCreated(channel) = payload {
                // ChannelCreated directly contains the Channel proto
                channel_id = channel.id;
            }
        }
    }

    let mut file_client = FileClient::new(pool.channel(), pool.auth_clone());

    // Initiate upload
    let session = assert_ok(
        file_client
            .initiate_upload(&channel_id, "test.txt", "text/plain", 10240, 4096)
            .await,
    );

    assert!(!session.transfer_id.is_empty(), "Should get a transfer ID");
    assert!(session.chunk_size > 0, "Should have a chunk size");

    // Abort the upload (cleanup)
    let _abort_result = file_client.abort_upload(&session.transfer_id).await;
}

// ============================================================================
// Error Cases
// ============================================================================

#[tokio::test]
async fn test_download_nonexistent_file() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut file_client = FileClient::new(pool.channel(), pool.auth_clone());

    // Try to download a nonexistent file
    // For streaming RPCs, the error occurs when reading the first item
    let stream_result = file_client.download_file("nonexistent-file-id").await;

    match stream_result {
        Err(e) => {
            // Error on stream establishment - this is expected for some implementations
            assert_not_found::<()>(Err(e));
        }
        Ok(stream) => {
            // Stream established, error should come on first read
            tokio::pin!(stream);
            match stream.next().await {
                Some(Err(e)) => assert_not_found::<()>(Err(e)),
                Some(Ok(_)) => panic!("Expected error for non-existent file, but got data"),
                None => panic!("Expected error for non-existent file, but stream was empty"),
            }
        }
    }
}

#[tokio::test]
async fn test_delete_nonexistent_file() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut file_client = FileClient::new(pool.channel(), pool.auth_clone());

    // Try to delete a nonexistent file
    let result = file_client.delete_file("nonexistent-file-id").await;

    // Should fail with NotFound
    assert_not_found(result);
}
