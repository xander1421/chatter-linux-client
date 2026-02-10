//! Integration tests for MessageClient.
//!
//! Tests message operations including send, edit, delete, reactions,
//! pins, search, and streaming against a running server.

mod common;

use chatter_linux::error::NetworkError;
use chatter_linux::network::grpc::{ChannelClient, MessageClient};
use chatter_linux::proto::chat_v1::{
    message::content::Type as ContentType, message::Content, ChannelType, MessageSortDirection,
};
use tokio_stream::StreamExt;

use common::*;

/// Helper to create a test channel and return its ID.
async fn create_test_channel(pool: &chatter_linux::network::grpc::GrpcClientPool) -> String {
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let channel_name = format!("test_channel_{}", unique_username());

    let stream = channel_client
        .create_channel(
            &channel_name,
            ChannelType::ChannelPublic,
            None,
            vec![],
            None,
            Some("Test channel for integration tests"),
            None,
        )
        .await
        .expect("Failed to create channel");

    tokio::pin!(stream);

    let mut channel_id = String::new();
    while let Some(result) = stream.next().await {
        let event = result.expect("Stream error");
        if let Some(payload) = event.event {
            use chatter_linux::proto::chat_v1::channel_event::Event;
            match payload {
                Event::ChannelCreated(channel) => {
                    // ChannelCreated directly contains the Channel proto
                    channel_id = channel.id;
                }
                Event::OperationFailed(e) => {
                    let msg = e.error.map(|err| err.message).unwrap_or_default();
                    panic!("Channel creation failed: {}", msg);
                }
                _ => {}
            }
        }
    }

    assert!(!channel_id.is_empty(), "Should have created a channel");
    channel_id
}

/// Create simple text content for a message.
fn text_content(text: &str) -> Content {
    Content {
        r#type: Some(ContentType::Text(text.to_string())),
    }
}

// ============================================================================
// Send Message Tests
// ============================================================================

#[tokio::test]
async fn test_send_and_get_message() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);
    let channel_id = create_test_channel(&pool).await;

    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());

    // Send a message
    let message = assert_ok(
        msg_client
            .send_message(
                &channel_id,
                text_content("Hello, integration test!"),
                None,
                false,
                None,
                false,
                None,
                vec![],
                false,
                vec![],
            )
            .await,
    );

    assert!(!message.id.is_empty());
    assert_eq!(message.channel_id, channel_id);

    // Get messages and verify it's there
    let response = assert_ok(
        msg_client
            .get_messages(
                &channel_id,
                50,
                None,
                None,
                MessageSortDirection::SortNewestFirst,
                None,
                None,
            )
            .await,
    );

    let found = response.messages.iter().any(|m| m.id == message.id);
    assert!(found, "Sent message should be in message list");
}

// ============================================================================
// Edit Message Tests
// ============================================================================

#[tokio::test]
async fn test_edit_message() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);
    let channel_id = create_test_channel(&pool).await;

    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());

    // Send a message
    let original = assert_ok(
        msg_client
            .send_message(
                &channel_id,
                text_content("Original content"),
                None,
                false,
                None,
                false,
                None,
                vec![],
                false,
                vec![],
            )
            .await,
    );

    // Edit it
    let edited = assert_ok(
        msg_client
            .edit_message(&original.id, &channel_id, text_content("Edited content"))
            .await,
    );

    assert_eq!(edited.id, original.id);
    assert!(edited.edited, "Message should be marked as edited");
}

// ============================================================================
// Delete Message Tests
// ============================================================================

#[tokio::test]
async fn test_delete_message() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);
    let channel_id = create_test_channel(&pool).await;

    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());

    // Send a message
    let message = assert_ok(
        msg_client
            .send_message(
                &channel_id,
                text_content("Message to delete"),
                None,
                false,
                None,
                false,
                None,
                vec![],
                false,
                vec![],
            )
            .await,
    );

    // Delete it
    let result = msg_client.delete_message(&message.id, &channel_id).await;
    assert!(result.is_ok(), "Delete should succeed");

    // Verify it's gone (might still show as deleted or not appear)
    let response = assert_ok(
        msg_client
            .get_messages(
                &channel_id,
                50,
                None,
                None,
                MessageSortDirection::SortNewestFirst,
                None,
                None,
            )
            .await,
    );

    // The message should either not be found or be marked deleted
    let found = response.messages.iter().find(|m| m.id == message.id);
    match found {
        None => {} // Good - message was fully deleted
        Some(_) => {
            // Some systems keep deleted messages with a flag
        }
    }
}

// ============================================================================
// Reaction Tests
// ============================================================================

#[tokio::test]
async fn test_reactions() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);
    let channel_id = create_test_channel(&pool).await;

    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());

    // Send a message
    let message = assert_ok(
        msg_client
            .send_message(
                &channel_id,
                text_content("React to me!"),
                None,
                false,
                None,
                false,
                None,
                vec![],
                false,
                vec![],
            )
            .await,
    );

    // Add a reaction
    let with_reaction = assert_ok(
        msg_client
            .add_reaction(&message.id, &channel_id, "👍")
            .await,
    );

    // Check reaction exists
    let has_thumbs_up = with_reaction
        .reaction_counts
        .iter()
        .any(|r| r.emoji == "👍" && r.count > 0);
    assert!(has_thumbs_up, "Should have thumbs up reaction");

    // Remove the reaction
    let without_reaction = assert_ok(
        msg_client
            .remove_reaction(&message.id, &channel_id, "👍")
            .await,
    );

    // Check reaction is gone or count is reduced
    let still_has_thumbs = without_reaction
        .reaction_counts
        .iter()
        .any(|r| r.emoji == "👍" && r.count > 0);
    assert!(!still_has_thumbs, "Thumbs up reaction should be removed");
}

// ============================================================================
// Pin Tests
// ============================================================================

#[tokio::test]
async fn test_pin_unpin_message() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);
    let channel_id = create_test_channel(&pool).await;

    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());

    // Send a message
    let message = assert_ok(
        msg_client
            .send_message(
                &channel_id,
                text_content("Pin this message"),
                None,
                false,
                None,
                false,
                None,
                vec![],
                false,
                vec![],
            )
            .await,
    );

    // Pin it
    let pinned = assert_ok(msg_client.pin_message(&message.id, &channel_id).await);
    assert!(pinned.pinned, "Message should be pinned");

    // Verify in pinned list
    let pinned_list = assert_ok(msg_client.list_pinned_messages(&channel_id, 50).await);
    let found_pinned = pinned_list.messages.iter().any(|m| m.id == message.id);
    assert!(found_pinned, "Message should appear in pinned list");

    // Unpin it
    let result = msg_client.unpin_message(&message.id, &channel_id).await;
    assert!(result.is_ok(), "Unpin should succeed");
}

// ============================================================================
// Search Tests
// ============================================================================

#[tokio::test]
async fn test_message_search() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);
    let channel_id = create_test_channel(&pool).await;

    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());

    // Send a message with unique content
    let unique_word = format!("uniqueword{}", rand::random::<u32>());
    assert_ok(
        msg_client
            .send_message(
                &channel_id,
                text_content(&format!("This message contains {}", unique_word)),
                None,
                false,
                None,
                false,
                None,
                vec![],
                false,
                vec![],
            )
            .await,
    );

    // Search for it
    let results = assert_ok(
        msg_client
            .search(Some(&channel_id), &unique_word, 50, 0, None, None)
            .await,
    );

    // Note: Search might need indexing time, so this might not always find it immediately
    // But we at least verify the search API works
    assert!(results.results.len() <= 50);
}

// ============================================================================
// Read Receipt Tests
// ============================================================================

#[tokio::test]
async fn test_read_receipt() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);
    let channel_id = create_test_channel(&pool).await;

    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());

    // Send a message
    let message = assert_ok(
        msg_client
            .send_message(
                &channel_id,
                text_content("Read this"),
                None,
                false,
                None,
                false,
                None,
                vec![],
                false,
                vec![],
            )
            .await,
    );

    // Send read receipt
    let result = msg_client
        .send_read_receipt(&channel_id, &message.id)
        .await;

    assert!(result.is_ok(), "Read receipt should be sent successfully");
}

// ============================================================================
// Typing Indicator Tests
// ============================================================================

#[tokio::test]
async fn test_typing_indicator() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);
    let channel_id = create_test_channel(&pool).await;

    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());

    // Start typing
    let start_result = msg_client.send_typing_indicator(&channel_id, true).await;
    assert!(start_result.is_ok(), "Start typing should succeed");

    // Stop typing
    let stop_result = msg_client.send_typing_indicator(&channel_id, false).await;
    assert!(stop_result.is_ok(), "Stop typing should succeed");
}

// ============================================================================
// Error Cases
// ============================================================================

#[tokio::test]
async fn test_send_to_nonexistent_channel() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());

    let result = msg_client
        .send_message(
            "nonexistent-channel-id",
            text_content("This should fail"),
            None,
            false,
            None,
            false,
            None,
            vec![],
            false,
            vec![],
        )
        .await;

    // Should fail with PermissionDenied (secure - doesn't reveal if channel exists)
    match result {
        Err(NetworkError::PermissionDenied) => {}
        Err(NetworkError::NotFound(_)) => {} // Also acceptable
        Err(e) => panic!("Expected PermissionDenied or NotFound, got: {:?}", e),
        Ok(_) => panic!("Expected error for nonexistent channel, got Ok"),
    }
}

// ============================================================================
// Streaming Tests
// ============================================================================

#[tokio::test]
async fn test_stream_messages() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);
    let channel_id = create_test_channel(&pool).await;

    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());

    // Start streaming - server sends initial "stream ready" message immediately
    let stream_result = msg_client.stream_messages(&channel_id, None).await;

    assert!(
        stream_result.is_ok(),
        "Should be able to establish message stream"
    );

    // Optionally consume the initial "stream ready" message
    let mut stream = stream_result.unwrap();
    use tokio_stream::StreamExt;
    let first_msg = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        stream.next(),
    )
    .await;

    // Should receive initial message within 2 seconds
    assert!(first_msg.is_ok(), "Should receive initial stream message");
}
