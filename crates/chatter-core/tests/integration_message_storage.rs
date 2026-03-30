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
    let sent_msg = Message::new_text(dm_channel.id, Some(alice), "Hello Bob!".to_string(), sent_at);
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
    let received_msg = Message::new_text(dm_channel.id, Some(bob), "Hi Alice!".to_string(), sent_at);

    // Use insert_or_ignore like the polling code does
    let inserted = message_repo.insert_or_ignore(&received_msg).unwrap();
    assert!(inserted, "Message should be inserted");

    // Try inserting duplicate - should be ignored
    let duplicate_inserted = message_repo.insert_or_ignore(&received_msg).unwrap();
    assert!(!duplicate_inserted, "Duplicate message should be ignored");

    // Verify only one message exists
    let messages = message_repo.list_for_channel(&dm_channel.id, None, 100).unwrap();
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
    let msg1 = Message::new_text(dm_ab.id, Some(alice), "Hi Bob".to_string(), next_timestamp_ns());
    let msg2 = Message::new_text(dm_ab.id, Some(bob), "Hi Alice".to_string(), next_timestamp_ns());
    let msg3 = Message::new_text(dm_ac.id, Some(alice), "Hi Charlie".to_string(), next_timestamp_ns());

    message_repo.insert(&msg1).unwrap();
    message_repo.insert(&msg2).unwrap();
    message_repo.insert(&msg3).unwrap();

    // List messages for Alice-Bob channel
    let ab_messages = message_repo.list_for_channel(&dm_ab.id, None, 100).unwrap();
    assert_eq!(ab_messages.len(), 2);

    // List messages for Alice-Charlie channel
    let ac_messages = message_repo.list_for_channel(&dm_ac.id, None, 100).unwrap();
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
    let msg = Message::new_text(channel.id, Some(sender), "Hello channel!".to_string(), sent_at);
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
    let mut msg = Message::new_text(channel.id, Some(sender), "Test message".to_string(), sent_at);
    msg.id = msg_id;

    // First insert should succeed
    let first = message_repo.insert_or_ignore(&msg).unwrap();
    assert!(first);

    // Second insert with same ID should be ignored
    let second = message_repo.insert_or_ignore(&msg).unwrap();
    assert!(!second);

    // Only one message in database
    let messages = message_repo.list_for_channel(&channel.id, None, 100).unwrap();
    assert_eq!(messages.len(), 1);
}

#[test]
fn test_private_channel_message_storage() {
    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Private);
    let sender = UserId::new();

    let msg = Message::new_text(channel.id, Some(sender), "Private message".to_string(), next_timestamp_ns());
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
    let msg = Message::new_text(channel_id, Some(user), "Note to self".to_string(), next_timestamp_ns());
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
    let mut msg = Message::new_text(channel.id, Some(sender), "Original text".to_string(), next_timestamp_ns());
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

    let mut msg = Message::new_text(channel.id, Some(sender), "To be deleted".to_string(), next_timestamp_ns());
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

    let channel = create_test_channel(&channel_repo, ChannelType::Public);

    // Initial count should be 0
    let loaded = channel_repo.get(&channel.id).unwrap().unwrap();
    assert_eq!(loaded.unread_count, 0);

    // Increment
    let new_count = channel_repo.increment_unread_count(&channel.id).unwrap();
    assert_eq!(new_count, 1);

    // Increment again
    let new_count = channel_repo.increment_unread_count(&channel.id).unwrap();
    assert_eq!(new_count, 2);

    // Verify in database
    let loaded = channel_repo.get(&channel.id).unwrap().unwrap();
    assert_eq!(loaded.unread_count, 2);
}

#[test]
fn test_dm_channel_unread_count() {
    let (_temp_dir, db) = setup_test_db();
    let channel_repo = ChannelRepository::new(db.clone());

    let alice = UserId::new();
    let bob = UserId::new();

    let dm_channel = create_dm_channel(&channel_repo, &alice, &bob);

    // Increment for received messages
    channel_repo.increment_unread_count(&dm_channel.id).unwrap();
    channel_repo.increment_unread_count(&dm_channel.id).unwrap();

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

    let mut msg1 = Message::new_text(channel.id, Some(sender), "First".to_string(), base_ts);
    let mut msg2 = Message::new_text(channel.id, Some(sender), "Second".to_string(), base_ts + 5_000_000_000);
    let mut msg3 = Message::new_text(channel.id, Some(sender), "Third".to_string(), base_ts + 10_000_000_000);

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
    let messages = message_repo.list_for_channel(&channel.id, None, 100).unwrap();
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
        let msg = Message::new_text(channel.id, Some(sender), format!("Message {}", i), next_timestamp_ns());
        message_repo.insert(&msg).unwrap();
    }

    // Verify all stored
    let messages = message_repo.list_for_channel(&channel.id, None, 200).unwrap();
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
        let msg = Message::new_text(channel.id, Some(*peer), "Hello!".to_string(), next_timestamp_ns());
        message_repo.insert(&msg).unwrap();
    }

    // Verify each channel has exactly one message
    for (channel, _) in &channels {
        let messages = message_repo.list_for_channel(&channel.id, None, 100).unwrap();
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
    let msg = Message::new_text(channel.id, Some(sender), "Signal-style test".to_string(), sent_at);
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
    let msg = Message::new_text(channel.id, Some(sender), "Test message".to_string(), sent_at);
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

    let alice_msg = Message::new_text(channel.id, Some(alice), "Hello from Alice".to_string(), alice_ts);
    let bob_msg = Message::new_text(channel.id, Some(bob), "Hello from Bob".to_string(), bob_ts);

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

    let msg1 = Message::new_text(channel.id, Some(sender), "First".to_string(), ts1);
    let msg2 = Message::new_text(channel.id, Some(sender), "Second".to_string(), ts2);
    let msg3 = Message::new_text(channel.id, Some(sender), "Third".to_string(), ts3);

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

    let mut msg1 = Message::new_text(channel.id, Some(sender), "Original message".to_string(), sent_at);
    let original_id = msg1.id;

    let mut msg2 = Message::new_text(channel.id, Some(sender), "Duplicate delivery".to_string(), sent_at);
    msg2.id = MessageId::new(); // Different message ID

    // First insert should succeed
    let first = message_repo.insert_or_ignore(&msg1).unwrap();
    assert!(first, "First message should be inserted");

    // Second insert with same (sender_id, sent_at) should be ignored
    let second = message_repo.insert_or_ignore(&msg2).unwrap();
    assert!(!second, "Duplicate (sender_id, sent_at) should be ignored");

    // Verify only one message exists
    let messages = message_repo.list_for_channel(&channel.id, None, 100).unwrap();
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

    let alice_msg = Message::new_text(channel.id, Some(alice), "From Alice".to_string(), same_ts);
    let bob_msg = Message::new_text(channel.id, Some(bob), "From Bob".to_string(), same_ts);

    // Both should be inserted successfully
    let first = message_repo.insert_or_ignore(&alice_msg).unwrap();
    assert!(first, "Alice's message should be inserted");

    let second = message_repo.insert_or_ignore(&bob_msg).unwrap();
    assert!(second, "Bob's message should be inserted (different sender)");

    // Verify both messages exist
    let messages = message_repo.list_for_channel(&channel.id, None, 100).unwrap();
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
    let msg = Message::new_text(channel.id, Some(sender), "Original text".to_string(), sent_at);
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
    let msg = Message::new_text(channel.id, Some(sender), "To be deleted".to_string(), sent_at);
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
    let msg = Message::new_text(channel.id, Some(sender), "Pin this message".to_string(), sent_at);
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
    let msg = Message::new_text(channel.id, Some(sender), "React to this".to_string(), sent_at);
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

    let msg1 = Message::new_text(channel1.id, Some(sender), "In channel 1".to_string(), ts1);
    let msg2 = Message::new_text(channel2.id, Some(sender), "In channel 2".to_string(), ts2);

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
    let msg1 = Message::new_text(channel1.id, Some(sender), "Msg in channel 1".to_string(), same_timestamp);
    let msg2 = Message::new_text(channel2.id, Some(sender), "Msg in channel 2".to_string(), same_timestamp);

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
    let msg = Message::new_text(channel.id, Some(sender), "Test".to_string(), ts);
    message_repo.insert(&msg).unwrap();

    // Use the lookup-based update
    let updated = message_repo
        .update_delivery_status_by_timestamp_with_lookup(ts, "read")
        .unwrap();
    assert!(updated);

    let loaded = message_repo.get(&msg.id).unwrap().unwrap();
    assert_eq!(loaded.delivery_status, "read");
}
