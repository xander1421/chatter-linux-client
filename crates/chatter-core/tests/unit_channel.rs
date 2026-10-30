//! Unit tests for channel operations.
//!
//! Tests channel creation, membership management, settings,
//! and channel types using the testing framework.

use crate::framework;

use framework::prelude::*;

// ============================================================================
// Channel Creation Tests
// ============================================================================

#[tokio::test]
async fn test_public_channel_creation() {
    let channel = ChannelBuilder::public("general").build();

    assert_eq!(channel.name, "general");
    assert_eq!(channel.channel_type, 1, "Public channel type should be 1");
    assert!(!channel.id.is_empty());
}

#[tokio::test]
async fn test_private_channel_creation() {
    let channel = ChannelBuilder::private("secret-team").build();

    assert_eq!(channel.name, "secret-team");
    assert_eq!(channel.channel_type, 2, "Private channel type should be 2");
}

#[tokio::test]
async fn test_dm_channel_creation() {
    let channel = ChannelBuilder::dm()
        .with_member("alice")
        .with_member("bob")
        .build();

    assert_eq!(channel.channel_type, 3, "DM channel type should be 3");
    assert_eq!(channel.member_ids.len(), 2);
}

#[tokio::test]
async fn test_group_dm_channel_creation() {
    let channel = ChannelBuilder::group_dm("Project Team")
        .with_member("alice")
        .with_member("bob")
        .with_member("charlie")
        .build();

    assert_eq!(channel.name, "Project Team");
    assert_eq!(channel.channel_type, 4, "Group DM type should be 4");
    assert_eq!(channel.member_ids.len(), 3);
}

#[tokio::test]
async fn test_channel_with_description() {
    let channel = ChannelBuilder::public("announcements")
        .with_description("Important company announcements")
        .build();

    assert_eq!(
        channel.description,
        Some("Important company announcements".to_string())
    );
}

#[tokio::test]
async fn test_channel_creator_is_member() {
    let channel = ChannelBuilder::public("my-channel")
        .created_by("alice")
        .build();

    assert_eq!(channel.creator_id, "alice");
    assert!(
        channel.member_ids.contains(&"alice".to_string()),
        "Creator should be a member"
    );
}

// ============================================================================
// Channel Membership Tests
// ============================================================================

#[tokio::test]
async fn test_add_members_to_channel() {
    let channel = ChannelBuilder::public("team-channel")
        .created_by("alice")
        .with_member("bob")
        .with_member("charlie")
        .build();

    assert_eq!(channel.member_ids.len(), 3);
    assert!(channel.member_ids.contains(&"alice".to_string()));
    assert!(channel.member_ids.contains(&"bob".to_string()));
    assert!(channel.member_ids.contains(&"charlie".to_string()));
}

#[tokio::test]
async fn test_add_members_bulk() {
    let channel = ChannelBuilder::public("team")
        .with_members(vec!["alice", "bob", "charlie", "dave"])
        .build();

    assert_eq!(channel.member_ids.len(), 4);
}

#[tokio::test]
async fn test_no_duplicate_members() {
    let channel = ChannelBuilder::public("team")
        .with_member("alice")
        .with_member("alice") // Duplicate
        .with_member("bob")
        .build();

    // Should only have 2 unique members
    assert_eq!(channel.member_ids.len(), 2);
}

// ============================================================================
// Channel Storage Tests
// ============================================================================

#[tokio::test]
async fn test_channel_save_and_retrieve() {
    use framework::mocks::InMemoryStorage;

    let storage = InMemoryStorage::new();

    let channel = ChannelBuilder::public("test-channel").build();
    let channel_id = channel.id.clone();

    storage.save_channel(channel);

    let retrieved = storage.get_channel(&channel_id);
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().name, "test-channel");
}

#[tokio::test]
async fn test_list_all_channels() {
    use framework::mocks::InMemoryStorage;

    let storage = InMemoryStorage::new();

    storage.save_channel(ChannelBuilder::public("channel-1").build());
    storage.save_channel(ChannelBuilder::public("channel-2").build());
    storage.save_channel(ChannelBuilder::private("channel-3").build());

    let all_channels = storage.get_all_channels();
    assert_eq!(all_channels.len(), 3);
}

#[tokio::test]
async fn test_delete_channel() {
    use framework::mocks::InMemoryStorage;

    let storage = InMemoryStorage::new();

    let channel = ChannelBuilder::public("to-delete").build();
    let channel_id = channel.id.clone();

    storage.save_channel(channel);
    assert!(storage.get_channel(&channel_id).is_some());

    let deleted = storage.delete_channel(&channel_id);
    assert!(deleted);
    assert!(storage.get_channel(&channel_id).is_none());
}

// ============================================================================
// Mock Server Channel Tests
// ============================================================================

#[tokio::test]
async fn test_mock_server_channel_operations() {
    use framework::mocks::MockGrpcServer;

    let server = MockGrpcServer::new();

    let channel = ChannelBuilder::public("server-channel")
        .with_description("Test channel")
        .build();

    let channel_id = server.create_channel(channel);
    assert!(!channel_id.is_empty());

    let retrieved = server.get_channel(&channel_id);
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().name, "server-channel");
}

#[tokio::test]
async fn test_mock_server_multiple_channels() {
    use framework::mocks::MockGrpcServer;

    let server = MockGrpcServer::new();

    let ch1 = ChannelBuilder::public("channel-1").build();
    let ch2 = ChannelBuilder::public("channel-2").build();
    let ch3 = ChannelBuilder::private("channel-3").build();

    let id1 = server.create_channel(ch1);
    let id2 = server.create_channel(ch2);
    let id3 = server.create_channel(ch3);

    assert!(server.get_channel(&id1).is_some());
    assert!(server.get_channel(&id2).is_some());
    assert!(server.get_channel(&id3).is_some());
}

// ============================================================================
// Channel with Messages Tests
// ============================================================================

#[tokio::test]
async fn test_channel_messages() {
    let harness = TestHarness::unit().await;

    let channel = ChannelBuilder::public("chat-room").build();
    let channel_id = harness.mock_server().create_channel(channel);

    // Add some messages
    for i in 0..5 {
        let msg = MessageBuilder::text(&format!("Message {}", i))
            .in_channel(&channel_id)
            .build();
        harness.mock_server().store_message(msg);
    }

    let messages = harness.mock_server().get_messages(&channel_id, 10);
    assert_eq!(messages.len(), 5);
}

#[tokio::test]
async fn test_channel_message_pagination() {
    let harness = TestHarness::unit().await;

    let channel = ChannelBuilder::public("busy-channel").build();
    let channel_id = harness.mock_server().create_channel(channel);

    // Add 20 messages
    for i in 0..20 {
        let msg = MessageBuilder::text(&format!("Message {}", i))
            .in_channel(&channel_id)
            .build();
        harness.mock_server().store_message(msg);
    }

    // Get only 5
    let page1 = harness.mock_server().get_messages(&channel_id, 5);
    assert_eq!(page1.len(), 5);

    // Get all 20
    let all = harness.mock_server().get_messages(&channel_id, 50);
    assert_eq!(all.len(), 20);
}

// ============================================================================
// Channel Type Specific Tests
// ============================================================================

#[tokio::test]
async fn test_public_channel_properties() {
    let channel = ChannelBuilder::public("public-room").build();

    assert_eq!(channel.channel_type, 1);
    // Public channels can be joined by anyone (in real implementation)
}

#[tokio::test]
async fn test_private_channel_properties() {
    let channel = ChannelBuilder::private("private-room").build();

    assert_eq!(channel.channel_type, 2);
    // Private channels require invitation (in real implementation)
}

// ============================================================================
// Channel Harness Integration Tests
// ============================================================================

#[tokio::test]
async fn test_harness_channel_flow() {
    let harness = TestHarness::unit().await;
    let (alice, bob) = harness.conversation_pair().await;

    // Alice creates a channel
    let channel = ChannelBuilder::private("team-chat")
        .created_by(alice.user_id())
        .with_member(bob.user_id())
        .with_description("Team discussion")
        .build();

    let channel_id = harness.mock_server().create_channel(channel);

    // Verify channel exists
    let retrieved = harness.mock_server().get_channel(&channel_id);
    assert!(retrieved.is_some());

    let ch = retrieved.unwrap();
    assert_eq!(ch.name, "team-chat");
    assert!(ch.member_ids.contains(&alice.user_id().to_string()));
    assert!(ch.member_ids.contains(&bob.user_id().to_string()));
}

#[tokio::test]
async fn test_harness_reset_clears_channels() {
    let harness = TestHarness::unit().await;

    let channel = ChannelBuilder::public("test").build();
    let channel_id = harness.mock_server().create_channel(channel);

    assert!(harness.mock_server().get_channel(&channel_id).is_some());

    harness.reset().await;

    assert!(harness.mock_server().get_channel(&channel_id).is_none());
}

// ============================================================================
// Channel Name Tests
// ============================================================================

#[tokio::test]
async fn test_unique_channel_name_generator() {
    let name1 = unique_channel_name();
    let name2 = unique_channel_name();

    assert!(name1.starts_with("test-channel-"));
    assert!(name2.starts_with("test-channel-"));
    assert_ne!(name1, name2, "Generated names should be unique");
}

#[tokio::test]
async fn test_channel_timestamp() {
    let channel = ChannelBuilder::public("timestamped").build();

    assert!(
        channel.created_at > 0,
        "Channel should have creation timestamp"
    );
}

// ============================================================================
// Channel with Users Context Tests
// ============================================================================

#[tokio::test]
async fn test_user_context_storage_access() {
    let harness = TestHarness::unit().await;
    let (alice, _bob) = harness.conversation_pair().await;

    // User context should have access to storage
    let storage = alice.storage();
    assert_eq!(storage.channel_count(), 0); // Empty initially

    // Add a channel
    let channel = ChannelBuilder::public("alice-channel").build();
    storage.save_channel(channel);

    assert_eq!(storage.channel_count(), 1);
}

// ============================================================================
// Error Cases
// ============================================================================

#[tokio::test]
async fn test_nonexistent_channel() {
    use framework::mocks::MockGrpcServer;

    let server = MockGrpcServer::new();

    let result = server.get_channel("nonexistent-id");
    assert!(result.is_none());
}

#[tokio::test]
async fn test_delete_nonexistent_channel() {
    use framework::mocks::InMemoryStorage;

    let storage = InMemoryStorage::new();

    let deleted = storage.delete_channel("nonexistent");
    assert!(!deleted, "Deleting nonexistent channel should return false");
}

// ============================================================================
// Channel Count Tests
// ============================================================================

#[tokio::test]
async fn test_storage_channel_count() {
    use framework::mocks::InMemoryStorage;

    let storage = InMemoryStorage::new();

    assert_eq!(storage.channel_count(), 0);

    storage.save_channel(ChannelBuilder::public("ch1").build());
    assert_eq!(storage.channel_count(), 1);

    storage.save_channel(ChannelBuilder::public("ch2").build());
    assert_eq!(storage.channel_count(), 2);
}
