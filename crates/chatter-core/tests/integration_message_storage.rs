//! Integration tests for message storage.
//!
//! Verifies that all message flows properly persist to the database:
//! - DM messages (sent and received)
//! - Channel messages (sent and received)
//! - System messages (member join/leave)
//! - Saved messages (Note to Self)
//! - Signal-style (sender_id, sent_at) lookups

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tempfile::TempDir;

use chatter_core::domain::channel::{Channel, ChannelType};
use chatter_core::domain::ids::{ChannelId, MessageId, UserId};
use chatter_core::domain::message::{ConversationType, Message, MessageContent};
use chatter_core::storage::{ChannelRepository, Database, MessageRepository};

// ============================================================================
// Test Setup Helpers
// ============================================================================

/// Atomic counter for generating unique timestamps in tests.
static TIMESTAMP_COUNTER: AtomicU64 = AtomicU64::new(1_700_000_000_000_000_000);

/// Generate a unique nanosecond timestamp for testing.
/// Each call returns a timestamp ~1ms later than the previous.
fn next_timestamp_ns() -> u64 {
    TIMESTAMP_COUNTER.fetch_add(1_000_000, Ordering::SeqCst)
}

fn setup_test_db() -> (TempDir, Arc<Database>) {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test_messages.db");
    let db = Database::open(&db_path, "test_passphrase").unwrap();
    db.run_migrations().unwrap();
    (temp_dir, Arc::new(db))
}

fn create_test_channel(channel_repo: &ChannelRepository, channel_type: ChannelType) -> Channel {
    let channel = Channel::new(
        ChannelId::new(),
        format!("Test Channel {:?}", channel_type),
        channel_type,
        UserId::new(),
    );
    channel_repo.insert(&channel).unwrap();
    channel
}

fn create_dm_channel(channel_repo: &ChannelRepository, user_a: &UserId, user_b: &UserId) -> Channel {
    let channel_id = ChannelId::from_dm_participants(user_a, user_b);
    let channel = Channel::new(
        channel_id,
        format!("DM with {}", user_b),
        ChannelType::DirectMessage,
        *user_a,
    );
    channel_repo.insert(&channel).unwrap();
    channel
}

// ============================================================================
// DM Message Storage Tests
// ============================================================================

#[test]
fn test_dm_message_storage_roundtrip() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    // Create two users
    let alice = UserId::new();
    let bob = UserId::new();

    // Create DM channel
    let dm_channel = create_dm_channel(&channel_repo, &alice, &bob);

    // Store a sent message (Alice -> Bob)
    let sent_at = next_timestamp_ns();
    let sent_msg = Message::new_text(dm_channel.id, ConversationType::MailboxDm, Some(alice), "Hello Bob!".to_string(), sent_at);
    message_repo.insert(&sent_msg).unwrap();

    // Verify message is stored
    let loaded = message_repo.get(&sent_msg.id).unwrap();
    assert!(loaded.is_some());
    let loaded_msg = loaded.unwrap();
    assert_eq!(loaded_msg.conversation_id, dm_channel.id);
    assert_eq!(loaded_msg.sender_id, Some(alice));
    assert_eq!(loaded_msg.sent_at, sent_at);
    match &loaded_msg.content {
        MessageContent::Text(text) => assert_eq!(text, "Hello Bob!"),
        _ => panic!("Expected text message"),
    }
}

#[test]
fn test_dm_message_received_storage() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let alice = UserId::new();
    let bob = UserId::new();

    // Create DM channel (simulating what happens when receiving a DM)
    let dm_channel = create_dm_channel(&channel_repo, &alice, &bob);

    // Simulate receiving a message from Bob
    let sent_at = next_timestamp_ns();
    let received_msg = Message::new_text(dm_channel.id, ConversationType::MailboxDm, Some(bob), "Hi Alice!".to_string(), sent_at);

    // Use insert_or_ignore like the polling code does
    let inserted = message_repo.insert_or_ignore(&received_msg).unwrap();
    assert!(inserted, "Message should be inserted");

    // Try inserting duplicate - should be ignored
    let duplicate_inserted = message_repo.insert_or_ignore(&received_msg).unwrap();
    assert!(!duplicate_inserted, "Duplicate message should be ignored");

    // Verify only one message exists
    let messages = message_repo.list_for_channel(&dm_channel.id, &ConversationType::MailboxDm, None, 100).unwrap();
    assert_eq!(messages.len(), 1);
}

#[test]
fn test_dm_channel_derived_id_consistency() {
    let alice = UserId::new();
    let bob = UserId::new();

    // Derived channel ID should be the same regardless of order
    let channel_id_1 = ChannelId::from_dm_participants(&alice, &bob);
    let channel_id_2 = ChannelId::from_dm_participants(&bob, &alice);

    assert_eq!(channel_id_1, channel_id_2, "DM channel ID should be deterministic");
}

#[test]
fn test_dm_messages_list_by_channel() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let alice = UserId::new();
    let bob = UserId::new();
    let charlie = UserId::new();

    // Create two DM channels
    let dm_ab = create_dm_channel(&channel_repo, &alice, &bob);
    let dm_ac = create_dm_channel(&channel_repo, &alice, &charlie);

    // Store messages in both channels
    let msg1 = Message::new_text(dm_ab.id, ConversationType::MailboxDm, Some(alice), "Hi Bob".to_string(), next_timestamp_ns());
    let msg2 = Message::new_text(dm_ab.id, ConversationType::MailboxDm, Some(bob), "Hi Alice".to_string(), next_timestamp_ns());
    let msg3 = Message::new_text(dm_ac.id, ConversationType::MailboxDm, Some(alice), "Hi Charlie".to_string(), next_timestamp_ns());

    message_repo.insert(&msg1).unwrap();
    message_repo.insert(&msg2).unwrap();
    message_repo.insert(&msg3).unwrap();

    // List messages for Alice-Bob channel
    let ab_messages = message_repo.list_for_channel(&dm_ab.id, &ConversationType::MailboxDm, None, 100).unwrap();
    assert_eq!(ab_messages.len(), 2);

    // List messages for Alice-Charlie channel
    let ac_messages = message_repo.list_for_channel(&dm_ac.id, &ConversationType::MailboxDm, None, 100).unwrap();
    assert_eq!(ac_messages.len(), 1);
}

// ============================================================================
// Channel Message Storage Tests
// ============================================================================

#[test]
fn test_channel_message_storage() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    // Create a public channel
    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let sender = UserId::new();

    // Store a channel message
    let sent_at = next_timestamp_ns();
    let msg = Message::new_text(channel.id, ConversationType::Channel, Some(sender), "Hello channel!".to_string(), sent_at);
    message_repo.insert(&msg).unwrap();

    // Verify storage
    let loaded = message_repo.get(&msg.id).unwrap().unwrap();
    assert_eq!(loaded.conversation_id, channel.id);
    assert_eq!(loaded.sent_at, sent_at);
    match &loaded.content {
        MessageContent::Text(text) => assert_eq!(text, "Hello channel!"),
        _ => panic!("Expected text message"),
    }
}

#[test]
fn test_channel_message_deduplication() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let sender = UserId::new();

    // Create message with specific ID
    let msg_id = MessageId::new();
    let sent_at = next_timestamp_ns();
    let mut msg = Message::new_text(channel.id, ConversationType::Channel, Some(sender), "Test message".to_string(), sent_at);
    msg.id = msg_id;

    // First insert should succeed
    let first = message_repo.insert_or_ignore(&msg).unwrap();
    assert!(first);

    // Second insert with same ID should be ignored
    let second = message_repo.insert_or_ignore(&msg).unwrap();
    assert!(!second);

    // Only one message in database
    let messages = message_repo.list_for_channel(&channel.id, &ConversationType::Channel, None, 100).unwrap();
    assert_eq!(messages.len(), 1);
}

#[test]
fn test_private_channel_message_storage() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Private);
    let sender = UserId::new();

    let msg = Message::new_text(channel.id, ConversationType::Channel, Some(sender), "Private message".to_string(), next_timestamp_ns());
    message_repo.insert(&msg).unwrap();

    let loaded = message_repo.get(&msg.id).unwrap().unwrap();
    assert_eq!(loaded.conversation_id, channel.id);
}

// ============================================================================
// Saved Messages Storage Tests
// ============================================================================

#[test]
fn test_saved_messages_storage() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let user = UserId::new();

    // SavedMessages channel_id equals user_id
    let channel_id = ChannelId::from(user);
    let channel = Channel::new(
        channel_id,
        "Saved Messages".to_string(),
        ChannelType::SavedMessages,
        user,
    );
    channel_repo.insert(&channel).unwrap();

    // Store a saved message
    let msg = Message::new_text(channel_id, ConversationType::SavedMessages, Some(user), "Note to self".to_string(), next_timestamp_ns());
    message_repo.insert(&msg).unwrap();

    // Verify storage
    let loaded = message_repo.get(&msg.id).unwrap().unwrap();
    assert_eq!(loaded.conversation_id, channel_id);
    assert_eq!(loaded.sender_id, Some(user));
}

// ============================================================================
// System Message Storage Tests
// ============================================================================

#[test]
fn test_system_message_user_joined() {
    use chatter_core::domain::message::SystemMessage;

    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let joined_user = UserId::new();

    // Create system message for user join
    let now = chrono::Utc::now();
    let msg = Message {
        id: MessageId::new(),
        conversation_id: channel.id,
        conversation_type: ConversationType::Channel,
        sender_id: None, // System messages have no sender
        device_id: None,
        content: MessageContent::System(SystemMessage::UserJoined(joined_user)),
        encrypted_content: None,
        thread_id: None,
        reply_to: None,
        reply_to_author_id: None,
        reply_to_text: None,
        reply_to_thumbnail: None,
        reply_to_thumbnail_mime: None,
        attachments: Vec::new(),
        is_pinned: false,
        is_edited: false,
        is_deleted: false,
        is_read: false,
        sent_at: next_timestamp_ns(),
        created_at: now,
        updated_at: now,
        edited_at: None,
        reaction_counts: Vec::new(),
        current_user_reactions: Vec::new(),
        mentioned_integrations: Vec::new(),
        targets_ai_bot: false,
        delivery_status: String::new(),
    };

    message_repo.insert(&msg).unwrap();

    let loaded = message_repo.get(&msg.id).unwrap().unwrap();
    assert!(loaded.sender_id.is_none());
    match &loaded.content {
        MessageContent::System(SystemMessage::UserJoined(uid)) => {
            assert_eq!(*uid, joined_user);
        }
        _ => panic!("Expected UserJoined system message"),
    }
}

#[test]
fn test_system_message_user_left() {
    use chatter_core::domain::message::SystemMessage;

    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let left_user = UserId::new();

    let now = chrono::Utc::now();
    let msg = Message {
        id: MessageId::new(),
        conversation_id: channel.id,
        conversation_type: ConversationType::Channel,
        sender_id: None,
        device_id: None,
        content: MessageContent::System(SystemMessage::UserLeft(left_user)),
        encrypted_content: None,
        thread_id: None,
        reply_to: None,
        reply_to_author_id: None,
        reply_to_text: None,
        reply_to_thumbnail: None,
        reply_to_thumbnail_mime: None,
        attachments: Vec::new(),
        is_pinned: false,
        is_edited: false,
        is_deleted: false,
        is_read: false,
        sent_at: next_timestamp_ns(),
        created_at: now,
        updated_at: now,
        edited_at: None,
        reaction_counts: Vec::new(),
        current_user_reactions: Vec::new(),
        mentioned_integrations: Vec::new(),
        targets_ai_bot: false,
        delivery_status: String::new(),
    };

    message_repo.insert(&msg).unwrap();

    let loaded = message_repo.get(&msg.id).unwrap().unwrap();
    match &loaded.content {
        MessageContent::System(SystemMessage::UserLeft(uid)) => {
            assert_eq!(*uid, left_user);
        }
        _ => panic!("Expected UserLeft system message"),
    }
}

// ============================================================================
// Message Update Tests
// ============================================================================

#[test]
fn test_message_update() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let sender = UserId::new();

    // Insert original message
    let mut msg = Message::new_text(channel.id, ConversationType::Channel, Some(sender), "Original text".to_string(), next_timestamp_ns());
    message_repo.insert(&msg).unwrap();

    // Update the message
    msg.content = MessageContent::Text("Edited text".to_string());
    msg.is_edited = true;
    msg.edited_at = Some(chrono::Utc::now());
    message_repo.update(&msg).unwrap();

    // Verify update
    let loaded = message_repo.get(&msg.id).unwrap().unwrap();
    assert!(loaded.is_edited);
    match &loaded.content {
        MessageContent::Text(text) => assert_eq!(text, "Edited text"),
        _ => panic!("Expected text message"),
    }
}

#[test]
fn test_message_soft_delete() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let sender = UserId::new();

    let mut msg = Message::new_text(channel.id, ConversationType::Channel, Some(sender), "To be deleted".to_string(), next_timestamp_ns());
    message_repo.insert(&msg).unwrap();

    // Soft delete
    msg.is_deleted = true;
    message_repo.update(&msg).unwrap();

    let loaded = message_repo.get(&msg.id).unwrap().unwrap();
    assert!(loaded.is_deleted);
}

// ============================================================================
// Channel Unread Count Tests
// ============================================================================

#[test]
fn test_channel_unread_count_increment() {
    let (_temp_dir, db) = setup_test_db();
    let channel_repo = ChannelRepository::new(db.clone());
    let message_repo = MessageRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let sender = UserId::new();

    // Initial count should be 0
    let loaded = channel_repo.get(&channel.id).unwrap().unwrap();
    assert_eq!(loaded.unread_count, 0);

    // Unread count is computed from unread messages in the messages table
    let msg1 = Message::new_text(
        channel.id,
        ConversationType::Channel,
        Some(sender),
        "unread message 1".to_string(),
        next_timestamp_ns(),
    );
    message_repo.insert(&msg1).unwrap();

    let loaded = channel_repo.get(&channel.id).unwrap().unwrap();
    assert_eq!(loaded.unread_count, 1);

    // Add another unread message
    let msg2 = Message::new_text(
        channel.id,
        ConversationType::Channel,
        Some(sender),
        "unread message 2".to_string(),
        next_timestamp_ns(),
    );
    message_repo.insert(&msg2).unwrap();

    // Verify in database
    let loaded = channel_repo.get(&channel.id).unwrap().unwrap();
    assert_eq!(loaded.unread_count, 2);
}

#[test]
fn test_dm_channel_unread_count() {
    let (_temp_dir, db) = setup_test_db();
    let channel_repo = ChannelRepository::new(db.clone());
    let message_repo = MessageRepository::new(db.clone());

    let alice = UserId::new();
    let bob = UserId::new();

    let dm_channel = create_dm_channel(&channel_repo, &alice, &bob);

    // Add unread received messages
    let msg1 = Message::new_text(
        dm_channel.id,
        ConversationType::Channel,
        Some(bob),
        "hey alice".to_string(),
        next_timestamp_ns(),
    );
    message_repo.insert(&msg1).unwrap();

    let msg2 = Message::new_text(
        dm_channel.id,
        ConversationType::Channel,
        Some(bob),
        "are you there?".to_string(),
        next_timestamp_ns(),
    );
    message_repo.insert(&msg2).unwrap();

    let loaded = channel_repo.get(&dm_channel.id).unwrap().unwrap();
    assert_eq!(loaded.unread_count, 2);
}

// ============================================================================
// Message Ordering Tests
// ============================================================================

#[test]
fn test_messages_ordered_by_timestamp() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let sender = UserId::new();

    // Create messages with different sent_at timestamps
    let base_ts = 1_700_000_000_000_000_000u64;

    let mut msg1 = Message::new_text(channel.id, ConversationType::Channel, Some(sender), "First".to_string(), base_ts);
    let mut msg2 = Message::new_text(channel.id, ConversationType::Channel, Some(sender), "Second".to_string(), base_ts + 5_000_000_000);
    let mut msg3 = Message::new_text(channel.id, ConversationType::Channel, Some(sender), "Third".to_string(), base_ts + 10_000_000_000);

    // Also set created_at to ensure they differ
    let now = chrono::Utc::now();
    msg1.created_at = now - chrono::Duration::seconds(10);
    msg2.created_at = now - chrono::Duration::seconds(5);
    msg3.created_at = now;

    // Insert in random order
    message_repo.insert(&msg2).unwrap();
    message_repo.insert(&msg1).unwrap();
    message_repo.insert(&msg3).unwrap();

    // Retrieve and verify order
    let messages = message_repo.list_for_channel(&channel.id, &ConversationType::Channel, None, 100).unwrap();
    assert_eq!(messages.len(), 3);

    // Messages are returned in descending order (newest first) for pagination
    match (&messages[0].content, &messages[1].content, &messages[2].content) {
        (MessageContent::Text(t1), MessageContent::Text(t2), MessageContent::Text(t3)) => {
            assert_eq!(t1, "Third");  // Newest first
            assert_eq!(t2, "Second");
            assert_eq!(t3, "First");  // Oldest last
        }
        _ => panic!("Expected text messages"),
    }
}

// ============================================================================
// Stress Tests
// ============================================================================

#[test]
fn test_bulk_message_storage() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let sender = UserId::new();

    // Insert 100 messages
    for i in 0..100 {
        let msg = Message::new_text(channel.id, ConversationType::Channel, Some(sender), format!("Message {}", i), next_timestamp_ns());
        message_repo.insert(&msg).unwrap();
    }

    // Verify all stored
    let messages = message_repo.list_for_channel(&channel.id, &ConversationType::Channel, None, 200).unwrap();
    assert_eq!(messages.len(), 100);
}

#[test]
fn test_concurrent_dm_channels() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let alice = UserId::new();

    // Create 10 DM channels with different users
    let mut channels = Vec::new();
    for _ in 0..10 {
        let other_user = UserId::new();
        let channel = create_dm_channel(&channel_repo, &alice, &other_user);
        channels.push((channel, other_user));
    }

    // Store messages in each channel
    for (channel, peer) in &channels {
        let msg = Message::new_text(channel.id, ConversationType::Channel, Some(*peer), "Hello!".to_string(), next_timestamp_ns());
        message_repo.insert(&msg).unwrap();
    }

    // Verify each channel has exactly one message
    for (channel, _) in &channels {
        let messages = message_repo.list_for_channel(&channel.id, &ConversationType::Channel, None, 100).unwrap();
        assert_eq!(messages.len(), 1);
    }
}

// ============================================================================
// Signal-Style (sender_id, sent_at) Lookup Tests
// ============================================================================

#[test]
fn test_get_by_sender_timestamp_basic() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let sender = UserId::new();

    // Create message with known timestamp
    let sent_at = 1_700_000_001_234_567_890u64;
    let msg = Message::new_text(channel.id, ConversationType::Channel, Some(sender), "Signal-style test".to_string(), sent_at);
    message_repo.insert(&msg).unwrap();

    // Lookup by (sender_id, sent_at) - Signal-style
    let found = message_repo.get_by_sender_timestamp(&sender, sent_at).unwrap();
    assert!(found.is_some());
    let found_msg = found.unwrap();
    assert_eq!(found_msg.id, msg.id);
    assert_eq!(found_msg.sender_id, Some(sender));
    assert_eq!(found_msg.sent_at, sent_at);
    match &found_msg.content {
        MessageContent::Text(text) => assert_eq!(text, "Signal-style test"),
        _ => panic!("Expected text message"),
    }
}

#[test]
fn test_get_by_sender_timestamp_not_found() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let sender = UserId::new();

    // Create message
    let sent_at = 1_700_000_001_234_567_890u64;
    let msg = Message::new_text(channel.id, ConversationType::Channel, Some(sender), "Test message".to_string(), sent_at);
    message_repo.insert(&msg).unwrap();

    // Try to find with wrong timestamp
    let wrong_ts = 1_700_000_002_000_000_000u64;
    let not_found = message_repo.get_by_sender_timestamp(&sender, wrong_ts).unwrap();
    assert!(not_found.is_none(), "Should not find message with wrong timestamp");

    // Try to find with wrong sender
    let wrong_sender = UserId::new();
    let not_found = message_repo.get_by_sender_timestamp(&wrong_sender, sent_at).unwrap();
    assert!(not_found.is_none(), "Should not find message with wrong sender");
}

#[test]
fn test_get_by_sender_timestamp_multiple_senders() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let alice = UserId::new();
    let bob = UserId::new();

    // Both users send messages at different times
    let alice_ts = 1_700_000_001_000_000_000u64;
    let bob_ts = 1_700_000_002_000_000_000u64;

    let alice_msg = Message::new_text(channel.id, ConversationType::Channel, Some(alice), "Hello from Alice".to_string(), alice_ts);
    let bob_msg = Message::new_text(channel.id, ConversationType::Channel, Some(bob), "Hello from Bob".to_string(), bob_ts);

    message_repo.insert(&alice_msg).unwrap();
    message_repo.insert(&bob_msg).unwrap();

    // Each lookup should find the correct message
    let found_alice = message_repo.get_by_sender_timestamp(&alice, alice_ts).unwrap().unwrap();
    assert_eq!(found_alice.id, alice_msg.id);

    let found_bob = message_repo.get_by_sender_timestamp(&bob, bob_ts).unwrap().unwrap();
    assert_eq!(found_bob.id, bob_msg.id);

    // Cross lookups should fail
    assert!(message_repo.get_by_sender_timestamp(&alice, bob_ts).unwrap().is_none());
    assert!(message_repo.get_by_sender_timestamp(&bob, alice_ts).unwrap().is_none());
}

#[test]
fn test_get_by_sender_timestamp_same_sender_different_times() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let sender = UserId::new();

    // Same sender sends multiple messages at different times
    let ts1 = 1_700_000_001_000_000_000u64;
    let ts2 = 1_700_000_002_000_000_000u64;
    let ts3 = 1_700_000_003_000_000_000u64;

    let msg1 = Message::new_text(channel.id, ConversationType::Channel, Some(sender), "First".to_string(), ts1);
    let msg2 = Message::new_text(channel.id, ConversationType::Channel, Some(sender), "Second".to_string(), ts2);
    let msg3 = Message::new_text(channel.id, ConversationType::Channel, Some(sender), "Third".to_string(), ts3);

    message_repo.insert(&msg1).unwrap();
    message_repo.insert(&msg2).unwrap();
    message_repo.insert(&msg3).unwrap();

    // Each timestamp should find the correct message
    let found1 = message_repo.get_by_sender_timestamp(&sender, ts1).unwrap().unwrap();
    let found2 = message_repo.get_by_sender_timestamp(&sender, ts2).unwrap().unwrap();
    let found3 = message_repo.get_by_sender_timestamp(&sender, ts3).unwrap().unwrap();

    match &found1.content {
        MessageContent::Text(t) => assert_eq!(t, "First"),
        _ => panic!("Expected text"),
    }
    match &found2.content {
        MessageContent::Text(t) => assert_eq!(t, "Second"),
        _ => panic!("Expected text"),
    }
    match &found3.content {
        MessageContent::Text(t) => assert_eq!(t, "Third"),
        _ => panic!("Expected text"),
    }
}

// ============================================================================
// Signal-Style Deduplication Tests
// ============================================================================

#[test]
fn test_signal_style_deduplication_same_sender_timestamp() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let sender = UserId::new();

    // Create two messages with same (sender_id, sent_at) but different message_ids
    // This simulates the same message arriving twice with different server-assigned IDs
    let sent_at = 1_700_000_001_234_567_890u64;

    let mut msg1 = Message::new_text(channel.id, ConversationType::Channel, Some(sender), "Original message".to_string(), sent_at);
    let original_id = msg1.id;

    let mut msg2 = Message::new_text(channel.id, ConversationType::Channel, Some(sender), "Duplicate delivery".to_string(), sent_at);
    msg2.id = MessageId::new(); // Different message ID

    // First insert should succeed
    let first = message_repo.insert_or_ignore(&msg1).unwrap();
    assert!(first, "First message should be inserted");

    // Second insert with same (sender_id, sent_at) should be ignored
    let second = message_repo.insert_or_ignore(&msg2).unwrap();
    assert!(!second, "Duplicate (sender_id, sent_at) should be ignored");

    // Verify only one message exists
    let messages = message_repo.list_for_channel(&channel.id, &ConversationType::Channel, None, 100).unwrap();
    assert_eq!(messages.len(), 1);

    // And it should be the original one
    assert_eq!(messages[0].id, original_id);
    match &messages[0].content {
        MessageContent::Text(t) => assert_eq!(t, "Original message"),
        _ => panic!("Expected text"),
    }
}

#[test]
fn test_signal_style_allows_different_senders_same_timestamp() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let alice = UserId::new();
    let bob = UserId::new();

    // Two different senders can have the same timestamp
    let same_ts = 1_700_000_001_234_567_890u64;

    let alice_msg = Message::new_text(channel.id, ConversationType::Channel, Some(alice), "From Alice".to_string(), same_ts);
    let bob_msg = Message::new_text(channel.id, ConversationType::Channel, Some(bob), "From Bob".to_string(), same_ts);

    // Both should be inserted successfully
    let first = message_repo.insert_or_ignore(&alice_msg).unwrap();
    assert!(first, "Alice's message should be inserted");

    let second = message_repo.insert_or_ignore(&bob_msg).unwrap();
    assert!(second, "Bob's message should be inserted (different sender)");

    // Verify both messages exist
    let messages = message_repo.list_for_channel(&channel.id, &ConversationType::Channel, None, 100).unwrap();
    assert_eq!(messages.len(), 2);
}

// ============================================================================
// Message Action Tests (Edit, Delete, Pin using Signal-style lookup)
// ============================================================================

#[test]
fn test_edit_message_via_signal_style_lookup() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let sender = UserId::new();

    // Create original message
    let sent_at = 1_700_000_001_234_567_890u64;
    let msg = Message::new_text(channel.id, ConversationType::Channel, Some(sender), "Original text".to_string(), sent_at);
    message_repo.insert(&msg).unwrap();

    // Simulate edit action: lookup by (sender_id, sent_at) and update
    let mut found = message_repo.get_by_sender_timestamp(&sender, sent_at).unwrap().unwrap();
    found.content = MessageContent::Text("Edited text".to_string());
    found.is_edited = true;
    found.edited_at = Some(chrono::Utc::now());
    message_repo.update(&found).unwrap();

    // Verify edit persisted
    let reloaded = message_repo.get_by_sender_timestamp(&sender, sent_at).unwrap().unwrap();
    assert!(reloaded.is_edited);
    match &reloaded.content {
        MessageContent::Text(t) => assert_eq!(t, "Edited text"),
        _ => panic!("Expected text"),
    }
}

#[test]
fn test_delete_message_via_signal_style_lookup() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let sender = UserId::new();

    // Create message
    let sent_at = 1_700_000_001_234_567_890u64;
    let msg = Message::new_text(channel.id, ConversationType::Channel, Some(sender), "To be deleted".to_string(), sent_at);
    message_repo.insert(&msg).unwrap();

    // Simulate delete action: lookup by (sender_id, sent_at) and soft delete
    let mut found = message_repo.get_by_sender_timestamp(&sender, sent_at).unwrap().unwrap();
    found.is_deleted = true;
    message_repo.update(&found).unwrap();

    // Verify delete persisted
    let reloaded = message_repo.get_by_sender_timestamp(&sender, sent_at).unwrap().unwrap();
    assert!(reloaded.is_deleted);
}

#[test]
fn test_pin_message_via_signal_style_lookup() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let sender = UserId::new();

    // Create message
    let sent_at = 1_700_000_001_234_567_890u64;
    let msg = Message::new_text(channel.id, ConversationType::Channel, Some(sender), "Pin this message".to_string(), sent_at);
    message_repo.insert(&msg).unwrap();

    // Simulate pin action: lookup by (sender_id, sent_at) and pin
    let mut found = message_repo.get_by_sender_timestamp(&sender, sent_at).unwrap().unwrap();
    assert!(!found.is_pinned);
    found.is_pinned = true;
    message_repo.update(&found).unwrap();

    // Verify pin persisted
    let reloaded = message_repo.get_by_sender_timestamp(&sender, sent_at).unwrap().unwrap();
    assert!(reloaded.is_pinned);

    // Unpin
    let mut found = message_repo.get_by_sender_timestamp(&sender, sent_at).unwrap().unwrap();
    found.is_pinned = false;
    message_repo.update(&found).unwrap();

    let reloaded = message_repo.get_by_sender_timestamp(&sender, sent_at).unwrap().unwrap();
    assert!(!reloaded.is_pinned);
}

#[test]
fn test_reaction_via_signal_style_lookup() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let sender = UserId::new();

    // Create message
    let sent_at = 1_700_000_001_234_567_890u64;
    let msg = Message::new_text(channel.id, ConversationType::Channel, Some(sender), "React to this".to_string(), sent_at);
    message_repo.insert(&msg).unwrap();

    // Simulate reaction: lookup by (sender_id, sent_at) and add reaction
    let mut found = message_repo.get_by_sender_timestamp(&sender, sent_at).unwrap().unwrap();
    use chatter_core::domain::message::ReactionCount;
    found.reaction_counts.push(ReactionCount {
        emoji: "👍".to_string(),
        count: 1,
    });
    found.current_user_reactions.push("👍".to_string());
    message_repo.update(&found).unwrap();

    // Verify reaction persisted
    let reloaded = message_repo.get_by_sender_timestamp(&sender, sent_at).unwrap().unwrap();
    assert_eq!(reloaded.reaction_counts.len(), 1);
    assert_eq!(reloaded.reaction_counts[0].emoji, "👍");
    assert_eq!(reloaded.reaction_counts[0].count, 1);
    assert!(reloaded.current_user_reactions.contains(&"👍".to_string()));
}

// ============================================================================
// Cross-Channel Signal-Style Lookup Tests
// ============================================================================

#[test]
fn test_signal_style_lookup_across_channels() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel1 = create_test_channel(&channel_repo, ChannelType::Public);
    let channel2 = create_test_channel(&channel_repo, ChannelType::Public);
    let sender = UserId::new();

    // Same sender sends messages in different channels
    let ts1 = 1_700_000_001_000_000_000u64;
    let ts2 = 1_700_000_002_000_000_000u64;

    let msg1 = Message::new_text(channel1.id, ConversationType::Channel, Some(sender), "In channel 1".to_string(), ts1);
    let msg2 = Message::new_text(channel2.id, ConversationType::Channel, Some(sender), "In channel 2".to_string(), ts2);

    message_repo.insert(&msg1).unwrap();
    message_repo.insert(&msg2).unwrap();

    // Signal-style lookup should find messages regardless of channel
    let found1 = message_repo.get_by_sender_timestamp(&sender, ts1).unwrap().unwrap();
    let found2 = message_repo.get_by_sender_timestamp(&sender, ts2).unwrap().unwrap();

    assert_eq!(found1.conversation_id, channel1.id);
    assert_eq!(found2.conversation_id, channel2.id);
}

// ============================================================================
// Cross-Conversation Isolation Tests (GRA-441)
// ============================================================================

/// Verify that update_delivery_status_by_timestamp scopes to a single
/// conversation and does not affect messages with the same timestamp in
/// other conversations.
#[test]
fn test_delivery_status_update_cross_conversation_isolation() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let sender = UserId::new();

    // Create two different channels
    let channel1 = create_test_channel(&channel_repo, ChannelType::Public);
    let channel2 = create_test_channel(&channel_repo, ChannelType::Public);

    // Insert messages with the SAME timestamp in different conversations
    let same_timestamp = next_timestamp_ns();
    let msg1 = Message::new_text(channel1.id, ConversationType::Channel, Some(sender), "Msg in channel 1".to_string(), same_timestamp);
    let msg2 = Message::new_text(channel2.id, ConversationType::Channel, Some(sender), "Msg in channel 2".to_string(), same_timestamp);

    message_repo.insert(&msg1).unwrap();
    message_repo.insert(&msg2).unwrap();

    // Update delivery status for channel 1 only (scoped update)
    let updated = message_repo
        .update_delivery_status_by_timestamp(
            &channel1.id,
            &ConversationType::Channel,
            same_timestamp,
            "delivered",
        )
        .unwrap();
    assert!(updated, "Should have updated at least one row");

    // Verify: channel 1 message should be "delivered"
    let loaded1 = message_repo.get(&msg1.id).unwrap().unwrap();
    assert_eq!(
        loaded1.delivery_status, "delivered",
        "Channel 1 message should be 'delivered'"
    );

    // Verify: channel 2 message should still be empty (unchanged)
    let loaded2 = message_repo.get(&msg2.id).unwrap().unwrap();
    assert_eq!(
        loaded2.delivery_status, "",
        "Channel 2 message must NOT be affected by scoped update to channel 1"
    );
}

/// Verify that the lookup-based fallback updates messages by primary key
/// without leaking across conversations unexpectedly.
#[test]
fn test_delivery_status_update_with_lookup_updates_by_id() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let sender = UserId::new();
    let channel = create_test_channel(&channel_repo, ChannelType::Public);

    let ts = next_timestamp_ns();
    let msg = Message::new_text(channel.id, ConversationType::Channel, Some(sender), "Test".to_string(), ts);
    message_repo.insert(&msg).unwrap();

    // Use the lookup-based update
    let updated = message_repo
        .update_delivery_status_by_timestamp_with_lookup(ts, "read")
        .unwrap();
    assert!(updated);

    let loaded = message_repo.get(&msg.id).unwrap().unwrap();
    assert_eq!(loaded.delivery_status, "read");
}

// ============================================================================
// Cross-Conversation-Type Isolation Tests (GRA-442)
// ============================================================================

/// Test 1: Insert messages with all 6 ConversationType values using the same
/// conversation_id and verify list_for_conversation returns only matching type.
#[test]
fn test_type_isolation_on_list() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);

    let all_types = [
        ConversationType::Channel,
        ConversationType::ServerDm,
        ConversationType::MailboxDm,
        ConversationType::AnonDm,
        ConversationType::AnonGroup,
        ConversationType::SavedMessages,
    ];

    let sender = UserId::new();

    // Insert one message per conversation type, all sharing the same conversation_id.
    for ct in &all_types {
        let msg = Message::new_text(
            channel.id,
            ct.clone(),
            Some(sender),
            format!("msg-{:?}", ct),
            next_timestamp_ns(),
        );
        message_repo.insert(&msg).unwrap();
    }

    // Each type should return exactly 1 message, and only its own.
    for ct in &all_types {
        let messages = message_repo
            .list_for_conversation(&channel.id, ct, None, 100)
            .unwrap();
        assert_eq!(
            messages.len(),
            1,
            "Expected exactly 1 message for {:?}, got {}",
            ct,
            messages.len()
        );
        match &messages[0].content {
            MessageContent::Text(text) => {
                assert_eq!(text, &format!("msg-{:?}", ct));
            }
            _ => panic!("Expected text for {:?}", ct),
        }
    }
}

/// Test 2: Insert AnonDm and AnonGroup messages with same channel_id and
/// verify no cross-leak via list_for_conversation.
#[test]
fn test_anon_dm_vs_anon_group_isolation() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let sender = UserId::new();

    // Insert 3 AnonDm messages
    for i in 0..3 {
        let msg = Message::new_text(
            channel.id,
            ConversationType::AnonDm,
            Some(sender),
            format!("anon-dm-{}", i),
            next_timestamp_ns(),
        );
        message_repo.insert(&msg).unwrap();
    }

    // Insert 2 AnonGroup messages (same channel_id)
    for i in 0..2 {
        let msg = Message::new_text(
            channel.id,
            ConversationType::AnonGroup,
            Some(sender),
            format!("anon-group-{}", i),
            next_timestamp_ns(),
        );
        message_repo.insert(&msg).unwrap();
    }

    // AnonDm list should have exactly 3
    let dm_msgs = message_repo
        .list_for_conversation(&channel.id, &ConversationType::AnonDm, None, 100)
        .unwrap();
    assert_eq!(dm_msgs.len(), 3, "AnonDm should have 3 messages");
    for m in &dm_msgs {
        match &m.content {
            MessageContent::Text(t) => assert!(
                t.starts_with("anon-dm-"),
                "AnonDm list leaked an AnonGroup message: {}",
                t
            ),
            _ => panic!("Expected text"),
        }
    }

    // AnonGroup list should have exactly 2
    let group_msgs = message_repo
        .list_for_conversation(&channel.id, &ConversationType::AnonGroup, None, 100)
        .unwrap();
    assert_eq!(group_msgs.len(), 2, "AnonGroup should have 2 messages");
    for m in &group_msgs {
        match &m.content {
            MessageContent::Text(t) => assert!(
                t.starts_with("anon-group-"),
                "AnonGroup list leaked an AnonDm message: {}",
                t
            ),
            _ => panic!("Expected text"),
        }
    }
}

/// Test 3: Two messages with same channel+timestamp but different types.
/// update_text_by_timestamp with one type should only change that message.
#[test]
fn test_update_text_by_timestamp_type_isolation() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let sender = UserId::new();
    let shared_ts = next_timestamp_ns();

    // Insert a Channel message and a ServerDm message at the same timestamp+channel
    let msg_channel = Message::new_text(
        channel.id,
        ConversationType::Channel,
        Some(sender),
        "original-channel".to_string(),
        shared_ts,
    );
    let msg_server_dm = Message::new_text(
        channel.id,
        ConversationType::ServerDm,
        Some(sender),
        "original-server-dm".to_string(),
        shared_ts,
    );
    message_repo.insert(&msg_channel).unwrap();
    message_repo.insert(&msg_server_dm).unwrap();

    // Update only the Channel message's text
    let updated = message_repo
        .update_text_by_timestamp(
            &channel.id,
            &ConversationType::Channel,
            shared_ts,
            "edited-channel",
        )
        .unwrap();
    assert!(updated, "Should have updated the Channel message");

    // Verify: Channel message text changed
    let loaded_channel = message_repo.get(&msg_channel.id).unwrap().unwrap();
    match &loaded_channel.content {
        MessageContent::Text(t) => assert_eq!(t, "edited-channel"),
        _ => panic!("Expected text"),
    }

    // Verify: ServerDm message text is untouched
    let loaded_dm = message_repo.get(&msg_server_dm.id).unwrap().unwrap();
    match &loaded_dm.content {
        MessageContent::Text(t) => assert_eq!(
            t, "original-server-dm",
            "ServerDm message must NOT be affected by Channel update"
        ),
        _ => panic!("Expected text"),
    }
}

/// Test 4: Two messages with same sent_at in different conversation types.
/// update_delivery_status_by_timestamp for one type should leave the other unchanged.
#[test]
fn test_update_delivery_status_by_timestamp_type_collision() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let sender = UserId::new();
    let shared_ts = next_timestamp_ns();

    let msg_mailbox = Message::new_text(
        channel.id,
        ConversationType::MailboxDm,
        Some(sender),
        "mailbox msg".to_string(),
        shared_ts,
    );
    let msg_saved = Message::new_text(
        channel.id,
        ConversationType::SavedMessages,
        Some(sender),
        "saved msg".to_string(),
        shared_ts,
    );
    message_repo.insert(&msg_mailbox).unwrap();
    message_repo.insert(&msg_saved).unwrap();

    // Update delivery status for MailboxDm only
    let updated = message_repo
        .update_delivery_status_by_timestamp(
            &channel.id,
            &ConversationType::MailboxDm,
            shared_ts,
            "delivered",
        )
        .unwrap();
    assert!(updated, "Should update MailboxDm row");

    // MailboxDm should be "delivered"
    let loaded_mailbox = message_repo.get(&msg_mailbox.id).unwrap().unwrap();
    assert_eq!(loaded_mailbox.delivery_status, "delivered");

    // SavedMessages should still be empty
    let loaded_saved = message_repo.get(&msg_saved.id).unwrap().unwrap();
    assert_eq!(
        loaded_saved.delivery_status, "",
        "SavedMessages message must NOT be affected by MailboxDm delivery update"
    );
}

/// Test 5: Insert unread messages in different conversation types, verify
/// get_unread_count returns correct counts per type.
#[test]
fn test_get_unread_count_per_type_isolation() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let sender = UserId::new();

    // Insert 3 unread Channel messages
    for _ in 0..3 {
        let msg = Message::new_text(
            channel.id,
            ConversationType::Channel,
            Some(sender),
            "channel unread".to_string(),
            next_timestamp_ns(),
        );
        message_repo.insert(&msg).unwrap();
    }

    // Insert 5 unread MailboxDm messages (same channel_id)
    for _ in 0..5 {
        let msg = Message::new_text(
            channel.id,
            ConversationType::MailboxDm,
            Some(sender),
            "mailbox unread".to_string(),
            next_timestamp_ns(),
        );
        message_repo.insert(&msg).unwrap();
    }

    // Insert 1 unread AnonDm message
    let msg = Message::new_text(
        channel.id,
        ConversationType::AnonDm,
        Some(sender),
        "anon unread".to_string(),
        next_timestamp_ns(),
    );
    message_repo.insert(&msg).unwrap();

    // Verify per-type counts
    let channel_count = message_repo
        .get_unread_count(&channel.id, &ConversationType::Channel)
        .unwrap();
    assert_eq!(channel_count, 3, "Channel unread count should be 3");

    let mailbox_count = message_repo
        .get_unread_count(&channel.id, &ConversationType::MailboxDm)
        .unwrap();
    assert_eq!(mailbox_count, 5, "MailboxDm unread count should be 5");

    let anon_count = message_repo
        .get_unread_count(&channel.id, &ConversationType::AnonDm)
        .unwrap();
    assert_eq!(anon_count, 1, "AnonDm unread count should be 1");

    // Types with no messages should return 0
    let server_dm_count = message_repo
        .get_unread_count(&channel.id, &ConversationType::ServerDm)
        .unwrap();
    assert_eq!(server_dm_count, 0, "ServerDm unread count should be 0");
}

/// Test 6: Insert unread messages in two types, mark one type as read,
/// verify the other type's messages are still unread.
#[test]
fn test_mark_conversation_read_per_type_isolation() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let sender = UserId::new();

    // Insert 4 unread Channel messages
    for _ in 0..4 {
        let msg = Message::new_text(
            channel.id,
            ConversationType::Channel,
            Some(sender),
            "channel msg".to_string(),
            next_timestamp_ns(),
        );
        message_repo.insert(&msg).unwrap();
    }

    // Insert 3 unread AnonGroup messages (same channel_id)
    for _ in 0..3 {
        let msg = Message::new_text(
            channel.id,
            ConversationType::AnonGroup,
            Some(sender),
            "anon-group msg".to_string(),
            next_timestamp_ns(),
        );
        message_repo.insert(&msg).unwrap();
    }

    // Sanity: both should have unread counts
    assert_eq!(
        message_repo
            .get_unread_count(&channel.id, &ConversationType::Channel)
            .unwrap(),
        4
    );
    assert_eq!(
        message_repo
            .get_unread_count(&channel.id, &ConversationType::AnonGroup)
            .unwrap(),
        3
    );

    // Mark Channel as read
    message_repo
        .mark_conversation_read(&channel.id, &ConversationType::Channel)
        .unwrap();

    // Channel should now be 0
    assert_eq!(
        message_repo
            .get_unread_count(&channel.id, &ConversationType::Channel)
            .unwrap(),
        0,
        "Channel unread count should be 0 after mark_conversation_read"
    );

    // AnonGroup should still be 3 (untouched)
    assert_eq!(
        message_repo
            .get_unread_count(&channel.id, &ConversationType::AnonGroup)
            .unwrap(),
        3,
        "AnonGroup unread count must NOT be affected by marking Channel as read"
    );
}

/// Test 7: Add reactions to messages in different conversation types, verify
/// get_reaction_counts_for_conversation returns only matching type.
#[test]
fn test_reaction_counts_per_type_isolation() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let sender = UserId::new();
    let reactor = UserId::new();

    // Insert a Channel message and react to it
    let ts_channel = next_timestamp_ns();
    let msg_channel = Message::new_text(
        channel.id,
        ConversationType::Channel,
        Some(sender),
        "channel msg".to_string(),
        ts_channel,
    );
    message_repo.insert(&msg_channel).unwrap();
    message_repo
        .add_reaction(&sender, ts_channel, &reactor, &reactor, "fire")
        .unwrap();

    // Insert a MailboxDm message and react to it with a different emoji
    let ts_dm = next_timestamp_ns();
    let msg_dm = Message::new_text(
        channel.id,
        ConversationType::MailboxDm,
        Some(sender),
        "dm msg".to_string(),
        ts_dm,
    );
    message_repo.insert(&msg_dm).unwrap();
    message_repo
        .add_reaction(&sender, ts_dm, &reactor, &reactor, "heart")
        .unwrap();

    // get_reaction_counts_for_conversation for Channel should only have "fire"
    let channel_reactions = message_repo
        .get_reaction_counts_for_conversation(&channel.id, &ConversationType::Channel)
        .unwrap();
    // Map is keyed by sent_at
    assert!(
        channel_reactions.contains_key(&ts_channel),
        "Channel reactions should contain ts_channel key"
    );
    assert!(
        !channel_reactions.contains_key(&ts_dm),
        "Channel reactions must NOT contain MailboxDm timestamp key"
    );
    let channel_emojis: Vec<&String> = channel_reactions[&ts_channel]
        .iter()
        .map(|(emoji, _count)| emoji)
        .collect();
    assert!(channel_emojis.contains(&&"fire".to_string()));
    assert!(!channel_emojis.contains(&&"heart".to_string()));

    // get_reaction_counts_for_conversation for MailboxDm should only have "heart"
    let dm_reactions = message_repo
        .get_reaction_counts_for_conversation(&channel.id, &ConversationType::MailboxDm)
        .unwrap();
    assert!(
        dm_reactions.contains_key(&ts_dm),
        "MailboxDm reactions should contain ts_dm key"
    );
    assert!(
        !dm_reactions.contains_key(&ts_channel),
        "MailboxDm reactions must NOT contain Channel timestamp key"
    );
    let dm_emojis: Vec<&String> = dm_reactions[&ts_dm]
        .iter()
        .map(|(emoji, _count)| emoji)
        .collect();
    assert!(dm_emojis.contains(&&"heart".to_string()));
    assert!(!dm_emojis.contains(&&"fire".to_string()));
}

/// Test 8: Insert messages across types, verify list_for_channel (which takes
/// conversation_type) returns type-filtered results with correct pagination.
#[test]
fn test_pagination_isolation_across_types() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let sender = UserId::new();

    // Insert 5 Channel messages
    let mut channel_msg_ids = Vec::new();
    for i in 0..5 {
        let msg = Message::new_text(
            channel.id,
            ConversationType::Channel,
            Some(sender),
            format!("channel-{}", i),
            next_timestamp_ns(),
        );
        message_repo.insert(&msg).unwrap();
        channel_msg_ids.push(msg.id);
    }

    // Insert 3 ServerDm messages (same channel_id)
    let mut server_dm_msg_ids = Vec::new();
    for i in 0..3 {
        let msg = Message::new_text(
            channel.id,
            ConversationType::ServerDm,
            Some(sender),
            format!("server-dm-{}", i),
            next_timestamp_ns(),
        );
        message_repo.insert(&msg).unwrap();
        server_dm_msg_ids.push(msg.id);
    }

    // Page 1: get first 2 Channel messages (newest first)
    let page1 = message_repo
        .list_for_channel(&channel.id, &ConversationType::Channel, None, 2)
        .unwrap();
    assert_eq!(page1.len(), 2, "Page 1 should have 2 Channel messages");
    for m in &page1 {
        assert_eq!(m.conversation_type, ConversationType::Channel);
    }

    // Page 2: get next 2 Channel messages using cursor
    let cursor = &page1.last().unwrap().id;
    let page2 = message_repo
        .list_for_channel(&channel.id, &ConversationType::Channel, Some(cursor), 2)
        .unwrap();
    assert_eq!(page2.len(), 2, "Page 2 should have 2 Channel messages");
    for m in &page2 {
        assert_eq!(m.conversation_type, ConversationType::Channel);
    }

    // Page 3: get the remaining 1 Channel message
    let cursor2 = &page2.last().unwrap().id;
    let page3 = message_repo
        .list_for_channel(&channel.id, &ConversationType::Channel, Some(cursor2), 2)
        .unwrap();
    assert_eq!(page3.len(), 1, "Page 3 should have 1 remaining Channel message");
    assert_eq!(page3[0].conversation_type, ConversationType::Channel);

    // Total unique IDs across all pages should be 5 (no ServerDm leaks)
    let all_ids: std::collections::HashSet<_> = page1.iter().chain(page2.iter()).chain(page3.iter()).map(|m| m.id).collect();
    assert_eq!(all_ids.len(), 5, "Should have exactly 5 unique Channel messages across all pages");

    // ServerDm pagination should be independent and return 3 total
    let dm_all = message_repo
        .list_for_channel(&channel.id, &ConversationType::ServerDm, None, 100)
        .unwrap();
    assert_eq!(dm_all.len(), 3, "ServerDm should have exactly 3 messages");
    for m in &dm_all {
        assert_eq!(m.conversation_type, ConversationType::ServerDm);
    }
}

/// Thread replies live in the thread panel, not the main conversation.
///
/// Guards the two halves of the duplicate-render bug: `list_for_channel` must
/// hide replies, and `reply_counts_for_channel` must report them so the root
/// bubble can render its "N replies" indicator.
#[test]
fn test_thread_replies_excluded_from_channel_list() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let sender = UserId::new();

    let root = Message::new_text(
        channel.id, ConversationType::Channel, Some(sender),
        "thread root".to_string(), next_timestamp_ns(),
    );
    message_repo.insert(&root).unwrap();

    for i in 0..3 {
        let mut reply = Message::new_text(
            channel.id, ConversationType::Channel, Some(sender),
            format!("reply {i}"), next_timestamp_ns(),
        );
        reply.thread_id = Some(root.id);
        message_repo.insert(&reply).unwrap();
    }

    // A plain (non-thread) message stays visible in the main list.
    let plain = Message::new_text(
        channel.id, ConversationType::Channel, Some(sender),
        "main channel message".to_string(), next_timestamp_ns(),
    );
    message_repo.insert(&plain).unwrap();

    let main_list = message_repo
        .list_for_channel(&channel.id, &ConversationType::Channel, None, 100)
        .unwrap();
    let main_ids: Vec<_> = main_list.iter().map(|m| m.id).collect();
    assert_eq!(main_list.len(), 2, "main list = root + plain message, no replies");
    assert!(main_ids.contains(&root.id));
    assert!(main_ids.contains(&plain.id));
    assert!(main_list.iter().all(|m| m.thread_id.is_none()));

    // Paginated path must filter too.
    let page = message_repo
        .list_for_channel(&channel.id, &ConversationType::Channel, Some(&plain.id), 100)
        .unwrap();
    assert!(page.iter().all(|m| m.thread_id.is_none()), "cursor page leaked thread replies");

    // The replies are still reachable through the thread query...
    let replies = message_repo.list_for_thread(&root.id, None, 100).unwrap();
    assert_eq!(replies.len(), 3);

    // ...and counted for the root bubble's indicator.
    let counts = message_repo
        .reply_counts_for_channel(&channel.id, &ConversationType::Channel)
        .unwrap();
    assert_eq!(counts.get(&root.id), Some(&3));
    assert_eq!(counts.get(&plain.id), None);
}
