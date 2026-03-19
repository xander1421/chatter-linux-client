//! Unit tests for inbox processing and event handling.
//!
//! Tests inbox message processing, event handlers,
//! and message routing using the testing framework.

mod common;
mod framework;

use framework::prelude::*;

// ============================================================================
// Message Content Type Tests
// ============================================================================

#[tokio::test]
async fn test_text_message_content() {
    let msg = MessageBuilder::text("Hello, world!").build();

    assert_eq!(msg.content, "Hello, world!");
    assert!(msg.attachments.is_empty());
}

#[tokio::test]
async fn test_image_message_content() {
    let msg = MessageBuilder::image().build();

    assert_eq!(msg.attachments.len(), 1);
    assert!(msg.attachments[0].mime_type.starts_with("image/"));
}

#[tokio::test]
async fn test_video_message_content() {
    let msg = MessageBuilder::video().build();

    assert_eq!(msg.attachments.len(), 1);
    assert!(msg.attachments[0].mime_type.starts_with("video/"));
}

// ============================================================================
// Message Processing Flow Tests
// ============================================================================

#[tokio::test]
async fn test_message_storage_after_receive() {
    let harness = TestHarness::unit().await;

    let channel = ChannelBuilder::public("inbox-test").build();
    let channel_id = harness.mock_server().create_channel(channel);

    // Simulate receiving a message
    let msg = MessageBuilder::text("Received message")
        .in_channel(&channel_id)
        .from("sender-123")
        .build();

    // Store in mock server (simulating inbox processing)
    harness.mock_server().store_message(msg.clone());

    // Verify message was stored
    let messages = harness.mock_server().get_messages(&channel_id, 10);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, "Received message");
}

#[tokio::test]
async fn test_message_from_different_senders() {
    let harness = TestHarness::unit().await;
    let (alice, bob) = harness.conversation_pair().await;

    let channel = ChannelBuilder::public("chat")
        .created_by(alice.user_id())
        .with_member(bob.user_id())
        .build();
    let channel_id = harness.mock_server().create_channel(channel);

    // Messages from Alice
    let msg1 = MessageBuilder::text("From Alice")
        .in_channel(&channel_id)
        .from(alice.user_id())
        .build();
    harness.mock_server().store_message(msg1);

    // Messages from Bob
    let msg2 = MessageBuilder::text("From Bob")
        .in_channel(&channel_id)
        .from(bob.user_id())
        .build();
    harness.mock_server().store_message(msg2);

    let messages = harness.mock_server().get_messages(&channel_id, 10);
    assert_eq!(messages.len(), 2);

    let senders: Vec<_> = messages.iter().map(|m| m.sender_id.as_str()).collect();
    assert!(senders.contains(&alice.user_id()));
    assert!(senders.contains(&bob.user_id()));
}

// ============================================================================
// Reply/Thread Processing Tests
// ============================================================================

#[tokio::test]
async fn test_reply_message_processing() {
    let harness = TestHarness::unit().await;

    let channel = ChannelBuilder::public("thread-test").build();
    let channel_id = harness.mock_server().create_channel(channel);

    // Original message
    let original = MessageBuilder::text("Original post")
        .in_channel(&channel_id)
        .build();
    let original_id = original.id.clone();
    harness.mock_server().store_message(original);

    // Reply message
    let reply = MessageBuilder::text("This is a reply")
        .in_channel(&channel_id)
        .reply_to(&original_id)
        .build();
    harness.mock_server().store_message(reply.clone());

    // Verify reply structure
    assert_eq!(reply.reply_to, Some(original_id));
}

// ============================================================================
// Attachment Processing Tests
// ============================================================================

#[tokio::test]
async fn test_attachment_message_processing() {
    let harness = TestHarness::unit().await;

    let channel = ChannelBuilder::public("media-channel").build();
    let channel_id = harness.mock_server().create_channel(channel);

    let attachment = AttachmentBuilder::image("photo.jpg").build();
    let msg = MessageBuilder::text("Check this out!")
        .in_channel(&channel_id)
        .with_attachment(attachment)
        .build();

    harness.mock_server().store_message(msg);

    let messages = harness.mock_server().get_messages(&channel_id, 10);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].attachments.len(), 1);
}

#[tokio::test]
async fn test_multiple_attachments_processing() {
    let harness = TestHarness::unit().await;

    let channel = ChannelBuilder::public("gallery").build();
    let channel_id = harness.mock_server().create_channel(channel);

    let img1 = AttachmentBuilder::image("photo1.jpg").build();
    let img2 = AttachmentBuilder::image("photo2.jpg").build();
    let img3 = AttachmentBuilder::image("photo3.jpg").build();

    let msg = MessageBuilder::text("Photo album")
        .in_channel(&channel_id)
        .with_attachment(img1)
        .with_attachment(img2)
        .with_attachment(img3)
        .build();

    harness.mock_server().store_message(msg);

    let messages = harness.mock_server().get_messages(&channel_id, 10);
    assert_eq!(messages[0].attachments.len(), 3);
}

// ============================================================================
// Message Ordering Tests
// ============================================================================

#[tokio::test]
async fn test_message_timestamp_ordering() {
    let harness = TestHarness::unit().await;

    let channel = ChannelBuilder::public("ordered-chat").build();
    let channel_id = harness.mock_server().create_channel(channel);

    // Send messages with small delays
    for i in 0..5 {
        let mut msg = MessageBuilder::text(&format!("Message {}", i))
            .in_channel(&channel_id)
            .build();
        // Ensure increasing timestamps
        msg.timestamp = 1000 + i as i64;
        harness.mock_server().store_message(msg);
    }

    let messages = harness.mock_server().get_messages(&channel_id, 10);

    // Should be sorted newest first
    for i in 1..messages.len() {
        assert!(
            messages[i - 1].timestamp >= messages[i].timestamp,
            "Messages should be sorted newest first"
        );
    }
}

// ============================================================================
// Channel Event Tests
// ============================================================================

#[tokio::test]
async fn test_channel_creation_event() {
    let harness = TestHarness::unit().await;

    let channel = ChannelBuilder::public("new-channel")
        .with_description("A newly created channel")
        .build();

    let channel_id = harness.mock_server().create_channel(channel);

    // Verify channel was created
    let retrieved = harness.mock_server().get_channel(&channel_id);
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().name, "new-channel");
}

// ============================================================================
// User Event Tests
// ============================================================================

#[tokio::test]
async fn test_user_registration_event() {
    let harness = TestHarness::unit().await;

    let user = harness.register_user("NewUser").await;

    // Verify user was registered
    let retrieved = harness.mock_server().get_user(&user.user_id);
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().display_name, "NewUser");
}

// ============================================================================
// Session Event Tests
// ============================================================================

#[tokio::test]
async fn test_session_creation_event() {
    let harness = TestHarness::unit().await;
    let user = harness.register_user("SessionUser").await;

    let token = harness.mock_server().create_session(&user.user_id, user.device_id);

    let session = harness.mock_server().validate_session(&token);
    assert!(session.is_some());
}

// ============================================================================
// Message Deduplication Tests
// ============================================================================

#[tokio::test]
async fn test_unique_message_ids() {
    let msg1 = MessageBuilder::text("First").build();
    let msg2 = MessageBuilder::text("Second").build();
    let msg3 = MessageBuilder::text("Third").build();

    assert_ne!(msg1.id, msg2.id);
    assert_ne!(msg2.id, msg3.id);
    assert_ne!(msg1.id, msg3.id);
}

// ============================================================================
// Encryption Flag Tests
// ============================================================================

#[tokio::test]
async fn test_messages_are_encrypted() {
    let msg = MessageBuilder::text("Secret").build();
    assert!(msg.encrypted, "Messages should be encrypted by default");
}

// ============================================================================
// Storage Integration Tests
// ============================================================================

#[tokio::test]
async fn test_inbox_storage_flow() {
    use framework::mocks::InMemoryStorage;

    let storage = InMemoryStorage::new();

    // Simulate receiving messages
    let msg1 = MessageBuilder::text("Inbox message 1")
        .in_channel("channel-1")
        .build();
    let msg2 = MessageBuilder::text("Inbox message 2")
        .in_channel("channel-1")
        .build();

    storage.save_message(msg1);
    storage.save_message(msg2);

    let messages = storage.get_messages_by_channel("channel-1", 10, None);
    assert_eq!(messages.len(), 2);
}

#[tokio::test]
async fn test_inbox_message_retrieval_by_id() {
    use framework::mocks::InMemoryStorage;

    let storage = InMemoryStorage::new();

    let msg = MessageBuilder::text("Specific message")
        .in_channel("channel-1")
        .build();
    let msg_id = msg.id.clone();

    storage.save_message(msg);

    let retrieved = storage.get_message(&msg_id);
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().content, "Specific message");
}

// ============================================================================
// Mock Trait Tests
// ============================================================================

#[tokio::test]
async fn test_mock_result_ok() {
    use framework::mocks::MockResult;

    let result: MockResult<String> = Ok("success".to_string());
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "success");
}

#[tokio::test]
async fn test_mock_result_err() {
    use framework::mocks::{MockResult, MockError};

    let result: MockResult<String> = Err(MockError::NotFound("item".to_string()));
    assert!(result.is_err());
}

// ============================================================================
// Conversation Flow Tests
// ============================================================================

#[tokio::test]
async fn test_full_conversation_flow() {
    let harness = TestHarness::unit().await;
    let (alice, bob) = harness.conversation_pair().await;

    // Create DM channel
    let dm = ChannelBuilder::build_dm_between(alice.user_id(), bob.user_id());
    let channel_id = harness.mock_server().create_channel(dm);

    // Simulate conversation
    let conversation = MessageBuilder::build_conversation(
        vec![
            (alice.user_id(), "Hey Bob!"),
            (bob.user_id(), "Hi Alice!"),
            (alice.user_id(), "How are you?"),
            (bob.user_id(), "Great, thanks!"),
        ],
        &channel_id,
    );

    for msg in conversation {
        harness.mock_server().store_message(msg);
    }

    let messages = harness.mock_server().get_messages(&channel_id, 10);
    assert_eq!(messages.len(), 4);
}

// ============================================================================
// Multi-Channel Tests
// ============================================================================

#[tokio::test]
async fn test_messages_isolated_by_channel() {
    let harness = TestHarness::unit().await;

    let channel1 = ChannelBuilder::public("channel-1").build();
    let channel2 = ChannelBuilder::public("channel-2").build();

    let id1 = harness.mock_server().create_channel(channel1);
    let id2 = harness.mock_server().create_channel(channel2);

    // Add messages to each channel
    harness.mock_server().store_message(
        MessageBuilder::text("Message in channel 1").in_channel(&id1).build()
    );
    harness.mock_server().store_message(
        MessageBuilder::text("Message in channel 2").in_channel(&id2).build()
    );

    // Each channel should only have its messages
    let msgs1 = harness.mock_server().get_messages(&id1, 10);
    let msgs2 = harness.mock_server().get_messages(&id2, 10);

    assert_eq!(msgs1.len(), 1);
    assert_eq!(msgs2.len(), 1);
    assert_eq!(msgs1[0].content, "Message in channel 1");
    assert_eq!(msgs2[0].content, "Message in channel 2");
}
