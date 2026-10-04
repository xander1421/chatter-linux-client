//! Framework tests - verifies the testing framework compiles and works.

use crate::framework;

use framework::prelude::*;

// ============================================================================
// Framework Smoke Tests
// ============================================================================

#[test]
fn test_user_builder() {
    let user = UserBuilder::new().build();
    assert!(!user.user_id.is_empty());
    assert!(!user.username.is_empty());
    assert!(!user.display_name.is_empty());
}

#[test]
fn test_user_builder_named() {
    let alice = UserBuilder::named("Alice").build();
    assert_eq!(alice.display_name, "Alice");
}

#[test]
fn test_user_builder_pair() {
    let (alice, bob) = UserBuilder::build_pair();
    assert_eq!(alice.display_name, "Alice");
    assert_eq!(bob.display_name, "Bob");
    assert_ne!(alice.user_id, bob.user_id);
}

#[test]
fn test_message_builder() {
    let msg = MessageBuilder::text("Hello, world!")
        .in_channel("channel-1")
        .from("user-1")
        .build();

    assert_eq!(msg.content, "Hello, world!");
    assert_eq!(msg.channel_id, "channel-1");
    assert_eq!(msg.sender_id, "user-1");
    assert!(msg.encrypted);
}

#[test]
fn test_message_builder_with_reply() {
    let msg = MessageBuilder::text("Reply")
        .reply_to("original-msg")
        .build();

    assert_eq!(msg.reply_to, Some("original-msg".to_string()));
}

#[test]
fn test_channel_builder() {
    let channel = ChannelBuilder::public("test-channel")
        .with_description("A test channel")
        .created_by("user-1")
        .with_member("user-2")
        .build();

    assert_eq!(channel.name, "test-channel");
    assert_eq!(channel.description, Some("A test channel".to_string()));
    assert!(channel.member_ids.contains(&"user-1".to_string()));
    assert!(channel.member_ids.contains(&"user-2".to_string()));
}

#[test]
fn test_channel_builder_dm() {
    let dm = ChannelBuilder::build_dm_between("alice", "bob");
    assert_eq!(dm.channel_type, 3); // DM type
    assert_eq!(dm.member_ids.len(), 2);
}

#[test]
fn test_attachment_builder() {
    let attachment = AttachmentBuilder::image("photo.jpg").build();
    assert_eq!(attachment.filename, "photo.jpg");
    assert_eq!(attachment.mime_type, "image/jpeg");
    assert!(!attachment.key.is_empty());
    assert!(!attachment.digest.is_empty());
}

#[test]
fn test_crypto_builder() {
    let (private, public) = CryptoBuilder::identity_key_pair();
    assert_eq!(private.len(), 32);
    assert_eq!(public.len(), 33);
    assert_eq!(public[0], 0x05); // DJB key type prefix
}

#[test]
fn test_session_builder() {
    let session = SessionBuilder::new("user-1", 1)
        .established()
        .build();

    assert_eq!(session.user_id, "user-1");
    assert_eq!(session.device_id, 1);
    assert!(session.established);
    assert!(!session.session_data.is_empty());
}

// ============================================================================
// In-Memory Storage Tests
// ============================================================================

#[test]
fn test_in_memory_storage() {
    use framework::mocks::InMemoryStorage;

    let storage = InMemoryStorage::new();

    // Test message storage
    let msg = MessageBuilder::text("Test")
        .in_channel("channel-1")
        .build();
    let msg_id = msg.id.clone();

    storage.save_message(msg);
    assert_eq!(storage.message_count(), 1);

    let retrieved = storage.get_message(&msg_id);
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().content, "Test");

    // Test channel storage
    let channel = ChannelBuilder::public("test").build();
    let channel_id = channel.id.clone();

    storage.save_channel(channel);
    assert_eq!(storage.channel_count(), 1);

    let retrieved = storage.get_channel(&channel_id);
    assert!(retrieved.is_some());

    // Test clear
    storage.clear();
    assert_eq!(storage.message_count(), 0);
    assert_eq!(storage.channel_count(), 0);
}

// ============================================================================
// In-Memory Signal Stores Tests
// ============================================================================

#[test]
fn test_in_memory_signal_stores() {
    use framework::mocks::InMemorySignalStores;

    let stores = InMemorySignalStores::new();

    // Identity key pair should be generated
    let (private, public) = stores.identity_key_pair();
    assert_eq!(private.len(), 32);
    assert_eq!(public.len(), 33);

    // Registration ID should be valid
    let reg_id = stores.registration_id();
    assert!(reg_id > 0 && reg_id <= 16380);

    // Test session storage
    let address = "user-1.1";
    assert!(!stores.has_session(address));

    stores.store_session(address, vec![1, 2, 3]);
    assert!(stores.has_session(address));

    let session = stores.load_session(address);
    assert_eq!(session, Some(vec![1, 2, 3]));

    // Test deterministic stores
    let stores1 = InMemorySignalStores::deterministic(42);
    let stores2 = InMemorySignalStores::deterministic(42);
    assert_eq!(stores1.identity_key_pair(), stores2.identity_key_pair());
    assert_eq!(stores1.registration_id(), stores2.registration_id());
}

// ============================================================================
// Network Simulator Tests
// ============================================================================

#[tokio::test]
async fn test_network_simulator_perfect() {
    use framework::mocks::NetworkSimulator;

    let network = NetworkSimulator::perfect();

    // Should never fail
    for _ in 0..100 {
        assert!(!network.should_fail());
    }
}

#[tokio::test]
async fn test_network_simulator_offline() {
    use framework::mocks::NetworkSimulator;

    let network = NetworkSimulator::offline();

    // Should always fail
    for _ in 0..10 {
        assert!(network.should_fail());
    }
}

// ============================================================================
// Mock Server Tests
// ============================================================================

#[test]
fn test_mock_grpc_server() {
    use framework::mocks::MockGrpcServer;

    let server = MockGrpcServer::new();

    // Register user
    let user = UserBuilder::named("Alice").build();
    let user_id = server.register_user(user.clone());
    assert_eq!(user_id, user.user_id);

    // Retrieve user
    let retrieved = server.get_user(&user_id);
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().display_name, "Alice");

    // Create channel
    let channel = ChannelBuilder::public("test").build();
    let channel_id = server.create_channel(channel.clone());

    let retrieved = server.get_channel(&channel_id);
    assert!(retrieved.is_some());

    // Store message
    let msg = MessageBuilder::text("Hello")
        .in_channel(&channel_id)
        .from(&user_id)
        .build();
    server.store_message(msg);

    let messages = server.get_messages(&channel_id, 10);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, "Hello");

    // Clear
    server.clear();
    assert!(server.get_user(&user_id).is_none());
}

// ============================================================================
// Harness Tests
// ============================================================================

#[tokio::test]
async fn test_harness_unit_mode() {
    let harness = TestHarness::unit().await;

    // Should have mock server in unit mode
    assert!(harness.mock_server.is_some());

    // Register users
    let alice = harness.register_user("Alice").await;
    let bob = harness.register_user("Bob").await;

    assert_eq!(alice.display_name, "Alice");
    assert_eq!(bob.display_name, "Bob");
    assert_ne!(alice.user_id, bob.user_id);

    // Get users
    let users = harness.get_users().await;
    assert_eq!(users.len(), 2);

    // Reset
    harness.reset().await;
    let users = harness.get_users().await;
    assert_eq!(users.len(), 0);
}

#[tokio::test]
async fn test_harness_conversation_pair() {
    let harness = TestHarness::unit().await;
    let (alice, bob) = harness.conversation_pair().await;

    assert_eq!(alice.display_name(), "Alice");
    assert_eq!(bob.display_name(), "Bob");
}

#[tokio::test]
async fn test_harness_builder() {
    let harness = TestHarness::builder()
        .with_mock_network()
        .with_passthrough_crypto()
        .with_memory_storage()
        .build()
        .await;

    assert!(harness.mock_server.is_some());
}

// ============================================================================
// Fixtures Tests
// ============================================================================

#[test]
fn test_fixtures() {
    let username = unique_username();
    assert!(username.starts_with("test_user_"));

    let password = random_password();
    assert!(password.starts_with("TestPass_"));

    let uak = random_uak();
    assert_eq!(uak.len(), 16);

    let (bundle, identity_key) = generate_prekey_bundle();
    assert!(!identity_key.is_empty());
    assert!(bundle.signed_prekey.is_some());
}
