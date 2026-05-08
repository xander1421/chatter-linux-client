//! E2E regression tests for the server-managed DM flow.
//!
//! Targets the production bug: friend-accept on the receiver side did NOT
//! fetch the freshly-created DM channel, so the local channels table stayed
//! empty until the next full sync. That broke:
//!   - sidebar (no `DirectMessage` row to render)
//!   - `spawn_server_dm_listener` (no row → no listener → DMs never arrived)
//!   - send-time `conversation_type_for_channel` (no row → defaulted to
//!     `Channel`, while reads queried `ServerDm` → empty conversation)
//!
//! Each test below pins one observable consequence of that bug. See the
//! per-test doc comments for the exact regression each one catches.
//!
//! Mirrors the conventions in `e2e_two_user_suite.rs`: gated on
//! `INTEGRATION_TESTS=1` + a server on `localhost:50051`; `skip_if_no_server!()`
//! macro; isolated tempdir per client; reuses `register_and_start` /
//! `unique_user` / `wait_for_event` style helpers.
//!
//! Run with:
//!   `INTEGRATION_TESTS=1 cargo test -p chatter-core --test e2e_server_dm_flow -- --nocapture`

#[macro_use]
mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use chatter_core::client::ChatterClient;
use chatter_core::domain::channel::ChannelType;
use chatter_core::domain::ids::{ChannelId, UserId};
use chatter_core::domain::message::ConversationType;
use chatter_core::events::DomainEvent;
use chatter_core::state::ActiveConversation;

const SERVER_URL: &str = "http://localhost:50051";
const EVENT_TIMEOUT: Duration = Duration::from_secs(30);
const PIN: &str = "123456";
const PASSWORD: &str = "TestPass123!";

// ============================================================================
// Helpers (mirror e2e_two_user_suite.rs)
// ============================================================================

fn build_client(dir: &tempfile::TempDir) -> Arc<ChatterClient> {
    ChatterClient::builder()
        .server_url(SERVER_URL)
        .data_dir(dir.path().to_str().unwrap())
        .build()
        .expect("Failed to build ChatterClient")
}

fn unique_user(prefix: &str) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let mut rng_bytes = [0u8; 4];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut rng_bytes);
    format!("{}_{:x}_{}", prefix, u32::from_le_bytes(rng_bytes), ts % 100000)
}

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
            Err(_) => {} // single-recv timeout, keep looping
        }
    }
    None
}

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
    result.user_id
}

/// Drive A→B server-friend setup: A sends `send_friend_request`, B accepts
/// via `respond_friend_request`. Returns the dm_channel_id that B got back
/// from the accept response.
///
/// This is the *sender-friend* path (regular, not PrivateFriendCode), which
/// produces a `ChannelType::DirectMessage` server channel.
async fn server_friend_and_get_dm(
    client_a: &Arc<ChatterClient>,
    uid_a: &UserId,
    client_b: &Arc<ChatterClient>,
    uid_b: &UserId,
) -> ChannelId {
    client_a
        .send_friend_request(uid_b, Some("Let's chat!"))
        .await
        .expect("send_friend_request failed");
    tokio::time::sleep(Duration::from_secs(2)).await;

    let resp = client_b
        .respond_friend_request(uid_a, true)
        .await
        .expect("respond_friend_request failed");
    resp.dm_channel_id
        .expect("DM channel id should be set on accept")
        .parse()
        .expect("dm_channel_id should parse as ChannelId")
}

/// Poll a channel-local lookup until it returns Some, with a timeout. Used
/// to wait for the receiver-side auto-sync that lands the DM channel row in
/// the local DB after FriendAccepted.
async fn wait_for_local_channel(
    client: &Arc<ChatterClient>,
    channel_id: &ChannelId,
    timeout: Duration,
) -> Option<chatter_core::domain::channel::Channel> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Some(ch) = client.channels().get_channel_local(channel_id).ok().flatten() {
            return Some(ch);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    None
}

// ============================================================================
// T8: Receiver auto-syncs DM channel on friend-accept
// ============================================================================
//
// Regression: `InboxEvent::FriendAccepted` must call
// `channel_service.get_channel_with_memberships(&ch_id)` before emitting
// the domain event, so the receiver's local channel_repo has the row before
// any sidebar refresh / listener spawn observes it. Before the fix, B's
// channels table was empty post-accept and the DM was invisible until
// re-login.

#[tokio::test]
async fn t8_server_friend_accept_auto_syncs_channel() {
    skip_if_no_server!();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(&dir_a);
    let client_b = build_client(&dir_b);

    let uid_a = register_and_start(&client_a, &unique_user("t8a"), "Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("t8b"), "Bob").await;

    // A→B server-friend handshake.
    let dm_channel_id =
        server_friend_and_get_dm(&client_a, &uid_a, &client_b, &uid_b).await;
    println!("  dm_channel_id: {}", dm_channel_id);

    // Poll for the receiver-side auto-sync. We must NOT call
    // `get_channel_with_memberships` ourselves — the whole point is that
    // the InboxEvent::FriendAccepted handler does it for us.
    //
    // Allow up to 10s: friend-status fanout + gRPC fetch + persist.
    let b_channel = wait_for_local_channel(&client_b, &dm_channel_id, Duration::from_secs(10))
        .await
        .expect("B should auto-sync DM channel after FriendAccepted");

    assert_eq!(
        b_channel.channel_type,
        ChannelType::DirectMessage,
        "B's local channel row should be a DirectMessage"
    );
    println!("  B has DM channel locally (channel_type = DirectMessage)");

    // Also assert membership: both users should be on B's local roster.
    let b_members = client_b.list_channel_members_local(&dm_channel_id);
    let b_has_a = b_members.iter().any(|m| m.user_id == uid_a);
    let b_has_b = b_members.iter().any(|m| m.user_id == uid_b);
    assert!(b_has_a, "B's local membership list should contain A");
    assert!(b_has_b, "B's local membership list should contain B");
    println!("  B's local membership: {} entries (A + B present)", b_members.len());

    // A's side was already fixed before the regression (the sender path
    // gets the channel back from the accept RPC), but assert it for
    // symmetry so a regression on the sender path isn't masked.
    let a_channel = wait_for_local_channel(&client_a, &dm_channel_id, Duration::from_secs(10))
        .await
        .expect("A should also have the DM channel locally");
    assert_eq!(a_channel.channel_type, ChannelType::DirectMessage);
    println!("  A has DM channel locally too");

    println!("T8 PASS — receiver auto-sync on friend-accept verified");

    client_a.shutdown().await;
    client_b.shutdown().await;
}

// ============================================================================
// T9: ServerDm stream listener spawned on friend-accept
// ============================================================================
//
// Regression: the StreamMessages listener for the new DM channel was never
// spawned dynamically when a friendship was accepted *during* a session,
// because the channel row wasn't in B's local DB at the time the inbox
// processor evaluated `get_channel_local`. After the fix, the inbox
// processor fetches the channel first, then spawns the listener. This test
// validates end-to-end: A sends, B receives via the dynamically-spawned
// listener.

#[tokio::test]
async fn t9_server_friend_accept_spawns_stream_listener() {
    skip_if_no_server!();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(&dir_a);
    let client_b = build_client(&dir_b);

    let uid_a = register_and_start(&client_a, &unique_user("t9a"), "Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("t9b"), "Bob").await;

    let dm_channel_id =
        server_friend_and_get_dm(&client_a, &uid_a, &client_b, &uid_b).await;
    println!("  dm_channel_id: {}", dm_channel_id);

    // Wait for B's auto-sync + listener spawn. We don't need to call
    // get_channel_with_memberships; the inbox processor does it.
    wait_for_local_channel(&client_b, &dm_channel_id, Duration::from_secs(10))
        .await
        .expect("B should auto-sync DM channel after FriendAccepted");

    // Sender side: ensure A has the channel row too (always true post-accept,
    // but we still need the local row for outgoing send routing).
    wait_for_local_channel(&client_a, &dm_channel_id, Duration::from_secs(10))
        .await
        .expect("A should have DM channel locally");

    // Subscribe channel-message stream on both sides for completeness;
    // mirrors `t3_direct_message`.
    client_a.subscribe_channel_messages(&dm_channel_id);
    client_b.subscribe_channel_messages(&dm_channel_id);

    // Brief settle period: lets the receiver-side auto-spawned listener
    // attach before the first send.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Subscribe to events BEFORE sending so we don't miss the receive.
    let mut events_b = client_b.subscribe_events();

    let test_message = "T9 — server DM via dynamically-spawned listener";
    client_a
        .send_text(ActiveConversation::Channel(dm_channel_id.clone()), test_message)
        .await
        .expect("A: send_text failed");
    println!("  A sent: {:?}", test_message);

    // B should receive via the listener that the inbox processor spawned at
    // FriendAccepted time. If the listener wasn't spawned, this times out.
    let received = wait_for_event(&mut events_b, EVENT_TIMEOUT, |event| {
        matches!(
            event,
            DomainEvent::MessageReceived { message }
                if message.conversation_id == dm_channel_id
        )
    })
    .await;

    let evt = received.expect("B should receive ServerDm message");
    if let DomainEvent::MessageReceived { message } = &evt {
        assert_eq!(
            message.conversation_type,
            ConversationType::ServerDm,
            "Receiver MUST tag the message as ServerDm (the regression stored it as MailboxDm)"
        );
        assert_eq!(
            message.conversation_id, dm_channel_id,
            "conversation_id should match the DM channel id"
        );
        assert_eq!(
            message.as_text().unwrap_or("<not text>"),
            test_message,
            "message text should round-trip"
        );
        println!(
            "  B received {} on channel {} (conv_type=ServerDm)",
            message.as_text().unwrap_or("<empty>"),
            message.conversation_id,
        );
    }

    println!("T9 PASS — stream listener was spawned and delivered the DM");

    client_a.shutdown().await;
    client_b.shutdown().await;
}

// ============================================================================
// T10: Both sides store messages with ConversationType::ServerDm
// ============================================================================
//
// Regression: the send path used `conversation_type_for_channel(channel_id)`
// against the local channel row. If the row was missing, it defaulted to
// `ConversationType::Channel`, but reads queried `ServerDm`, so messages
// silently disappeared from the UI. With the fix, the channel row is
// guaranteed present on both sides at send/receive time, and both buckets
// agree on `ServerDm`.

#[tokio::test]
async fn t10_server_friend_dm_stored_as_serverdm() {
    skip_if_no_server!();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(&dir_a);
    let client_b = build_client(&dir_b);

    let uid_a = register_and_start(&client_a, &unique_user("t10a"), "Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("t10b"), "Bob").await;

    let dm_channel_id =
        server_friend_and_get_dm(&client_a, &uid_a, &client_b, &uid_b).await;

    // Wait for the receiver's auto-sync of the channel row.
    wait_for_local_channel(&client_b, &dm_channel_id, Duration::from_secs(10))
        .await
        .expect("B should auto-sync DM channel");
    wait_for_local_channel(&client_a, &dm_channel_id, Duration::from_secs(10))
        .await
        .expect("A should have DM channel locally");

    client_a.subscribe_channel_messages(&dm_channel_id);
    client_b.subscribe_channel_messages(&dm_channel_id);
    tokio::time::sleep(Duration::from_secs(2)).await;

    let mut events_b = client_b.subscribe_events();

    let test_message = "T10 — should land in ServerDm bucket on both sides";
    client_a
        .send_text(ActiveConversation::Channel(dm_channel_id.clone()), test_message)
        .await
        .expect("send failed");

    let _ = wait_for_event(&mut events_b, EVENT_TIMEOUT, |event| {
        matches!(
            event,
            DomainEvent::MessageReceived { message }
                if message.conversation_id == dm_channel_id
        )
    })
    .await
    .expect("B should receive the DM");

    // ── Sender side: stored under ServerDm ─────────────────────────────
    let a_server_dm = client_a
        .messages()
        .get_messages_local_asc(&dm_channel_id, &ConversationType::ServerDm, None, 50)
        .expect("A: query ServerDm bucket");
    assert_eq!(
        a_server_dm.len(),
        1,
        "A: ServerDm bucket should have exactly 1 message, got {}",
        a_server_dm.len()
    );
    assert_eq!(
        a_server_dm[0].as_text().unwrap_or("<not text>"),
        test_message,
        "A: stored text should match"
    );

    // Negative: nothing in the MailboxDm bucket — that was the bug.
    let a_mailbox_dm = client_a
        .messages()
        .get_messages_local(&dm_channel_id, &ConversationType::MailboxDm, None, 50)
        .expect("A: query MailboxDm bucket");
    assert!(
        a_mailbox_dm.is_empty(),
        "A: MailboxDm bucket must be empty for a server-friend DM (got {})",
        a_mailbox_dm.len()
    );

    // ── Receiver side: same expectation ────────────────────────────────
    let b_server_dm = client_b
        .messages()
        .get_messages_local_asc(&dm_channel_id, &ConversationType::ServerDm, None, 50)
        .expect("B: query ServerDm bucket");
    assert_eq!(
        b_server_dm.len(),
        1,
        "B: ServerDm bucket should have exactly 1 message, got {}",
        b_server_dm.len()
    );
    assert_eq!(
        b_server_dm[0].as_text().unwrap_or("<not text>"),
        test_message,
        "B: stored text should match"
    );

    let b_mailbox_dm = client_b
        .messages()
        .get_messages_local(&dm_channel_id, &ConversationType::MailboxDm, None, 50)
        .expect("B: query MailboxDm bucket");
    assert!(
        b_mailbox_dm.is_empty(),
        "B: MailboxDm bucket must be empty for a server-friend DM (got {})",
        b_mailbox_dm.len()
    );

    println!("T10 PASS — both sides store ServerDm, MailboxDm bucket clean");

    client_a.shutdown().await;
    client_b.shutdown().await;
}

// ============================================================================
// T11: Channel::conversation_type() returns ServerDm for DirectMessage
// ============================================================================
//
// Regression guard for the mapping in `domain/channel.rs:125`:
//   ChannelType::DirectMessage => ConversationType::ServerDm
// If anyone changes that to AnonGroup/MailboxDm/Channel, every downstream
// query (mark-as-read, load-more, sidebar preview) silently misses rows.

#[tokio::test]
async fn t11_dm_channel_conversation_type_is_server_dm() {
    skip_if_no_server!();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(&dir_a);
    let client_b = build_client(&dir_b);

    let uid_a = register_and_start(&client_a, &unique_user("t11a"), "Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("t11b"), "Bob").await;

    let dm_channel_id =
        server_friend_and_get_dm(&client_a, &uid_a, &client_b, &uid_b).await;

    let a_channel = wait_for_local_channel(&client_a, &dm_channel_id, Duration::from_secs(10))
        .await
        .expect("A should have channel locally");
    let b_channel = wait_for_local_channel(&client_b, &dm_channel_id, Duration::from_secs(10))
        .await
        .expect("B should auto-sync channel locally");

    // Both sides see channel_type = DirectMessage
    assert_eq!(a_channel.channel_type, ChannelType::DirectMessage);
    assert_eq!(b_channel.channel_type, ChannelType::DirectMessage);

    // Both sides map to ServerDm — protect domain/channel.rs:125 mapping
    assert_eq!(
        a_channel.conversation_type(),
        ConversationType::ServerDm,
        "A: DM channel.conversation_type() must be ServerDm"
    );
    assert_eq!(
        b_channel.conversation_type(),
        ConversationType::ServerDm,
        "B: DM channel.conversation_type() must be ServerDm"
    );

    println!("T11 PASS — DM channel maps to ConversationType::ServerDm on both sides");

    client_a.shutdown().await;
    client_b.shutdown().await;
}

// ============================================================================
// T12: PrivateFriendCode DM stays on AnonDm path (not ServerDm)
// ============================================================================
//
// Routing distinction: server-friend (regular) → ServerDm/DirectMessage
// channel; private-friend (NDSS'21 PrivateFriendCode) → AnonDm via the
// mailbox path. This test pins the boundary: starting an anon DM must NOT
// produce a server-managed `DirectMessage` channel and the stored message
// must be in the AnonDm bucket, not ServerDm.

#[tokio::test]
async fn t12_private_friend_dm_uses_anon_dm() {
    skip_if_no_server!();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(&dir_a);
    let client_b = build_client(&dir_b);

    let uid_a = register_and_start(&client_a, &unique_user("t12a"), "Alice").await;
    let _uid_b = register_and_start(&client_b, &unique_user("t12b"), "Bob").await;

    // A publishes a private friend code (NDSS'21).
    let _private_code = client_a
        .users()
        .generate_private_friend_code()
        .expect("generate_private_friend_code failed");

    // B starts an anon DM with A. Mirrors `t5_anonymous_dm`. This routes
    // through the AnonDm path; no server-managed channel is created.
    let anon_dm = client_b
        .start_anon_dm(uid_a.clone(), Some("Anon hello from T12"))
        .await
        .expect("start_anon_dm failed");
    println!("  anon_dm.id: {}", anon_dm.id);

    // Allow the mailbox bootstrap to settle (matches t5).
    tokio::time::sleep(Duration::from_secs(5)).await;

    // ── No DirectMessage channel on either side ────────────────────────
    let a_channels = client_a.list_channels().expect("A: list_channels");
    let a_dms: Vec<_> = a_channels
        .iter()
        .filter(|c| c.channel_type == ChannelType::DirectMessage)
        .collect();
    assert!(
        a_dms.is_empty(),
        "A: PrivateFriendCode flow MUST NOT create a DirectMessage channel (got {})",
        a_dms.len()
    );

    let b_channels = client_b.list_channels().expect("B: list_channels");
    let b_dms: Vec<_> = b_channels
        .iter()
        .filter(|c| c.channel_type == ChannelType::DirectMessage)
        .collect();
    assert!(
        b_dms.is_empty(),
        "B: PrivateFriendCode flow MUST NOT create a DirectMessage channel (got {})",
        b_dms.len()
    );
    println!("  no DirectMessage channels on either side — correct");

    // ── B's sidebar bucket: anon DM appears ────────────────────────────
    let b_anon_dms = client_b.list_anon_dms().unwrap_or_default();
    let b_has_anon = b_anon_dms.iter().any(|d| d.id == anon_dm.id);
    assert!(b_has_anon, "B's list_anon_dms should contain the new conversation");
    println!("  B's anon DM list contains the conversation");

    // ── Stored message bucket (when present) ───────────────────────────
    // The initial-greeting message is enqueued through the anon DM mailbox
    // path; depending on bootstrap timing it may or may not have hit the
    // local store yet. If it has, it MUST be ConversationType::AnonDm and
    // MUST NOT show up in the ServerDm bucket.
    let conv_id: ChannelId = anon_dm.id.into();
    let b_anon = client_b
        .messages()
        .get_messages_local(&conv_id, &ConversationType::AnonDm, None, 50)
        .unwrap_or_default();
    let b_server_dm = client_b
        .messages()
        .get_messages_local(&conv_id, &ConversationType::ServerDm, None, 50)
        .unwrap_or_default();
    assert!(
        b_server_dm.is_empty(),
        "B: anon DM messages MUST NOT land in the ServerDm bucket (got {})",
        b_server_dm.len()
    );
    println!(
        "  B AnonDm bucket: {} messages, ServerDm bucket: 0 (correct)",
        b_anon.len()
    );

    println!("T12 PASS — PrivateFriendCode flow stays on AnonDm path");

    client_a.shutdown().await;
    client_b.shutdown().await;
}

// ============================================================================
// T13: Send-path conv_type matches read-path conv_type
// ============================================================================
//
// Regression: for the briefly-broken state where the sender's local channel
// row was missing at send time (because we hadn't yet auto-fetched), the
// send pipeline stored the row under `Channel` while the read pipeline
// queried `ServerDm`. The two never met; the message was effectively
// invisible. This test pins the post-fix invariant: the
// `conversation_type` on the stored row equals
// `Channel::conversation_type()` for the same channel.

#[tokio::test]
async fn t13_send_path_conv_type_matches_read_path() {
    skip_if_no_server!();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(&dir_a);
    let client_b = build_client(&dir_b);

    let uid_a = register_and_start(&client_a, &unique_user("t13a"), "Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("t13b"), "Bob").await;

    let dm_channel_id =
        server_friend_and_get_dm(&client_a, &uid_a, &client_b, &uid_b).await;

    // Make sure A has the channel row before sending; conversation_type()
    // reads from the local row.
    let a_channel = wait_for_local_channel(&client_a, &dm_channel_id, Duration::from_secs(10))
        .await
        .expect("A should have DM channel locally");
    let expected_conv_type = a_channel.conversation_type();
    assert_eq!(
        expected_conv_type,
        ConversationType::ServerDm,
        "Pre-condition: a DirectMessage channel maps to ServerDm"
    );

    client_a.subscribe_channel_messages(&dm_channel_id);
    client_b.subscribe_channel_messages(&dm_channel_id);

    let send_result = client_a
        .send_text(
            ActiveConversation::Channel(dm_channel_id.clone()),
            "T13 — invariant check",
        )
        .await
        .expect("send_text failed");
    let sent_at_ns = send_result.sent_at;
    println!(
        "  A sent message id={} at_ns={}",
        send_result.message_id_string(),
        sent_at_ns
    );

    // Look up the row by (sender, sent_at) — the storage primary key.
    // Retry briefly to ride out the local-write fence.
    let stored = {
        let start = Instant::now();
        loop {
            if let Some(m) = client_a.get_message_by_sender_timestamp(&uid_a, sent_at_ns) {
                break Some(m);
            }
            if start.elapsed() > Duration::from_secs(5) {
                break None;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    let stored = stored.expect("A: just-sent message should be in local DB");

    // The crux:
    assert_eq!(
        stored.conversation_type, expected_conv_type,
        "Stored conversation_type MUST equal Channel::conversation_type() — \
         this is the invariant that, when violated, made DMs invisible"
    );
    assert_eq!(
        stored.conversation_id, dm_channel_id,
        "Stored row should be in the DM channel"
    );
    assert_eq!(
        stored.sender_id,
        Some(uid_a.clone()),
        "Sender should be A"
    );
    println!(
        "  send-path stored under {:?}, read-path queries {:?} — match",
        stored.conversation_type, expected_conv_type
    );

    // Bonus: prove the read API returns it under that bucket.
    let by_bucket = client_a
        .messages()
        .get_messages_local_asc(&dm_channel_id, &expected_conv_type, None, 50)
        .expect("A: query");
    assert!(
        by_bucket.iter().any(|m| m.id == stored.id),
        "Reading {:?} bucket should surface the stored message",
        expected_conv_type
    );
    println!("T13 PASS — send/read conv_type invariant holds");

    client_a.shutdown().await;
    client_b.shutdown().await;
}
