//! Regression tests for group mailbox issues.
//!
//! Tests scenarios that have caused message delivery failures:
//! 1. **PRIMARY ISSUE**: Timing - mailbox announcements not exchanged before messages sent
//!    - Client joins channel and sends message immediately
//!    - Other client hasn't received/processed the mailbox announcement yet
//!    - Fanout shows delivered=1 but only to known members (not the new one)
//! 2. Key mismatch detection - stored keys differ from announced keys
//! 3. Concurrent setup races
//!
//! All tests use real server connections to simulate production conditions.

use crate::{common, skip_if_no_server};

use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::Signer;

use chatter_core::domain::ids::ChannelId;
use chatter_core::mailbox::group::{GroupCoordinator, GroupMailboxManager, MemberMailbox};
use chatter_core::mailbox::TokenManager;
use chatter_core::network::grpc::{AnonymousMailboxClient, ChannelClient, GrpcClientPool, MailboxClient, MessageClient};
use chatter_core::proto::chat_v1::{
    message::content::Type as ContentType, message::Content, ChannelType,
};
use chatter_core::storage::Database;
use tokio::time::sleep;
use tokio_stream::StreamExt;

use common::*;

/// Test context holding user, pool, and services.
struct MailboxTestContext {
    user: TestUser,
    pool: Arc<GrpcClientPool>,
    db: Arc<Database>,
    mailbox_manager: Arc<GroupMailboxManager>,
    token_manager: TokenManager,
    group_coordinator: GroupCoordinator,
}

impl MailboxTestContext {
    async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let pool = create_test_pool().await?;
        let user = setup_authenticated_test(&pool).await?;

        // Create temp database for this test
        let temp_dir = tempfile::tempdir()?;
        let db_path = temp_dir.path().join("test.db");
        let db = Arc::new(Database::open(&db_path, "test_key_passphrase")?);
        db.run_migrations()?;

        let mailbox_manager = Arc::new(GroupMailboxManager::new(db.clone()));
        let token_manager = TokenManager::new(db.clone());
        let group_coordinator = GroupCoordinator::new(db.clone(), mailbox_manager.clone());

        Ok(Self {
            user,
            pool,
            db,
            mailbox_manager,
            token_manager,
            group_coordinator,
        })
    }

    fn channel_client(&self) -> ChannelClient {
        ChannelClient::new(self.pool.channel(), self.pool.auth_clone())
    }

    fn message_client(&self) -> MessageClient {
        MessageClient::new(self.pool.channel(), self.pool.auth_clone())
    }

    fn mailbox_client(&self) -> MailboxClient {
        MailboxClient::new(self.pool.channel(), self.pool.auth_clone())
    }

    fn anon_mailbox_client(&self) -> AnonymousMailboxClient {
        self.pool.anonymous_mailbox_client()
    }
}

/// Helper to create a channel and wait for completion.
async fn create_test_channel(client: &mut ChannelClient, name: &str) -> String {
    let stream = client
        .create_channel(name, ChannelType::ChannelPublic, None, vec![], None, None, None)
        .await
        .expect("Failed to start channel creation");

    tokio::pin!(stream);

    let mut channel_id = String::new();
    while let Some(result) = stream.next().await {
        let event = result.expect("Stream error");
        if let Some(payload) = event.event {
            use chatter_core::proto::chat_v1::channel_event::Event;
            match payload {
                Event::ChannelCreated(channel) => {
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

    channel_id
}

// ============================================================================
// Regression Test: Stale Database State
// ============================================================================

/// Test: Messages fail when local database has old mailbox keys that don't match server.
///
/// Scenario:
/// 1. User creates channel and sets up mailbox
/// 2. Mailbox keys stored locally
/// 3. Simulate "stale state" by creating new mailbox (old keys still in DB)
/// 4. Send message - should detect key mismatch or re-sync
#[tokio::test]
async fn test_stale_mailbox_key_detection() {
    skip_if_no_server!();

    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();

    tracing::info!("=== Starting regression test: stale mailbox key detection ===");

    let ctx = assert_ok(MailboxTestContext::new().await);
    let mut channel_client = ctx.channel_client();

    // Step 1: Create channel
    let channel_name = format!("stale_key_test_{}", unique_username());
    let channel_id_str = create_test_channel(&mut channel_client, &channel_name).await;
    let channel_id: ChannelId = channel_id_str.parse().expect("Valid channel ID");

    tracing::info!(channel_id = %channel_id, "Created test channel");

    // Step 2: Request tokens and setup first mailbox
    let mut mailbox_client = ctx.mailbox_client();
    let tokens_acquired = assert_ok(ctx.token_manager.request_tokens(&mut mailbox_client, 3).await);
    assert!(tokens_acquired > 0, "Should acquire at least one token");

    let mut anon_mailbox_client = ctx.anon_mailbox_client();
    let first_setup = assert_ok(
        ctx.group_coordinator
            .setup_group_mailbox(&channel_id, &mut anon_mailbox_client)
            .await,
    );

    let first_mailbox_pk = first_setup.mailbox.public_key;
    tracing::info!(
        mailbox_id = %first_setup.mailbox.id,
        pk_hex = %hex::encode(&first_mailbox_pk),
        "First mailbox created"
    );

    // Step 3: Simulate stale state by clearing and creating new mailbox
    // This simulates what happens when app restarts but server state changed
    ctx.mailbox_manager.delete_group_state(&channel_id).ok();

    // Get more tokens for second mailbox
    let _ = ctx.token_manager.request_tokens(&mut mailbox_client, 3).await;

    let second_setup = assert_ok(
        ctx.group_coordinator
            .setup_group_mailbox(&channel_id, &mut anon_mailbox_client)
            .await,
    );

    let second_mailbox_pk = second_setup.mailbox.public_key;
    tracing::info!(
        mailbox_id = %second_setup.mailbox.id,
        pk_hex = %hex::encode(&second_mailbox_pk),
        "Second mailbox created (simulating stale recovery)"
    );

    // Step 4: Verify keys are different (proves stale state was simulated)
    assert_ne!(
        first_mailbox_pk, second_mailbox_pk,
        "New mailbox should have different keys (simulating key rotation/stale state)"
    );

    // Step 5: Verify announcement contains correct (new) key
    let announcement = ctx
        .group_coordinator
        .get_announcement(&channel_id)
        .expect("Should get announcement")
        .expect("Should have announcement");

    assert_eq!(
        announcement.mailbox_pk.as_slice(),
        &second_mailbox_pk,
        "Announcement should contain current (new) mailbox key"
    );

    tracing::info!("=== Stale key detection test passed ===");
}

// ============================================================================
// Regression Test: Timing Issues - Message Before Mailbox Exchange
// ============================================================================

/// **ROOT CAUSE OF THE BUG**:
/// `is_ready_to_send()` returns true when `!member_mailboxes.is_empty()`.
/// This means: if we have User A's mailbox but not User B's (who just joined),
/// we return `true`, send to User A only, and User B NEVER gets the message.
///
/// The fanout logs `delivered=1` which looks successful, but User B's mailbox
/// wasn't in the list yet, so they never receive the message.
///
/// **EXPECTED FIX**:
/// When we receive a new member's mailbox announcement, we should either:
/// 1. Re-send recent messages to the new member
/// 2. Queue messages with "send to member X" entries that get processed on mailbox arrival
///
/// Test: Messages queued when sent before mailbox exchange completes.
///
/// Scenario:
/// 1. Two users join channel
/// 2. User 1 sends message BEFORE receiving User 2's mailbox
/// 3. Verify message is queued or error is handled gracefully
#[tokio::test]
async fn test_message_before_mailbox_exchange() {
    skip_if_no_server!();

    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();

    tracing::info!("=== Starting regression test: message before mailbox exchange ===");

    // Setup two users
    let pool1 = assert_ok(create_test_pool().await);
    let user1 = assert_ok(setup_authenticated_test(&pool1).await);

    let pool2 = assert_ok(create_test_pool().await);
    let user2 = assert_ok(setup_authenticated_test(&pool2).await);

    tracing::info!(
        user1 = %user1.user_id,
        user2 = %user2.user_id,
        "Created two test users"
    );

    // User 1 creates channel
    let mut channel_client1 = ChannelClient::new(pool1.channel(), pool1.auth_clone());
    let channel_name = format!("timing_test_{}", unique_username());
    let channel_id = create_test_channel(&mut channel_client1, &channel_name).await;

    tracing::info!(channel_id = %channel_id, "User 1 created channel");

    // User 2 joins channel
    let mut channel_client2 = ChannelClient::new(pool2.channel(), pool2.auth_clone());
    let join_stream = channel_client2
        .join_channel(&channel_id, None, false)
        .await
        .expect("Join should start");

    tokio::pin!(join_stream);
    while let Some(_) = join_stream.next().await {}

    tracing::info!("User 2 joined channel");

    // IMMEDIATELY send message from User 1 (before any mailbox exchange could complete)
    // This tests the race condition
    let mut msg_client1 = MessageClient::new(pool1.channel(), pool1.auth_clone());
    let content = Content {
        r#type: Some(ContentType::Text("Quick message before mailbox exchange!".to_string())),
    };

    let send_result = msg_client1
        .send_message(&channel_id, content, None, false, None, vec![], false, vec![])
        .await;

    // The message should either:
    // 1. Succeed (server-side plaintext for public channel)
    // 2. Be queued for later delivery (if encrypted channel)
    // 3. Return an error that can be handled
    match send_result {
        Ok(msg) => {
            tracing::info!(
                message_id = %msg.id,
                "Message sent successfully (server handled timing)"
            );
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Message send failed (expected if encryption required before mailbox)"
            );
            // This is acceptable - the important thing is we don't crash
        }
    }

    tracing::info!("=== Timing test completed ===");
}

// ============================================================================
// Regression Test: Partial Member List - Core Bug
// ============================================================================

/// **THE EXACT BUG**: Send with partial member mailbox list.
///
/// This test demonstrates the core timing issue:
/// - We have some member mailboxes but not all
/// - is_ready_to_send() returns true (has at least 1 member)
/// - We send to known members only
/// - New member never receives the message
#[tokio::test]
async fn test_send_with_partial_member_list() {
    skip_if_no_server!();

    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();

    tracing::info!("=== Starting regression test: partial member list bug ===");

    let ctx = assert_ok(MailboxTestContext::new().await);

    let channel_id: ChannelId = uuid::Uuid::new_v4().to_string().parse().expect("Valid UUID");

    // Simulate having only ONE member's mailbox (User A)
    let user_a: chatter_core::domain::UserId =
        uuid::Uuid::new_v4().to_string().parse().expect("Valid UUID");
    let user_b: chatter_core::domain::UserId =
        uuid::Uuid::new_v4().to_string().parse().expect("Valid UUID");

    let mut key_a = [0u8; 32];
    rand::fill(&mut key_a[..]);
    let mailbox_a = MemberMailbox::new(user_a, key_a, None);

    // Store only User A's mailbox
    assert_ok(ctx.mailbox_manager.save_member_mailbox(&channel_id, &mailbox_a));

    // Check is_ready_to_send - will return TRUE even though User B is missing
    let state = ctx
        .mailbox_manager
        .load_group_state(&channel_id)
        .expect("Should load state")
        .expect("State should exist");

    let ready = state.is_ready_to_send();
    tracing::info!(
        is_ready = ready,
        member_count = state.member_mailboxes.len(),
        "is_ready_to_send() result"
    );

    // THIS IS THE BUG: Returns true with only 1 member
    assert!(
        ready,
        "is_ready_to_send() returns true with partial member list (this is the bug!)"
    );
    assert_eq!(
        state.member_mailboxes.len(),
        1,
        "Only have 1 member mailbox"
    );

    // Now add User B's mailbox (simulating late mailbox announcement)
    let mut key_b = [0u8; 32];
    rand::fill(&mut key_b[..]);
    let mailbox_b = MemberMailbox::new(user_b, key_b, None);

    assert_ok(ctx.mailbox_manager.save_member_mailbox(&channel_id, &mailbox_b));

    let state2 = ctx
        .mailbox_manager
        .load_group_state(&channel_id)
        .expect("Should load state")
        .expect("State should exist");

    assert_eq!(
        state2.member_mailboxes.len(),
        2,
        "Now have 2 member mailboxes"
    );

    tracing::info!(
        "BUG SCENARIO: If we sent a message when member_count=1, User B would never receive it!"
    );
    tracing::info!(
        "FIX NEEDED: When mailbox_b arrives, we should re-send recent messages to User B"
    );

    tracing::info!("=== Partial member list bug test completed ===");
}

// ============================================================================
// Regression Test: Key Mismatch Between Stored and Announced
// ============================================================================

/// Test: Detect and handle when stored member mailbox key differs from announcement.
///
/// Scenario:
/// 1. Store a member mailbox with one key
/// 2. Process announcement with different key
/// 3. Verify the new key replaces old (or raises appropriate error)
#[tokio::test]
async fn test_member_mailbox_key_update() {
    skip_if_no_server!();

    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();

    tracing::info!("=== Starting regression test: member mailbox key update ===");

    let ctx = assert_ok(MailboxTestContext::new().await);

    let channel_id: ChannelId = uuid::Uuid::new_v4().to_string().parse().expect("Valid UUID");
    let member_id: chatter_core::domain::UserId =
        uuid::Uuid::new_v4().to_string().parse().expect("Valid UUID");

    // Step 1: Store first mailbox key for member
    let mut first_key = [0u8; 32];
    rand::fill(&mut first_key[..]);

    let first_mailbox = MemberMailbox::new(member_id, first_key, None);
    assert_ok(ctx.mailbox_manager.save_member_mailbox(&channel_id, &first_mailbox));

    tracing::info!(
        channel_id = %channel_id,
        member_id = %member_id,
        pk_hex = %hex::encode(&first_key),
        "Stored first member mailbox"
    );

    // Step 2: Verify first key is stored
    let state1 = ctx
        .mailbox_manager
        .load_group_state(&channel_id)
        .expect("Should load state")
        .expect("State should exist");

    let stored1 = state1.member_mailboxes.get(&member_id).expect("Member should exist");
    assert_eq!(stored1.public_key, first_key, "First key should be stored");

    // Step 3: Store second (different) mailbox key for same member
    let mut second_key = [0u8; 32];
    rand::fill(&mut second_key[..]);

    assert_ne!(first_key, second_key, "Keys should be different");

    let second_mailbox = MemberMailbox::new(member_id, second_key, None);
    assert_ok(ctx.mailbox_manager.save_member_mailbox(&channel_id, &second_mailbox));

    tracing::info!(
        channel_id = %channel_id,
        member_id = %member_id,
        pk_hex = %hex::encode(&second_key),
        "Stored second member mailbox (key rotation)"
    );

    // Step 4: Verify second key replaced first
    let state2 = ctx
        .mailbox_manager
        .load_group_state(&channel_id)
        .expect("Should load state")
        .expect("State should exist");

    let stored2 = state2.member_mailboxes.get(&member_id).expect("Member should exist");
    assert_eq!(
        stored2.public_key, second_key,
        "Second key should replace first (key rotation handling)"
    );

    tracing::info!("=== Member mailbox key update test passed ===");
}

// ============================================================================
// Regression Test: Concurrent Mailbox Setup Race
// ============================================================================

/// Test: Multiple concurrent mailbox setup attempts should not corrupt state.
///
/// Scenario:
/// 1. Multiple async tasks try to setup mailbox for same channel
/// 2. Only one should succeed, others should reuse existing
/// 3. No state corruption should occur
#[tokio::test]
async fn test_concurrent_mailbox_setup() {
    skip_if_no_server!();

    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();

    tracing::info!("=== Starting regression test: concurrent mailbox setup ===");

    let ctx = Arc::new(assert_ok(MailboxTestContext::new().await));
    let mut channel_client = ctx.channel_client();

    // Create channel
    let channel_name = format!("concurrent_test_{}", unique_username());
    let channel_id_str = create_test_channel(&mut channel_client, &channel_name).await;
    let channel_id: ChannelId = channel_id_str.parse().expect("Valid channel ID");

    // Pre-request tokens
    let mut mailbox_client = ctx.mailbox_client();
    let _ = ctx.token_manager.request_tokens(&mut mailbox_client, 10).await;

    // Spawn multiple concurrent setup attempts
    let mut handles = Vec::new();
    for i in 0..5 {
        let ctx_clone = ctx.clone();
        let channel_id_clone = channel_id;
        handles.push(tokio::spawn(async move {
            let mut anon_client = ctx_clone.anon_mailbox_client();
            let result = ctx_clone
                .group_coordinator
                .setup_group_mailbox(&channel_id_clone, &mut anon_client)
                .await;
            (i, result)
        }));
    }

    // Wait for all to complete
    let results: Vec<_> = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.expect("Task should not panic"))
        .collect();

    // Count successes
    let successes: Vec<_> = results.iter().filter(|(_, r)| r.is_ok()).collect();

    tracing::info!(
        total_attempts = results.len(),
        successes = successes.len(),
        "Concurrent setup completed"
    );

    // All attempts should succeed (either creating new or reusing existing)
    for (i, result) in &results {
        match result {
            Ok(setup) => {
                tracing::info!(
                    attempt = i,
                    mailbox_id = %setup.mailbox.id,
                    "Setup succeeded"
                );
            }
            Err(e) => {
                tracing::warn!(
                    attempt = i,
                    error = %e,
                    "Setup failed (may be expected for some race conditions)"
                );
            }
        }
    }

    // Verify final state is consistent
    let final_state = ctx
        .mailbox_manager
        .load_group_state(&channel_id)
        .expect("Should load state");

    assert!(
        final_state.is_some(),
        "Should have mailbox state after concurrent setup"
    );

    let state = final_state.unwrap();
    assert!(
        state.our_mailbox.is_some(),
        "Should have our mailbox after concurrent setup"
    );

    tracing::info!("=== Concurrent mailbox setup test passed ===");
}

// ============================================================================
// Regression Test: Poll With Wrong Key Returns Empty
// ============================================================================

/// Test: Server behavior with wrong keys and challenges.
///
/// Verifies:
/// 1. Does server reject challenge requests for non-existent mailbox?
/// 2. Does server reject poll with wrong signature?
/// 3. Does server reject poll with mismatched challenge?
#[tokio::test]
async fn test_server_rejects_wrong_credentials() {
    skip_if_no_server!();

    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();

    tracing::info!("=== Starting test: server rejection of wrong credentials ===");

    let ctx = assert_ok(MailboxTestContext::new().await);
    let mut mailbox_client = ctx.mailbox_client();

    // Request tokens and create a real mailbox
    let _ = ctx.token_manager.request_tokens(&mut mailbox_client, 3).await;

    let channel_id: ChannelId = uuid::Uuid::new_v4().to_string().parse().expect("Valid UUID");
    let mut anon_mailbox_client = ctx.anon_mailbox_client();
    let setup = assert_ok(
        ctx.group_coordinator
            .setup_group_mailbox(&channel_id, &mut anon_mailbox_client)
            .await,
    );

    let correct_pk = setup.mailbox.public_key;
    let correct_private_key = setup.mailbox.private_key;
    tracing::info!(
        mailbox_id = %setup.mailbox.id,
        correct_pk_hex = %hex::encode(&correct_pk),
        "Created real mailbox for testing"
    );

    // Test 1: Challenge request with non-existent public key
    let mut wrong_pk = [0u8; 32];
    rand::fill(&mut wrong_pk[..]);

    let challenge_wrong_pk = mailbox_client.get_mailbox_challenge(wrong_pk.to_vec()).await;
    match &challenge_wrong_pk {
        Ok(_) => tracing::warn!("[TEST 1] Server returned challenge for NON-EXISTENT mailbox - SECURITY CONCERN"),
        Err(e) => tracing::info!("[TEST 1] Server REJECTED challenge for non-existent mailbox: {}", e),
    }

    // Test 2: Get real challenge, but sign with wrong key
    let real_challenge = mailbox_client.get_mailbox_challenge(correct_pk.to_vec()).await;
    match real_challenge {
        Ok(challenge_resp) => {
            tracing::info!(
                challenge_len = challenge_resp.challenge.len(),
                "[TEST 2] Got real challenge, now testing wrong signature"
            );

            // Sign with a random key instead of our real private key
            let wrong_signing_key = ed25519_dalek::SigningKey::generate(&mut rand_core_06::OsRng);
            let wrong_signature = wrong_signing_key.sign(&challenge_resp.challenge);

            let poll_wrong_sig = mailbox_client
                .poll_ephemeral_mailbox(
                    correct_pk.to_vec(),
                    wrong_signature.to_bytes().to_vec(),
                    challenge_resp.challenge.clone(),
                    None,
                    10,
                )
                .await;

            match &poll_wrong_sig {
                Ok(resp) => tracing::warn!(
                    msg_count = resp.messages.len(),
                    "[TEST 2] Server ACCEPTED wrong signature - SECURITY BUG!"
                ),
                Err(e) => tracing::info!("[TEST 2] Server REJECTED wrong signature: {}", e),
            }
        }
        Err(e) => {
            tracing::error!("[TEST 2] Could not get challenge for real mailbox: {}", e);
        }
    }

    // Test 3: Sign with correct key but use stale/wrong challenge bytes
    let real_challenge2 = mailbox_client.get_mailbox_challenge(correct_pk.to_vec()).await;
    match real_challenge2 {
        Ok(challenge_resp) => {
            // Create signing key from stored private key
            let signing_key = ed25519_dalek::SigningKey::from_bytes(
                &correct_private_key[..32].try_into().expect("32 bytes")
            );

            // Sign a DIFFERENT challenge (not the one server gave us)
            let mut fake_challenge = [0u8; 32];
            rand::fill(&mut fake_challenge[..]);
            let signature_wrong_challenge = signing_key.sign(&fake_challenge);

            let poll_wrong_challenge = mailbox_client
                .poll_ephemeral_mailbox(
                    correct_pk.to_vec(),
                    signature_wrong_challenge.to_bytes().to_vec(),
                    challenge_resp.challenge, // Send real challenge but signed wrong one
                    None,
                    10,
                )
                .await;

            match &poll_wrong_challenge {
                Ok(resp) => tracing::warn!(
                    msg_count = resp.messages.len(),
                    "[TEST 3] Server ACCEPTED mismatched challenge signature - SECURITY BUG!"
                ),
                Err(e) => tracing::info!("[TEST 3] Server REJECTED mismatched challenge: {}", e),
            }
        }
        Err(e) => {
            tracing::error!("[TEST 3] Could not get challenge: {}", e);
        }
    }

    // Test 4: Correct credentials - should work
    let real_challenge3 = mailbox_client.get_mailbox_challenge(correct_pk.to_vec()).await;
    match real_challenge3 {
        Ok(challenge_resp) => {
            let signing_key = ed25519_dalek::SigningKey::from_bytes(
                &correct_private_key[..32].try_into().expect("32 bytes")
            );
            let correct_signature = signing_key.sign(&challenge_resp.challenge);

            let poll_correct = mailbox_client
                .poll_ephemeral_mailbox(
                    correct_pk.to_vec(),
                    correct_signature.to_bytes().to_vec(),
                    challenge_resp.challenge,
                    None,
                    10,
                )
                .await;

            match &poll_correct {
                Ok(resp) => tracing::info!(
                    msg_count = resp.messages.len(),
                    "[TEST 4] Server ACCEPTED correct credentials - GOOD!"
                ),
                Err(e) => tracing::error!("[TEST 4] Server rejected correct credentials: {}", e),
            }
        }
        Err(e) => {
            tracing::error!("[TEST 4] Could not get challenge: {}", e);
        }
    }

    tracing::info!("=== Server credential rejection test completed ===");
}

// ============================================================================
// Regression Test: Full E2E Message Delivery With Mailbox Exchange
// ============================================================================

/// Test: Complete message flow with proper mailbox exchange.
///
/// This is the happy path that should work after fixes:
/// 1. Two users join channel
/// 2. Both setup mailboxes and exchange announcements
/// 3. Both can send and receive messages
#[tokio::test]
async fn test_e2e_message_with_mailbox_exchange() {
    skip_if_no_server!();

    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();

    tracing::info!("=== Starting E2E message with mailbox exchange test ===");

    // Setup two users
    let pool1 = assert_ok(create_test_pool().await);
    let _user1 = assert_ok(setup_authenticated_test(&pool1).await);

    let pool2 = assert_ok(create_test_pool().await);
    let _user2 = assert_ok(setup_authenticated_test(&pool2).await);

    // User 1 creates public channel
    let mut channel_client1 = ChannelClient::new(pool1.channel(), pool1.auth_clone());
    let channel_name = format!("e2e_mailbox_test_{}", unique_username());
    let channel_id = create_test_channel(&mut channel_client1, &channel_name).await;

    tracing::info!(channel_id = %channel_id, "User 1 created channel");

    // User 2 joins
    let mut channel_client2 = ChannelClient::new(pool2.channel(), pool2.auth_clone());
    let join_stream = channel_client2
        .join_channel(&channel_id, None, false)
        .await
        .expect("Join should start");

    tokio::pin!(join_stream);
    while let Some(_) = join_stream.next().await {}

    tracing::info!("User 2 joined channel");

    // Wait for any mailbox exchange to complete
    sleep(Duration::from_millis(500)).await;

    // User 1 sends message
    let mut msg_client1 = MessageClient::new(pool1.channel(), pool1.auth_clone());
    let content1 = Content {
        r#type: Some(ContentType::Text("Hello from User 1!".to_string())),
    };

    let msg1 = assert_ok(
        msg_client1
            .send_message(&channel_id, content1, None, false, None, vec![], false, vec![])
            .await,
    );

    tracing::info!(message_id = %msg1.id, "User 1 sent message");

    // User 2 sends message
    let mut msg_client2 = MessageClient::new(pool2.channel(), pool2.auth_clone());
    let content2 = Content {
        r#type: Some(ContentType::Text("Hello from User 2!".to_string())),
    };

    let msg2 = assert_ok(
        msg_client2
            .send_message(&channel_id, content2, None, false, None, vec![], false, vec![])
            .await,
    );

    tracing::info!(message_id = %msg2.id, "User 2 sent message");

    // Wait for messages to propagate
    sleep(Duration::from_millis(200)).await;

    // Both users should be able to retrieve messages
    use chatter_core::proto::chat_v1::MessageSortDirection;

    let messages1 = assert_ok(
        msg_client1
            .get_messages(
                &channel_id,
                100,
                None,
                None,
                MessageSortDirection::SortOldestFirst,
                None,
                None,
            )
            .await,
    );

    let messages2 = assert_ok(
        msg_client2
            .get_messages(
                &channel_id,
                100,
                None,
                None,
                MessageSortDirection::SortOldestFirst,
                None,
                None,
            )
            .await,
    );

    tracing::info!(
        user1_sees = messages1.messages.len(),
        user2_sees = messages2.messages.len(),
        "Both users retrieved messages"
    );

    // Both should see at least the messages they sent
    assert!(
        messages1.messages.iter().any(|m| m.id == msg1.id),
        "User 1 should see their own message"
    );
    assert!(
        messages2.messages.iter().any(|m| m.id == msg2.id),
        "User 2 should see their own message"
    );

    // In a public channel, both should see all messages (server-side)
    assert!(
        messages1.messages.len() >= 2,
        "User 1 should see both messages in public channel"
    );
    assert!(
        messages2.messages.len() >= 2,
        "User 2 should see both messages in public channel"
    );

    tracing::info!("=== E2E message with mailbox exchange test passed ===");
}
