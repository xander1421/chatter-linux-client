//! End-to-end flow tests.
//!
//! Tests complete user workflows from registration through
//! messaging, using the full testing framework stack.

use crate::framework;

use framework::prelude::*;
use std::time::Duration;

// ============================================================================
// Complete User Registration and Login Flow
// ============================================================================

#[tokio::test]
async fn test_e2e_user_registration_flow() {
    let harness = TestHarness::unit().await;

    // Step 1: Generate user credentials
    let user = UserBuilder::new()
        .username("alice_test")
        .display_name("Alice Test")
        .build();

    assert!(!user.user_id.is_empty(), "User should have an ID");
    assert!(!user.password.is_empty(), "User should have a password");

    // Step 2: Generate prekey bundle for registration
    let (bundle, identity_key) = generate_prekey_bundle();
    assert!(bundle.signed_prekey.is_some(), "Should have signed prekey");
    assert!(!identity_key.is_empty(), "Should have identity key");

    // Step 3: Register in mock server
    harness.mock_server().register_user(user.clone());

    // Step 4: Verify registration
    let registered = harness.mock_server().get_user(&user.user_id);
    assert!(registered.is_some(), "User should be registered");
    assert_eq!(registered.unwrap().display_name, "Alice Test");
}

#[tokio::test]
async fn test_e2e_session_establishment_flow() {
    let harness = TestHarness::unit().await;

    // Register two users
    let alice = harness.register_user("Alice").await;
    let bob = harness.register_user("Bob").await;

    // Create sessions for both
    let alice_token = harness.mock_server().create_session(&alice.user_id, alice.device_id);
    let bob_token = harness.mock_server().create_session(&bob.user_id, bob.device_id);

    // Verify sessions are valid
    assert!(harness.mock_server().validate_session(&alice_token).is_some());
    assert!(harness.mock_server().validate_session(&bob_token).is_some());
}

// ============================================================================
// Complete DM Conversation Flow
// ============================================================================

#[tokio::test]
async fn test_e2e_dm_conversation_flow() {
    let harness = TestHarness::unit().await;
    let (alice, bob) = harness.conversation_pair().await;

    // Step 1: Create DM channel
    let dm = ChannelBuilder::build_dm_between(alice.user_id(), bob.user_id());
    let dm_id = harness.mock_server().create_channel(dm);

    // Step 2: Establish Signal session (simulated)
    let alice_bob_addr = format!("{}.{}", bob.user_id(), bob.device_id());
    harness.signal_stores.store_session(&alice_bob_addr, random_bytes(256));

    // Step 3: Alice sends first message
    let msg1 = MessageBuilder::text("Hey Bob, how's it going?")
        .in_channel(&dm_id)
        .from(alice.user_id())
        .build();
    harness.mock_server().store_message(msg1);

    // Step 4: Bob replies
    let msg2 = MessageBuilder::text("Hi Alice! I'm doing great, thanks!")
        .in_channel(&dm_id)
        .from(bob.user_id())
        .build();
    harness.mock_server().store_message(msg2);

    // Step 5: Alice sends another message
    let msg3 = MessageBuilder::text("Glad to hear it! Want to catch up later?")
        .in_channel(&dm_id)
        .from(alice.user_id())
        .build();
    harness.mock_server().store_message(msg3);

    // Step 6: Verify conversation
    let messages = harness.mock_server().get_messages(&dm_id, 10);
    assert_eq!(messages.len(), 3, "Should have 3 messages");
}

#[tokio::test]
async fn test_e2e_dm_with_attachments_flow() {
    let harness = TestHarness::unit().await;
    let (alice, bob) = harness.conversation_pair().await;

    let dm = ChannelBuilder::build_dm_between(alice.user_id(), bob.user_id());
    let dm_id = harness.mock_server().create_channel(dm);

    // Alice sends photo
    let photo = AttachmentBuilder::image("vacation.jpg")
        .size(2 * 1024 * 1024)
        .build();

    let msg = MessageBuilder::text("Check out this photo from my vacation!")
        .in_channel(&dm_id)
        .from(alice.user_id())
        .with_attachment(photo)
        .build();
    harness.mock_server().store_message(msg);

    // Verify
    let messages = harness.mock_server().get_messages(&dm_id, 10);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].attachments.len(), 1);
    assert_eq!(messages[0].attachments[0].filename, "vacation.jpg");
}

// ============================================================================
// Complete Group Channel Flow
// ============================================================================

#[tokio::test]
async fn test_e2e_group_channel_flow() {
    let harness = TestHarness::unit().await;
    let (alice, bob, charlie) = harness.conversation_trio().await;

    // Step 1: Alice creates a group channel
    let channel = ChannelBuilder::private("Project Team")
        .created_by(alice.user_id())
        .with_member(bob.user_id())
        .with_member(charlie.user_id())
        .with_description("Our project discussion channel")
        .build();
    let channel_id = harness.mock_server().create_channel(channel);

    // Step 2: Verify channel and members
    let retrieved = harness.mock_server().get_channel(&channel_id).unwrap();
    assert_eq!(retrieved.member_ids.len(), 3);

    // Step 3: Group conversation
    let conversation = vec![
        (alice.user_id(), "Hey team, let's discuss the roadmap"),
        (bob.user_id(), "Sounds good! What's the priority?"),
        (charlie.user_id(), "I think we should focus on the MVP first"),
        (alice.user_id(), "Agreed. Let's break down the tasks"),
    ];

    for (sender, content) in conversation {
        let msg = MessageBuilder::text(content)
            .in_channel(&channel_id)
            .from(sender)
            .build();
        harness.mock_server().store_message(msg);
    }

    // Step 4: Verify all messages
    let messages = harness.mock_server().get_messages(&channel_id, 10);
    assert_eq!(messages.len(), 4);
}

// ============================================================================
// Complete Thread/Reply Flow
// ============================================================================

#[tokio::test]
async fn test_e2e_thread_flow() {
    let harness = TestHarness::unit().await;
    let (alice, bob) = harness.conversation_pair().await;

    let channel = ChannelBuilder::public("general")
        .created_by(alice.user_id())
        .with_member(bob.user_id())
        .build();
    let channel_id = harness.mock_server().create_channel(channel);

    // Alice posts original message
    let original = MessageBuilder::text("Anyone want to grab lunch?")
        .in_channel(&channel_id)
        .from(alice.user_id())
        .build();
    let original_id = original.id.clone();
    harness.mock_server().store_message(original);

    // Bob replies in thread
    let reply1 = MessageBuilder::text("Sure! What time?")
        .in_channel(&channel_id)
        .from(bob.user_id())
        .reply_to(&original_id)
        .build();
    harness.mock_server().store_message(reply1);

    // Alice responds
    let reply2 = MessageBuilder::text("How about noon?")
        .in_channel(&channel_id)
        .from(alice.user_id())
        .reply_to(&original_id)
        .build();
    harness.mock_server().store_message(reply2);

    // Bob confirms
    let reply3 = MessageBuilder::text("Perfect, see you then!")
        .in_channel(&channel_id)
        .from(bob.user_id())
        .reply_to(&original_id)
        .build();
    harness.mock_server().store_message(reply3);

    // Verify thread
    let messages = harness.mock_server().get_messages(&channel_id, 10);
    assert_eq!(messages.len(), 4);

    // Count thread replies
    let replies: Vec<_> = messages
        .iter()
        .filter(|m| m.reply_to == Some(original_id.clone()))
        .collect();
    assert_eq!(replies.len(), 3, "Should have 3 replies");
}

// ============================================================================
// Multi-Channel Workflow
// ============================================================================

#[tokio::test]
async fn test_e2e_multi_channel_workflow() {
    let harness = TestHarness::unit().await;
    let (alice, bob, charlie) = harness.conversation_trio().await;

    // Create multiple channels
    let general = ChannelBuilder::public("general")
        .created_by(alice.user_id())
        .with_members(vec![bob.user_id(), charlie.user_id()])
        .build();
    let general_id = harness.mock_server().create_channel(general);

    let random = ChannelBuilder::public("random")
        .created_by(bob.user_id())
        .with_members(vec![alice.user_id(), charlie.user_id()])
        .build();
    let random_id = harness.mock_server().create_channel(random);

    let dm_alice_bob = ChannelBuilder::build_dm_between(alice.user_id(), bob.user_id());
    let dm_id = harness.mock_server().create_channel(dm_alice_bob);

    // Activity in different channels
    harness.mock_server().store_message(
        MessageBuilder::text("Welcome everyone!")
            .in_channel(&general_id)
            .from(alice.user_id())
            .build()
    );

    harness.mock_server().store_message(
        MessageBuilder::text("Check out this meme")
            .in_channel(&random_id)
            .from(bob.user_id())
            .build()
    );

    harness.mock_server().store_message(
        MessageBuilder::text("Private message")
            .in_channel(&dm_id)
            .from(alice.user_id())
            .build()
    );

    // Verify isolation
    assert_eq!(harness.mock_server().get_messages(&general_id, 10).len(), 1);
    assert_eq!(harness.mock_server().get_messages(&random_id, 10).len(), 1);
    assert_eq!(harness.mock_server().get_messages(&dm_id, 10).len(), 1);
}

// ============================================================================
// Session Management Flow
// ============================================================================

#[tokio::test]
async fn test_e2e_multi_device_flow() {
    let harness = TestHarness::unit().await;

    let alice = harness.register_user("Alice").await;

    // Alice logs in from multiple devices
    let device1_token = harness.mock_server().create_session(&alice.user_id, 1);
    let device2_token = harness.mock_server().create_session(&alice.user_id, 2);
    let device3_token = harness.mock_server().create_session(&alice.user_id, 3);

    // All sessions should be valid
    assert!(harness.mock_server().validate_session(&device1_token).is_some());
    assert!(harness.mock_server().validate_session(&device2_token).is_some());
    assert!(harness.mock_server().validate_session(&device3_token).is_some());

    // Create Signal sessions for each device
    let bob = harness.register_user("Bob").await;
    for device_id in 1..=3 {
        let addr = format!("{}.{}", bob.user_id, device_id);
        harness.signal_stores.store_session(&addr, random_bytes(256));
        assert!(harness.signal_stores.has_session(&addr));
    }
}

// ============================================================================
// Media Sharing Flow
// ============================================================================

#[tokio::test]
async fn test_e2e_media_sharing_flow() {
    let harness = TestHarness::unit().await;
    let (alice, bob) = harness.conversation_pair().await;

    let channel = ChannelBuilder::public("media-sharing")
        .created_by(alice.user_id())
        .with_member(bob.user_id())
        .build();
    let channel_id = harness.mock_server().create_channel(channel);

    // Alice shares an image
    let image = AttachmentBuilder::image("sunset.jpg")
        .size(3 * 1024 * 1024)
        .build();
    harness.mock_server().store_message(
        MessageBuilder::text("Beautiful sunset!")
            .in_channel(&channel_id)
            .from(alice.user_id())
            .with_attachment(image)
            .build()
    );

    // Bob shares a video
    let video = AttachmentBuilder::video("funny_cat.mp4")
        .size(15 * 1024 * 1024)
        .build();
    harness.mock_server().store_message(
        MessageBuilder::text("Check out this cat video!")
            .in_channel(&channel_id)
            .from(bob.user_id())
            .with_attachment(video)
            .build()
    );

    // Alice shares a document
    let doc = AttachmentBuilder::document("meeting_notes.pdf")
        .size(500 * 1024)
        .build();
    harness.mock_server().store_message(
        MessageBuilder::text("Meeting notes attached")
            .in_channel(&channel_id)
            .from(alice.user_id())
            .with_attachment(doc)
            .build()
    );

    // Verify all media
    let messages = harness.mock_server().get_messages(&channel_id, 10);
    assert_eq!(messages.len(), 3);

    let total_attachments: usize = messages.iter().map(|m| m.attachments.len()).sum();
    assert_eq!(total_attachments, 3);
}

// ============================================================================
// Crypto Key Lifecycle Flow
// ============================================================================

#[tokio::test]
async fn test_e2e_crypto_key_lifecycle() {
    use framework::mocks::InMemorySignalStores;

    // Step 1: Generate identity at registration
    let stores = InMemorySignalStores::new();
    let (private, public) = stores.identity_key_pair();
    assert_eq!(private.len(), 32);
    assert_eq!(public.len(), 33);

    // Step 2: Generate prekeys
    let (prekey_id, prekey_priv, _prekey_pub) = CryptoBuilder::prekey(1);
    assert_eq!(prekey_id, 1);
    assert_eq!(prekey_priv.len(), 32);

    // Step 3: Generate signed prekey
    let (spk_id, _spk_priv, _spk_pub, signature) = CryptoBuilder::signed_prekey(1);
    assert_eq!(spk_id, 1);
    assert_eq!(signature.len(), 64);

    // Step 4: Establish session
    let peer_address = "bob.1";
    stores.store_session(peer_address, random_bytes(256));
    assert!(stores.has_session(peer_address));

    // Step 5: Store peer identity
    stores.store_identity(peer_address, CryptoBuilder::identity_key_pair().1);
    assert!(stores.load_identity(peer_address).is_some());
}

// ============================================================================
// Harness Reset Flow
// ============================================================================

#[tokio::test]
async fn test_e2e_harness_reset_flow() {
    let harness = TestHarness::unit().await;

    // Create some state
    let alice = harness.register_user("Alice").await;
    let bob = harness.register_user("Bob").await;

    let channel = ChannelBuilder::public("test").build();
    let channel_id = harness.mock_server().create_channel(channel);

    harness.mock_server().store_message(
        MessageBuilder::text("Hello").in_channel(&channel_id).build()
    );

    // Verify state exists
    assert!(harness.mock_server().get_user(&alice.user_id).is_some());
    assert!(harness.mock_server().get_user(&bob.user_id).is_some());
    assert!(harness.mock_server().get_channel(&channel_id).is_some());
    assert_eq!(harness.mock_server().get_messages(&channel_id, 10).len(), 1);

    // Reset
    harness.reset().await;

    // All state should be cleared
    assert!(harness.mock_server().get_user(&alice.user_id).is_none());
    assert!(harness.mock_server().get_user(&bob.user_id).is_none());
    assert!(harness.mock_server().get_channel(&channel_id).is_none());
}

// ============================================================================
// Network Conditions Flow
// ============================================================================

#[tokio::test]
async fn test_e2e_with_network_latency() {
    let harness = TestHarness::builder()
        .with_mock_network()
        .with_latency(Duration::from_millis(5))
        .build()
        .await;

    let (alice, bob) = harness.conversation_pair().await;

    let channel = ChannelBuilder::public("latency-test").build();
    let channel_id = harness.mock_server().create_channel(channel);

    // Operations should still work despite latency
    for i in 0..3 {
        let msg = MessageBuilder::text(&format!("Message {}", i))
            .in_channel(&channel_id)
            .from(if i % 2 == 0 { alice.user_id() } else { bob.user_id() })
            .build();
        harness.mock_server().store_message(msg);
    }

    let messages = harness.mock_server().get_messages(&channel_id, 10);
    assert_eq!(messages.len(), 3);
}

// ============================================================================
// Offline Mode Flow
// ============================================================================

#[tokio::test]
async fn test_e2e_offline_network_detection() {
    let harness = TestHarness::builder()
        .offline()
        .build()
        .await;

    // Network should indicate offline
    assert!(harness.network.should_fail(), "Offline network should fail");
}

// ============================================================================
// Complete Message Lifecycle
// ============================================================================

#[tokio::test]
async fn test_e2e_message_lifecycle() {
    let harness = TestHarness::unit().await;
    let (alice, _bob) = harness.conversation_pair().await;

    let channel = ChannelBuilder::public("lifecycle-test").build();
    let channel_id = harness.mock_server().create_channel(channel);

    // Create message
    let msg = MessageBuilder::text("Original message")
        .in_channel(&channel_id)
        .from(alice.user_id())
        .build();
    let msg_id = msg.id.clone();

    // Store (send)
    harness.mock_server().store_message(msg);

    // Retrieve
    let messages = harness.mock_server().get_messages(&channel_id, 10);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].id, msg_id);
    assert_eq!(messages[0].content, "Original message");

    // In a real scenario, edit and delete would be tested here
    // Our mock server doesn't have edit/delete, but the flow is demonstrated
}
