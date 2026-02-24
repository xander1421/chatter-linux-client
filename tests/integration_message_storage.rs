//! Integration tests for message storage.
//!
//! Verifies that all message flows properly persist to the database:
//! - DM messages (sent and received)
//! - Channel messages (sent and received)
//! - System messages (member join/leave)
//! - Saved messages (Note to Self)

mod common;

use std::sync::Arc;
use tempfile::TempDir;

use chatter_linux::domain::channel::{Channel, ChannelType};
use chatter_linux::domain::ids::{ChannelId, MessageId, UserId};
use chatter_linux::domain::message::{Message, MessageContent};
use chatter_linux::storage::{ChannelRepository, Database, MessageRepository};

// ============================================================================
// Test Setup Helpers
// ============================================================================

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
    let sent_msg = Message::new_text(dm_channel.id, Some(alice), "Hello Bob!".to_string());
    message_repo.insert(&sent_msg).unwrap();

    // Verify message is stored
    let loaded = message_repo.get(&sent_msg.id).unwrap();
    assert!(loaded.is_some());
    let loaded_msg = loaded.unwrap();
    assert_eq!(loaded_msg.channel_id, dm_channel.id);
    assert_eq!(loaded_msg.sender_id, Some(alice));
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
    let received_msg = Message::new_text(dm_channel.id, Some(bob), "Hi Alice!".to_string());

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
    let msg1 = Message::new_text(dm_ab.id, Some(alice), "Hi Bob".to_string());
    let msg2 = Message::new_text(dm_ab.id, Some(bob), "Hi Alice".to_string());
    let msg3 = Message::new_text(dm_ac.id, Some(alice), "Hi Charlie".to_string());

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
    let msg = Message::new_text(channel.id, Some(sender), "Hello channel!".to_string());
    message_repo.insert(&msg).unwrap();

    // Verify storage
    let loaded = message_repo.get(&msg.id).unwrap().unwrap();
    assert_eq!(loaded.channel_id, channel.id);
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
    let mut msg = Message::new_text(channel.id, Some(sender), "Test message".to_string());
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

    let msg = Message::new_text(channel.id, Some(sender), "Private message".to_string());
    message_repo.insert(&msg).unwrap();

    let loaded = message_repo.get(&msg.id).unwrap().unwrap();
    assert_eq!(loaded.channel_id, channel.id);
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
    let msg = Message::new_text(channel_id, Some(user), "Note to self".to_string());
    message_repo.insert(&msg).unwrap();

    // Verify storage
    let loaded = message_repo.get(&msg.id).unwrap().unwrap();
    assert_eq!(loaded.channel_id, channel_id);
    assert_eq!(loaded.sender_id, Some(user));
}

// ============================================================================
// System Message Storage Tests
// ============================================================================

#[test]
fn test_system_message_user_joined() {
    use chatter_linux::domain::message::SystemMessage;

    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let joined_user = UserId::new();

    // Create system message for user join
    let msg = Message {
        id: MessageId::new(),
        channel_id: channel.id,
        sender_id: None, // System messages have no sender
        content: MessageContent::System(SystemMessage::UserJoined(joined_user)),
        thread_id: None,
        reply_to: None,
        attachments: Vec::new(),
        is_pinned: false,
        is_edited: false,
        is_deleted: false,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        edited_at: None,
        reaction_counts: Vec::new(),
        current_user_reactions: Vec::new(),
        mentioned_integrations: Vec::new(),
        targets_ai_bot: false,
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
    use chatter_linux::domain::message::SystemMessage;

    let (_temp_dir, db) = setup_test_db();
    let message_repo = MessageRepository::new(db.clone());
    let channel_repo = ChannelRepository::new(db.clone());

    let channel = create_test_channel(&channel_repo, ChannelType::Public);
    let left_user = UserId::new();

    let msg = Message {
        id: MessageId::new(),
        channel_id: channel.id,
        sender_id: None,
        content: MessageContent::System(SystemMessage::UserLeft(left_user)),
        thread_id: None,
        reply_to: None,
        attachments: Vec::new(),
        is_pinned: false,
        is_edited: false,
        is_deleted: false,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        edited_at: None,
        reaction_counts: Vec::new(),
        current_user_reactions: Vec::new(),
        mentioned_integrations: Vec::new(),
        targets_ai_bot: false,
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
    let mut msg = Message::new_text(channel.id, Some(sender), "Original text".to_string());
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

    let mut msg = Message::new_text(channel.id, Some(sender), "To be deleted".to_string());
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

    // Create messages with different timestamps
    let now = chrono::Utc::now();

    let mut msg1 = Message::new_text(channel.id, Some(sender), "First".to_string());
    msg1.created_at = now - chrono::Duration::seconds(10);

    let mut msg2 = Message::new_text(channel.id, Some(sender), "Second".to_string());
    msg2.created_at = now - chrono::Duration::seconds(5);

    let mut msg3 = Message::new_text(channel.id, Some(sender), "Third".to_string());
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
        let msg = Message::new_text(channel.id, Some(sender), format!("Message {}", i));
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
        let msg = Message::new_text(channel.id, Some(*peer), "Hello!".to_string());
        message_repo.insert(&msg).unwrap();
    }

    // Verify each channel has exactly one message
    for (channel, _) in &channels {
        let messages = message_repo.list_for_channel(&channel.id, None, 100).unwrap();
        assert_eq!(messages.len(), 1);
    }
}
