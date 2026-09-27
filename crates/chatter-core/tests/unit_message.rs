//! Unit tests for message operations.
//!
//! Tests message sending, receiving, editing, deleting,
//! reactions, pins, and threading using mocked services.

use crate::common;
use crate::framework;

use framework::prelude::*;

// ============================================================================
// Message Creation Tests
// ============================================================================

#[tokio::test]
async fn test_message_builder_text() {
    let msg = MessageBuilder::text("Hello, world!")
        .in_channel("channel-123")
        .from("user-456")
        .build();

    assert_eq!(msg.content, "Hello, world!");
    assert_eq!(msg.channel_id, "channel-123");
    assert_eq!(msg.sender_id, "user-456");
    assert!(!msg.id.is_empty(), "Message should have an ID");
    assert!(msg.timestamp > 0, "Message should have a timestamp");
}

#[tokio::test]
async fn test_message_builder_with_attachments() {
    let attachment = AttachmentBuilder::image("photo.jpg").build();

    let msg = MessageBuilder::text("Check out this photo!")
        .in_channel("channel-123")
        .with_attachment(attachment.clone())
        .build();

    assert_eq!(msg.attachments.len(), 1);
    assert_eq!(msg.attachments[0].filename, "photo.jpg");
}

#[tokio::test]
async fn test_message_builder_encrypted() {
    let msg = MessageBuilder::text("Secret message")
        .in_channel("channel-123")
        .build();

    // Messages should be encrypted by default
    assert!(msg.encrypted, "Messages should be encrypted by default");
}

#[tokio::test]
async fn test_message_builder_reply() {
    let msg = MessageBuilder::text("This is a reply")
        .in_channel("channel-123")
        .reply_to("original-message-id")
        .build();

    assert_eq!(msg.reply_to, Some("original-message-id".to_string()));
}

#[tokio::test]
async fn test_build_conversation() {
    let messages = MessageBuilder::build_conversation(
        vec![
            ("Alice", "Hello Bob!"),
            ("Bob", "Hi Alice!"),
            ("Alice", "How are you?"),
            ("Bob", "I'm great, thanks!"),
        ],
        "channel-1",
    );

    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0].sender_id, "Alice");
    assert_eq!(messages[0].content, "Hello Bob!");
    assert_eq!(messages[1].sender_id, "Bob");
    assert_eq!(messages[1].content, "Hi Alice!");

    // All should be in the same channel
    for msg in &messages {
        assert_eq!(msg.channel_id, "channel-1");
    }

    // Timestamps should be sequential
    for i in 1..messages.len() {
        assert!(
            messages[i].timestamp >= messages[i - 1].timestamp,
            "Timestamps should be sequential"
        );
    }
}

// ============================================================================
// Message Storage Tests
// ============================================================================

#[tokio::test]
async fn test_in_memory_storage_save_and_retrieve() {
    use framework::mocks::InMemoryStorage;

    let storage = InMemoryStorage::new();

    let msg = MessageBuilder::text("Test message")
        .in_channel("channel-1")
        .from("user-1")
        .build();

    let msg_id = msg.id.clone();
    storage.save_message(msg);

    let retrieved = storage.get_message(&msg_id);
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().content, "Test message");
}

#[tokio::test]
async fn test_storage_get_messages_by_channel() {
    use framework::mocks::InMemoryStorage;

    let storage = InMemoryStorage::new();

    // Add messages to different channels
    storage.save_message(
        MessageBuilder::text("Msg 1")
            .in_channel("channel-A")
            .build(),
    );
    storage.save_message(
        MessageBuilder::text("Msg 2")
            .in_channel("channel-A")
            .build(),
    );
    storage.save_message(
        MessageBuilder::text("Msg 3")
            .in_channel("channel-B")
            .build(),
    );

    let channel_a_msgs = storage.get_messages_by_channel("channel-A", 10, None);
    assert_eq!(channel_a_msgs.len(), 2);

    let channel_b_msgs = storage.get_messages_by_channel("channel-B", 10, None);
    assert_eq!(channel_b_msgs.len(), 1);
}

#[tokio::test]
async fn test_storage_delete_message() {
    use framework::mocks::InMemoryStorage;

    let storage = InMemoryStorage::new();

    let msg = MessageBuilder::text("To be deleted")
        .in_channel("channel-1")
        .build();
    let msg_id = msg.id.clone();
    storage.save_message(msg);

    assert!(storage.get_message(&msg_id).is_some());

    let deleted = storage.delete_message(&msg_id);
    assert!(deleted, "Delete should return true");

    assert!(
        storage.get_message(&msg_id).is_none(),
        "Message should be gone"
    );
}

#[tokio::test]
async fn test_storage_pagination() {
    use framework::mocks::InMemoryStorage;

    let storage = InMemoryStorage::new();

    // Add 10 messages
    for i in 0..10 {
        let mut msg = MessageBuilder::text(&format!("Message {}", i))
            .in_channel("channel-1")
            .build();
        // Set increasing timestamps
        msg.timestamp = 1000 + i as i64;
        storage.save_message(msg);
    }

    // Get first 5
    let first_page = storage.get_messages_by_channel("channel-1", 5, None);
    assert_eq!(first_page.len(), 5);

    // Messages should be sorted newest first
    assert!(first_page[0].timestamp > first_page[4].timestamp);
}

// ============================================================================
// Mock Server Message Tests
// ============================================================================

#[tokio::test]
async fn test_mock_server_store_message() {
    use framework::mocks::MockGrpcServer;

    let server = MockGrpcServer::new();

    // Create channel first
    let channel = ChannelBuilder::public("test-channel").build();
    let channel_id = server.create_channel(channel);

    // Store message
    let msg = MessageBuilder::text("Hello!")
        .in_channel(&channel_id)
        .from("user-1")
        .build();
    let msg_id = server.store_message(msg);

    // Retrieve messages
    let messages = server.get_messages(&channel_id, 10);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].id, msg_id);
    assert_eq!(messages[0].content, "Hello!");
}

#[tokio::test]
async fn test_mock_server_multiple_messages() {
    use framework::mocks::MockGrpcServer;

    let server = MockGrpcServer::new();

    let channel = ChannelBuilder::public("test").build();
    let channel_id = server.create_channel(channel);

    // Store multiple messages
    for i in 0..5 {
        let msg = MessageBuilder::text(&format!("Message {}", i))
            .in_channel(&channel_id)
            .build();
        server.store_message(msg);
    }

    let messages = server.get_messages(&channel_id, 10);
    assert_eq!(messages.len(), 5);
}

// ============================================================================
// Reaction Tests
// ============================================================================

#[tokio::test]
async fn test_message_reaction_data() {
    // Test fixture for reaction data
    let msg = MessageBuilder::text("React to me!")
        .in_channel("channel-1")
        .build();

    // Message should start without reactions
    assert!(msg.reactions.is_empty());
}

// ============================================================================
// Thread/Reply Tests
// ============================================================================

#[tokio::test]
async fn test_thread_message_creation() {
    let parent = MessageBuilder::text("Parent message")
        .in_channel("channel-1")
        .build();

    let reply = MessageBuilder::text("Reply to parent")
        .in_channel("channel-1")
        .reply_to(&parent.id)
        .build();

    assert_eq!(reply.reply_to, Some(parent.id.clone()));
}

#[tokio::test]
async fn test_thread_chain() {
    let channel_id = "channel-1";

    let root = MessageBuilder::text("Thread root")
        .in_channel(channel_id)
        .build();

    let reply1 = MessageBuilder::text("First reply")
        .in_channel(channel_id)
        .reply_to(&root.id)
        .build();

    let reply2 = MessageBuilder::text("Second reply")
        .in_channel(channel_id)
        .reply_to(&root.id)
        .build();

    // All replies point to the same root
    assert_eq!(reply1.reply_to, Some(root.id.clone()));
    assert_eq!(reply2.reply_to, Some(root.id.clone()));
}

// ============================================================================
// Harness Integration Tests
// ============================================================================

#[tokio::test]
async fn test_harness_message_flow() {
    let harness = TestHarness::unit().await;
    let (alice, bob) = harness.conversation_pair().await;

    // Create a channel
    let channel = ChannelBuilder::public("test-channel")
        .created_by(alice.user_id())
        .with_member(bob.user_id())
        .build();

    let channel_id = harness.mock_server().create_channel(channel);

    // Alice sends a message
    let msg = MessageBuilder::text("Hello Bob!")
        .in_channel(&channel_id)
        .from(alice.user_id())
        .build();

    harness.mock_server().store_message(msg);

    // Verify message is stored
    let messages = harness.mock_server().get_messages(&channel_id, 10);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].sender_id, alice.user_id());
}

#[tokio::test]
async fn test_harness_storage_operations() {
    let harness = TestHarness::unit().await;

    // Test storage directly
    let msg = MessageBuilder::text("Test")
        .in_channel("channel-1")
        .build();
    let msg_id = msg.id.clone();

    harness.storage.save_message(msg);

    let retrieved = harness.storage.get_message(&msg_id);
    assert!(retrieved.is_some());

    // Reset should clear storage
    harness.reset().await;
    assert_eq!(harness.storage.message_count(), 0);
}

// ============================================================================
// Message Content Type Tests
// ============================================================================

#[tokio::test]
async fn test_text_message() {
    let msg = MessageBuilder::text("Plain text message").build();
    assert_eq!(msg.content, "Plain text message");
}

#[tokio::test]
async fn test_image_message() {
    let msg = MessageBuilder::image().build();

    // Image message should have attachment
    assert_eq!(msg.attachments.len(), 1);
    assert!(msg.attachments[0].mime_type.starts_with("image/"));
}

#[tokio::test]
async fn test_video_message() {
    let msg = MessageBuilder::video().build();

    assert_eq!(msg.attachments.len(), 1);
    assert!(msg.attachments[0].mime_type.starts_with("video/"));
}

// ============================================================================
// Message Timestamp Tests
// ============================================================================

#[tokio::test]
async fn test_message_timestamp_ordering() {
    let msg1 = MessageBuilder::text("First").build();
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let msg2 = MessageBuilder::text("Second").build();

    assert!(
        msg2.timestamp >= msg1.timestamp,
        "Later message should have equal or greater timestamp"
    );
}

#[tokio::test]
async fn test_unique_message_ids() {
    let msg1 = MessageBuilder::text("Message 1").build();
    let msg2 = MessageBuilder::text("Message 2").build();

    assert_ne!(msg1.id, msg2.id, "Each message should have unique ID");
}

// ============================================================================
// Attachment Tests
// ============================================================================

#[tokio::test]
async fn test_attachment_builder_image() {
    let attachment = AttachmentBuilder::image("photo.jpg").build();

    assert_eq!(attachment.filename, "photo.jpg");
    assert_eq!(attachment.mime_type, "image/jpeg");
    assert!(!attachment.key.is_empty(), "Should have encryption key");
    assert!(!attachment.digest.is_empty(), "Should have digest");
}

#[tokio::test]
async fn test_attachment_builder_video() {
    let attachment = AttachmentBuilder::video("video.mp4").build();

    assert_eq!(attachment.filename, "video.mp4");
    assert_eq!(attachment.mime_type, "video/mp4");
    assert_eq!(attachment.size, 10 * 1024 * 1024); // 10MB default
}

#[tokio::test]
async fn test_attachment_builder_audio() {
    let attachment = AttachmentBuilder::audio("song.mp3").build();

    assert_eq!(attachment.filename, "song.mp3");
    assert_eq!(attachment.mime_type, "audio/mp3");
}

#[tokio::test]
async fn test_attachment_builder_document() {
    let attachment = AttachmentBuilder::document("report.pdf").build();

    assert_eq!(attachment.filename, "report.pdf");
    assert_eq!(attachment.mime_type, "application/pdf");
}

#[tokio::test]
async fn test_attachment_custom_size() {
    let attachment = AttachmentBuilder::image("large.jpg")
        .size(50 * 1024 * 1024) // 50MB
        .build();

    assert_eq!(attachment.size, 50 * 1024 * 1024);
}

#[tokio::test]
async fn test_attachment_crypto_keys() {
    let att1 = AttachmentBuilder::image("1.jpg").build();
    let att2 = AttachmentBuilder::image("2.jpg").build();

    // Each attachment should have unique encryption keys
    assert_ne!(att1.key, att2.key, "Encryption keys should be unique");
    assert_ne!(att1.digest, att2.digest, "Digests should be unique");
}

// ============================================================================
// Message Validation Tests
// ============================================================================

#[tokio::test]
async fn test_empty_message_content() {
    let msg = MessageBuilder::text("").build();
    assert_eq!(msg.content, "");
}

#[tokio::test]
async fn test_unicode_message_content() {
    let content = "Hello 世界! 🎉 Привет мир! مرحبا";
    let msg = MessageBuilder::text(content).build();
    assert_eq!(msg.content, content);
}

#[tokio::test]
async fn test_long_message_content() {
    let long_content = "A".repeat(10_000);
    let msg = MessageBuilder::text(&long_content).build();
    assert_eq!(msg.content.len(), 10_000);
}
