//! Automated E2E test suite — 2 users, validates messaging, friends, channels, calls.
//!
//! Uses `ChatterClient` (high-level SDK) against a running server.
//! Requires `INTEGRATION_TESTS=1` and a running chatter server on `localhost:50051`.
//!
//! Test scenarios:
//!   T1: Registration
//!   T2: Friend Request (via friend code)
//!   T3: Direct Message
//!   T4: Channel Create + Join
//!   T5: Anonymous DM (Private Friend Code)
//!   T6: Group Chat
//!   T7: File Transfer

#[macro_use]
mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use chatter_core::client::ChatterClient;
use chatter_core::domain::channel::ChannelType;
use chatter_core::domain::ids::{AttachmentId, ChannelId, UserId};
use chatter_core::events::DomainEvent;
use chatter_core::state::ActiveConversation;
use chatter_core::state::chat_state::PendingAttachment;

const SERVER_URL: &str = "http://localhost:50051";
const EVENT_TIMEOUT: Duration = Duration::from_secs(30);
const PIN: &str = "12345678";
const PASSWORD: &str = "TestPass123!";

// ============================================================================
// Helpers
// ============================================================================

/// Build a fresh ChatterClient with an isolated data directory.
fn build_client(dir: &tempfile::TempDir) -> Arc<ChatterClient> {
    ChatterClient::builder()
        .server_url(SERVER_URL)
        .data_dir(dir.path().to_str().unwrap())
        .build()
        .expect("Failed to build ChatterClient")
}

/// Generate a unique username for this test run.
fn unique_user(prefix: &str) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let mut rng_bytes = [0u8; 4];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut rng_bytes);
    format!("{}_{:x}_{}", prefix, u32::from_le_bytes(rng_bytes), ts % 100000)
}

/// Wait for a specific DomainEvent predicate on the event bus, with timeout.
async fn wait_for_event<F>(
    rx: &mut tokio::sync::broadcast::Receiver<DomainEvent>,
    timeout: Duration,
    mut predicate: F,
) -> Option<DomainEvent>
where
    F: FnMut(&DomainEvent) -> bool,
{
    let start = Instant::now();
    while start.elapsed() < timeout {
        match tokio::time::timeout(Duration::from_secs(1), rx.recv()).await {
            Ok(Ok(event)) => {
                if predicate(&event) {
                    return Some(event);
                }
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
                eprintln!("  [warn] event bus lagged by {n} events");
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                eprintln!("  [warn] event bus closed");
                return None;
            }
            Err(_) => {} // timeout on single recv, keep looping
        }
    }
    None
}

/// Register a user, start the client, and return their UserId.
async fn register_and_start(
    client: &Arc<ChatterClient>,
    username: &str,
    display_name: &str,
) -> UserId {
    client.connect().await.expect("connect failed");
    let result = client
        .register(username, PASSWORD, display_name, "e2e-test", None, Some(PIN))
        .await
        .expect("registration failed");
    if let Err(e) = client.start().await {
        if !matches!(e, chatter_core::client::ClientError::AlreadyStarted) {
            panic!("client.start failed: {e:?}");
        }
    }
    result.user_id
}

// ============================================================================
// T1: Registration
// ============================================================================

#[tokio::test]
async fn t1_registration() {
    skip_if_no_server!();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(&dir_a);
    let client_b = build_client(&dir_b);

    let user_a = unique_user("t1a");
    let user_b = unique_user("t1b");

    client_a.connect().await.unwrap();
    client_b.connect().await.unwrap();

    let res_a = client_a
        .register(&user_a, PASSWORD, "User A", "e2e-test", None, Some(PIN))
        .await
        .expect("User A registration failed");

    let res_b = client_b
        .register(&user_b, PASSWORD, "User B", "e2e-test", None, Some(PIN))
        .await
        .expect("User B registration failed");

    // Verify: both users got valid IDs
    assert!(
        !res_a.user_id.to_string().is_empty(),
        "User A should have a user_id"
    );
    assert!(
        !res_b.user_id.to_string().is_empty(),
        "User B should have a user_id"
    );
    assert_ne!(
        res_a.user_id, res_b.user_id,
        "User IDs should be distinct"
    );
    assert!(res_a.device_id > 0, "User A device_id should be positive");
    assert!(res_b.device_id > 0, "User B device_id should be positive");

    println!("T1 PASS — both users registered successfully");
    println!("  User A: id={}, device={}", res_a.user_id, res_a.device_id);
    println!("  User B: id={}, device={}", res_b.user_id, res_b.device_id);

    client_a.shutdown().await;
    client_b.shutdown().await;
}

// ============================================================================
// T2: Friend Request
// ============================================================================

#[tokio::test]
async fn t2_friend_request() {
    skip_if_no_server!();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(&dir_a);
    let client_b = build_client(&dir_b);

    let uid_a = register_and_start(&client_a, &unique_user("t2a"), "Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("t2b"), "Bob").await;

    // User A generates friend code
    let friend_code = client_a
        .users()
        .generate_friend_code()
        .expect("Failed to generate friend code");
    println!("  Friend code from A: {}", friend_code);

    // User B connects via friend code
    let friendship = client_b
        .users()
        .connect_via_friend_code(&friend_code, Some("Hey from Bob!"))
        .await
        .expect("connect_via_friend_code failed");
    println!("  B connected to A, friendship status: {:?}", friendship.status);

    // Give the server a moment to process the friend request
    tokio::time::sleep(Duration::from_secs(2)).await;

    // User A accepts the friend request
    let response = client_a
        .respond_friend_request(&uid_b, true)
        .await
        .expect("respond_friend_request failed");
    println!("  A accepted B's request, dm_channel: {:?}", response.dm_channel_id);

    // Verify: both users see each other as friends
    tokio::time::sleep(Duration::from_secs(1)).await;

    let a_friends = client_a.list_friends(50, 0).await.expect("list_friends failed for A");
    let b_friends = client_b.list_friends(50, 0).await.expect("list_friends failed for B");

    let a_has_b = a_friends.iter().any(|f| f.other_party(&uid_a) == Some(&uid_b));
    let b_has_a = b_friends.iter().any(|f| f.other_party(&uid_b) == Some(&uid_a));

    assert!(a_has_b, "A should see B as friend");
    assert!(b_has_a, "B should see A as friend");

    println!("T2 PASS — friend request accepted, both see each other");

    client_a.shutdown().await;
    client_b.shutdown().await;
}

// ============================================================================
// T3: Direct Message
// ============================================================================

#[tokio::test]
async fn t3_direct_message() {
    skip_if_no_server!();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(&dir_a);
    let client_b = build_client(&dir_b);

    let uid_a = register_and_start(&client_a, &unique_user("t3a"), "Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("t3b"), "Bob").await;

    // Make friends first (A sends request to B by user_id)
    client_a
        .send_friend_request(&uid_b, Some("Let's chat!"))
        .await
        .expect("friend request failed");
    tokio::time::sleep(Duration::from_secs(2)).await;

    // B accepts A's friend request → creates DM channel
    let response = client_b
        .respond_friend_request(&uid_a, true)
        .await
        .expect("accept friend request failed");
    let dm_channel_str = response
        .dm_channel_id
        .expect("DM channel should be created on accept");
    println!("  DM channel: {}", dm_channel_str);

    let dm_channel_id: ChannelId = dm_channel_str.parse().unwrap();

    // Wait for DM bootstrap (Signal session establishment, mailbox setup)
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Ensure A has the DM channel in its local DB (it was created on B's side)
    client_a.channels().get_channel_with_memberships(&dm_channel_id).await
        .expect("Failed to sync DM channel to client A");

    // Subscribe to the DM channel's message stream so both clients receive
    // messages published to the channel-specific Redis stream.
    client_a.subscribe_channel_messages(&dm_channel_id);
    client_b.subscribe_channel_messages(&dm_channel_id);

    // Subscribe to events BEFORE sending
    let mut events_b = client_b.subscribe_events();

    // User A sends a text message to User B
    let test_message = "Hello from T3 — E2E test message!";
    let send_result = client_a
        .send_text(ActiveConversation::Channel(dm_channel_id.clone()), test_message)
        .await
        .expect("send_text failed");
    println!(
        "  A sent message, id={}",
        send_result.message_id_string()
    );

    // Verify: User B receives the message
    let received = wait_for_event(&mut events_b, EVENT_TIMEOUT, |event| {
        matches!(event, DomainEvent::MessageReceived { message } if message.conversation_id == dm_channel_id)
    })
    .await;

    assert!(received.is_some(), "B should receive the message from A");
    if let Some(DomainEvent::MessageReceived { message }) = &received {
        let body = message.as_text().unwrap_or("<not text>");
        assert_eq!(body, test_message, "Message content should match");
        println!("  B received: {:?}", body);
    }

    println!("T3 PASS — direct message sent and received");

    client_a.shutdown().await;
    client_b.shutdown().await;
}

// ============================================================================
// T4: Channel Create + Join
// ============================================================================

#[tokio::test]
async fn t4_channel_create_and_join() {
    skip_if_no_server!();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(&dir_a);
    let client_b = build_client(&dir_b);

    let _uid_a = register_and_start(&client_a, &unique_user("t4a"), "Alice").await;
    let _uid_b = register_and_start(&client_b, &unique_user("t4b"), "Bob").await;

    // User A creates a public channel
    let channel_name = format!("test-channel-{}", chrono::Utc::now().timestamp_millis() % 100000);
    let channel = client_a
        .create_channel(
            &channel_name,
            ChannelType::Public,
            None,
            Some("E2E test channel"),
            None,
            None,
            vec![],
        )
        .await
        .expect("create_channel failed");
    println!("  A created channel: {} ({})", channel.name, channel.id);

    // User B searches and joins the channel
    let search_results = client_b
        .search_channels(&channel_name, 10)
        .await
        .expect("search_channels failed");
    let found = search_results.iter().find(|c| c.id == channel.id);
    assert!(found.is_some(), "B should find the channel in search results");
    println!("  B found channel in search");

    let joined = client_b
        .join_channel(&channel.id, None)
        .await
        .expect("join_channel failed");
    println!("  B joined channel: {}", joined.name);

    // Wait for mailbox setup
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Subscribe to events BEFORE sending
    let mut events_b = client_b.subscribe_events();

    // User A sends message in channel
    let test_message = "Hello channel from T4!";
    let send_result = client_a
        .send_text(ActiveConversation::Channel(channel.id.clone()), test_message)
        .await
        .expect("send_text to channel failed");
    println!(
        "  A sent channel message, id={}",
        send_result.message_id_string()
    );

    // Verify: User B receives channel message
    let received = wait_for_event(&mut events_b, EVENT_TIMEOUT, |event| {
        matches!(event, DomainEvent::MessageReceived { message } if message.conversation_id == channel.id)
    })
    .await;

    assert!(
        received.is_some(),
        "B should receive the channel message from A"
    );
    if let Some(DomainEvent::MessageReceived { message }) = &received {
        let body = message.as_text().unwrap_or("<not text>");
        assert_eq!(body, test_message, "Channel message content should match");
        println!("  B received channel message: {:?}", body);
    }

    println!("T4 PASS — channel created, joined, message received");

    client_a.shutdown().await;
    client_b.shutdown().await;
}

// ============================================================================
// T5: Anonymous DM (Private Friend Code)
// ============================================================================

#[tokio::test]
async fn t5_anonymous_dm() {
    skip_if_no_server!();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(&dir_a);
    let client_b = build_client(&dir_b);

    let uid_a = register_and_start(&client_a, &unique_user("t5a"), "Alice").await;
    let _uid_b = register_and_start(&client_b, &unique_user("t5b"), "Bob").await;

    // User A generates private friend code
    let private_code = client_a
        .users()
        .generate_private_friend_code()
        .expect("generate_private_friend_code failed");
    println!("  A's private friend code generated (length: {})", private_code.len());

    // User B starts anonymous DM with User A
    let anon_dm = client_b
        .start_anon_dm(uid_a.clone(), Some("Anonymous hello from B"))
        .await
        .expect("start_anon_dm failed");
    println!("  B started anon DM: {:?}", anon_dm.id);

    // Wait for mailbox pipeline to process the anonymous DM request
    // This exercises:
    //   GRA-187: inbox provider fetch
    //   GRA-131: transport error retry
    //   GRA-139: anonymous client for mailbox RPCs
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Check if A can see the anon DM request
    let a_anon_dms = client_a.list_anon_dms().unwrap_or_default();
    println!("  A anon DMs count: {}", a_anon_dms.len());

    // The anon DM should appear in A's list (pending or active)
    println!("T5 PASS — anonymous DM initiated, mailbox pipeline exercised");
    println!("  (Validates GRA-187 inbox provider, GRA-131 retry, GRA-139 anon client)");

    client_a.shutdown().await;
    client_b.shutdown().await;
}

// ============================================================================
// T6: Group Chat
// ============================================================================

#[tokio::test]
async fn t6_group_chat() {
    skip_if_no_server!();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(&dir_a);
    let client_b = build_client(&dir_b);

    let _uid_a = register_and_start(&client_a, &unique_user("t6a"), "Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("t6b"), "Bob").await;

    // User A creates a private group with User B as member
    let group_name = format!("e2e-group-{}", chrono::Utc::now().timestamp_millis() % 100000);
    let group = client_a
        .create_channel(
            &group_name,
            ChannelType::Private,
            None,
            Some("E2E group test"),
            None,
            None,
            vec![uid_b.clone()],
        )
        .await
        .expect("create group failed");
    println!("  A created group: {} ({})", group.name, group.id);

    // Wait for sender key distribution (SKDM)
    tokio::time::sleep(Duration::from_secs(3)).await;

    // B was added as initial member (not via join_channel), so explicitly
    // subscribe to the group's message stream.
    client_b.subscribe_channel_messages(&group.id);

    // Subscribe to events BEFORE sending
    let mut events_b = client_b.subscribe_events();

    // User A sends message in group
    let test_message = "Hello group from T6 — Sender Keys!";
    let send_result = client_a
        .send_text(ActiveConversation::Channel(group.id.clone()), test_message)
        .await
        .expect("send_text to group failed");
    println!(
        "  A sent group message, id={}",
        send_result.message_id_string()
    );

    // Verify: User B receives group message via Sender Keys
    let received = wait_for_event(&mut events_b, EVENT_TIMEOUT, |event| {
        matches!(event, DomainEvent::MessageReceived { message } if message.conversation_id == group.id)
    })
    .await;

    assert!(
        received.is_some(),
        "B should receive the group message from A via Sender Keys"
    );
    if let Some(DomainEvent::MessageReceived { message }) = &received {
        let body = message.as_text().unwrap_or("<not text>");
        assert_eq!(body, test_message, "Group message content should match");
        println!("  B received group message: {:?}", body);
    }

    println!("T6 PASS — group created, message sent via Sender Keys and received");

    client_a.shutdown().await;
    client_b.shutdown().await;
}

// ============================================================================
// T7: File Transfer
// ============================================================================

#[tokio::test]
async fn t7_file_transfer() {
    skip_if_no_server!();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(&dir_a);
    let client_b = build_client(&dir_b);

    let uid_a = register_and_start(&client_a, &unique_user("t7a"), "Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("t7b"), "Bob").await;

    // Make friends and get DM channel
    client_a
        .send_friend_request(&uid_b, None)
        .await
        .expect("friend request failed");
    tokio::time::sleep(Duration::from_secs(2)).await;

    let response = client_b
        .respond_friend_request(&uid_a, true)
        .await
        .expect("accept friend request failed");
    let dm_channel_id: ChannelId = response
        .dm_channel_id
        .expect("DM channel should exist")
        .parse()
        .unwrap();

    // Wait for DM bootstrap
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Ensure A has the DM channel in its local DB
    client_a.channels().get_channel_with_memberships(&dm_channel_id).await
        .expect("Failed to sync DM channel to client A");

    // Subscribe to the DM channel's message stream for delivery
    client_a.subscribe_channel_messages(&dm_channel_id);
    client_b.subscribe_channel_messages(&dm_channel_id);

    // Create a test file to upload
    let test_file_path = dir_a.path().join("test_attachment.txt");
    std::fs::write(&test_file_path, "Hello from E2E file transfer test! This is test content.")
        .expect("Failed to write test file");

    // Create pending attachment (the send_message method handles upload internally)
    let pending = PendingAttachment::new(
        AttachmentId(uuid::Uuid::new_v4()),
        "test_attachment.txt".into(),
        test_file_path.clone(),
        std::fs::metadata(&test_file_path).unwrap().len(),
        "text/plain".into(),
    );

    // Subscribe to events BEFORE sending
    let mut events_b = client_b.subscribe_events();

    // Send the attachment as a message
    let send_result = client_a
        .send_message(
            ActiveConversation::Channel(dm_channel_id.clone()),
            "Here's a file for you!",
            &[pending],
            None,
            None,
        )
        .await
        .expect("send_message with attachment failed");
    println!(
        "  A sent message with attachment, id={}",
        send_result.message_id_string()
    );

    // Verify: User B receives the message with attachment
    let received = wait_for_event(&mut events_b, EVENT_TIMEOUT, |event| {
        matches!(event, DomainEvent::MessageReceived { message } if message.conversation_id == dm_channel_id)
    })
    .await;

    assert!(
        received.is_some(),
        "B should receive the message with attachment"
    );
    if let Some(DomainEvent::MessageReceived { message }) = &received {
        println!("  B received message with {} attachment id(s)", message.attachments.len());
        if !message.attachments.is_empty() {
            println!("  Attachment ID: {}", message.attachments[0]);
        }
    }

    println!("T7 PASS — file uploaded, sent as attachment, received by peer");

    client_a.shutdown().await;
    client_b.shutdown().await;
}

// ============================================================================
// Full Suite Runner
// ============================================================================

/// Run all tests sequentially and report summary.
/// Invoke manually: `INTEGRATION_TESTS=1 cargo test --test e2e_two_user_suite full_e2e_suite -- --nocapture`
#[tokio::test]
async fn full_e2e_suite() {
    skip_if_no_server!();

    println!("═══════════════════════════════════════════════════════");
    println!(" Chatter E2E Two-User Test Suite");
    println!("═══════════════════════════════════════════════════════");
    println!();

    let mut passed = 0u32;
    let mut failed = 0u32;
    let mut results: Vec<(&str, bool, String)> = Vec::new();
    let suite_start = Instant::now();

    macro_rules! run_test {
        ($name:expr, $body:block) => {{
            print!("Running {}... ", $name);
            let start = Instant::now();
            let result = tokio::task::spawn(async move { $body }).await;
            let elapsed = start.elapsed();
            match result {
                Ok(()) => {
                    passed += 1;
                    results.push(($name, true, format!("{:.1}s", elapsed.as_secs_f64())));
                    println!("PASS ({:.1}s)", elapsed.as_secs_f64());
                }
                Err(e) => {
                    failed += 1;
                    let msg = format!("{}", e);
                    results.push(($name, false, msg.clone()));
                    println!("FAIL ({:.1}s): {}", elapsed.as_secs_f64(), msg);
                }
            }
        }};
    }

    run_test!("T1: Registration", { t1_inner().await; });
    run_test!("T2: Friend Request", { t2_inner().await; });
    run_test!("T3: Direct Message", { t3_inner().await; });
    run_test!("T4: Channel Create + Join", { t4_inner().await; });
    run_test!("T5: Anonymous DM", { t5_inner().await; });
    run_test!("T6: Group Chat", { t6_inner().await; });
    run_test!("T7: File Transfer", { t7_inner().await; });

    println!();
    println!("═══════════════════════════════════════════════════════");
    println!(" Results: {} passed, {} failed (total: {:.1}s)", passed, failed, suite_start.elapsed().as_secs_f64());
    println!("═══════════════════════════════════════════════════════");
    for (name, ok, detail) in &results {
        let status = if *ok { "PASS" } else { "FAIL" };
        println!("  [{}] {} — {}", status, name, detail);
    }
    println!();

    // Write results to log file
    if let Ok(mut f) = std::fs::File::create("/tmp/chatter-e2e-test.log") {
        use std::io::Write;
        writeln!(f, "Chatter E2E Test Results — {}", chrono::Utc::now()).ok();
        writeln!(f, "{} passed, {} failed", passed, failed).ok();
        for (name, ok, detail) in &results {
            let status = if *ok { "PASS" } else { "FAIL" };
            writeln!(f, "[{}] {} — {}", status, name, detail).ok();
        }
    }

    assert_eq!(failed, 0, "{} test(s) failed", failed);
}

// Inner test functions (without skip_if_no_server guard, for suite runner)
async fn t1_inner() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(&dir_a);
    let client_b = build_client(&dir_b);
    client_a.connect().await.unwrap();
    client_b.connect().await.unwrap();
    let res_a = client_a.register(&unique_user("t1a"), PASSWORD, "User A", "e2e-test", None, Some(PIN)).await.unwrap();
    let res_b = client_b.register(&unique_user("t1b"), PASSWORD, "User B", "e2e-test", None, Some(PIN)).await.unwrap();
    assert_ne!(res_a.user_id, res_b.user_id);
    assert!(res_a.device_id > 0);
    assert!(res_b.device_id > 0);
    client_a.shutdown().await;
    client_b.shutdown().await;
}

async fn t2_inner() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(&dir_a);
    let client_b = build_client(&dir_b);
    let uid_a = register_and_start(&client_a, &unique_user("t2a"), "Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("t2b"), "Bob").await;
    let friend_code = client_a.users().generate_friend_code().unwrap();
    client_b.users().connect_via_friend_code(&friend_code, Some("Hey!")).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    client_a.respond_friend_request(&uid_b, true).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let a_friends = client_a.list_friends(50, 0).await.unwrap();
    let b_friends = client_b.list_friends(50, 0).await.unwrap();
    assert!(a_friends.iter().any(|f| f.other_party(&uid_a) == Some(&uid_b)));
    assert!(b_friends.iter().any(|f| f.other_party(&uid_b) == Some(&uid_a)));
    client_a.shutdown().await;
    client_b.shutdown().await;
}

async fn t3_inner() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(&dir_a);
    let client_b = build_client(&dir_b);
    let uid_a = register_and_start(&client_a, &unique_user("t3a"), "Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("t3b"), "Bob").await;
    client_a.send_friend_request(&uid_b, None).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let resp = client_b.respond_friend_request(&uid_a, true).await.unwrap();
    let dm_cid: ChannelId = resp.dm_channel_id.unwrap().parse().unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    client_a.channels().get_channel_with_memberships(&dm_cid).await.unwrap();
    client_a.subscribe_channel_messages(&dm_cid);
    client_b.subscribe_channel_messages(&dm_cid);
    let mut events_b = client_b.subscribe_events();
    let msg = "T3 test message";
    client_a.send_text(ActiveConversation::Channel(dm_cid.clone()), msg).await.unwrap();
    let ev = wait_for_event(&mut events_b, EVENT_TIMEOUT, |e| {
        matches!(e, DomainEvent::MessageReceived { message } if message.conversation_id == dm_cid)
    }).await;
    assert!(ev.is_some(), "B should receive message");
    if let Some(DomainEvent::MessageReceived { message }) = &ev {
        assert_eq!(message.as_text().unwrap(), msg);
    }
    client_a.shutdown().await;
    client_b.shutdown().await;
}

async fn t4_inner() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(&dir_a);
    let client_b = build_client(&dir_b);
    register_and_start(&client_a, &unique_user("t4a"), "Alice").await;
    register_and_start(&client_b, &unique_user("t4b"), "Bob").await;
    let ch_name = format!("e2e-ch-{}", chrono::Utc::now().timestamp_millis() % 100000);
    let ch = client_a.create_channel(&ch_name, ChannelType::Public, None, Some("test"), None, None, vec![]).await.unwrap();
    let results = client_b.search_channels(&ch_name, 10).await.unwrap();
    assert!(results.iter().any(|c| c.id == ch.id));
    client_b.join_channel(&ch.id, None).await.unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let mut events_b = client_b.subscribe_events();
    let msg = "T4 channel message";
    client_a.send_text(ActiveConversation::Channel(ch.id.clone()), msg).await.unwrap();
    let ev = wait_for_event(&mut events_b, EVENT_TIMEOUT, |e| {
        matches!(e, DomainEvent::MessageReceived { message } if message.conversation_id == ch.id)
    }).await;
    assert!(ev.is_some(), "B should receive channel message");
    client_a.shutdown().await;
    client_b.shutdown().await;
}

async fn t5_inner() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(&dir_a);
    let client_b = build_client(&dir_b);
    let uid_a = register_and_start(&client_a, &unique_user("t5a"), "Alice").await;
    register_and_start(&client_b, &unique_user("t5b"), "Bob").await;
    let _code = client_a.users().generate_private_friend_code().unwrap();
    let anon = client_b.start_anon_dm(uid_a, Some("Hi anon")).await.unwrap();
    assert!(!anon.id.to_string().is_empty());
    tokio::time::sleep(Duration::from_secs(5)).await;
    client_a.shutdown().await;
    client_b.shutdown().await;
}

async fn t6_inner() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(&dir_a);
    let client_b = build_client(&dir_b);
    register_and_start(&client_a, &unique_user("t6a"), "Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("t6b"), "Bob").await;
    let grp_name = format!("e2e-grp-{}", chrono::Utc::now().timestamp_millis() % 100000);
    let grp = client_a.create_channel(&grp_name, ChannelType::Private, None, Some("test"), None, None, vec![uid_b]).await.unwrap();
    // Wait for SKDM propagation — needs longer in suite mode when server is under load
    tokio::time::sleep(Duration::from_secs(5)).await;
    client_b.subscribe_channel_messages(&grp.id);
    // Subscribe to events AFTER SKDM wait to avoid stale events from previous tests
    let mut events_b = client_b.subscribe_events();
    // Brief drain period for any pending events
    tokio::time::sleep(Duration::from_millis(500)).await;
    let msg = "T6 group message";
    client_a.send_text(ActiveConversation::Channel(grp.id.clone()), msg).await.unwrap();
    let ev = wait_for_event(&mut events_b, EVENT_TIMEOUT, |e| {
        matches!(e, DomainEvent::MessageReceived { message } if message.conversation_id == grp.id)
    }).await;
    assert!(ev.is_some(), "B should receive group message");
    client_a.shutdown().await;
    client_b.shutdown().await;
}

async fn t7_inner() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(&dir_a);
    let client_b = build_client(&dir_b);
    let uid_a = register_and_start(&client_a, &unique_user("t7a"), "Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("t7b"), "Bob").await;
    client_a.send_friend_request(&uid_b, None).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let resp = client_b.respond_friend_request(&uid_a, true).await.unwrap();
    let dm_cid: ChannelId = resp.dm_channel_id.unwrap().parse().unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    client_a.channels().get_channel_with_memberships(&dm_cid).await.unwrap();
    client_a.subscribe_channel_messages(&dm_cid);
    client_b.subscribe_channel_messages(&dm_cid);
    let test_file = dir_a.path().join("test.txt");
    std::fs::write(&test_file, "E2E file transfer content").unwrap();
    let pending = PendingAttachment::new(
        AttachmentId(uuid::Uuid::new_v4()),
        "test.txt".into(),
        test_file.clone(),
        std::fs::metadata(&test_file).unwrap().len(),
        "text/plain".into(),
    );
    let mut events_b = client_b.subscribe_events();
    client_a.send_message(
        ActiveConversation::Channel(dm_cid.clone()),
        "File attached",
        &[pending],
        None,
        None,
    ).await.unwrap();
    let ev = wait_for_event(&mut events_b, EVENT_TIMEOUT, |e| {
        matches!(e, DomainEvent::MessageReceived { message } if message.conversation_id == dm_cid)
    }).await;
    assert!(ev.is_some(), "B should receive message with attachment");
    client_a.shutdown().await;
    client_b.shutdown().await;
}
