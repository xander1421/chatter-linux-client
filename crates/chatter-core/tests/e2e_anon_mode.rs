//! E2E regression tests for the app-wide "Anon Mode" toggle.
//!
//! Anon Mode is a pure UI/presentation flag stored in an in-memory
//! `AtomicBool` on `ChatterClient`. When enabled, the UI hides server-friend
//! DMs, public/private channels, and the Friends/Saved entry points; only
//! anon DMs and anon groups remain visible. The flag is NOT persisted —
//! every fresh client starts with `anon_mode = false` so privacy properties
//! auto-revert on restart.
//!
//! These tests pin the SDK contract:
//!   - default state is off
//!   - toggle is read-write, side-effect free at the SDK layer
//!   - `verify_pin_for_anon_mode` is read-only (does NOT flip the flag)
//!   - PIN gate behaves correctly for right/wrong/empty/short inputs
//!   - enabling anon mode does NOT affect message storage or transport on
//!     server-managed DMs (it's a UI filter, nothing more)
//!
//! Mirrors the conventions in `e2e_server_dm_flow.rs`: gated on
//! `INTEGRATION_TESTS=1` + a server on `localhost:50051`; `skip_if_no_server!()`
//! macro; isolated tempdir per client.
//!
//! Run with (the server has no plaintext mode -- point CHATTER_TLS_ROOT_CA_PEM
//! at its cert, e.g. `certs/server.crt` from `./create-certs.sh`):
//!   `CHATTER_TLS_ROOT_CA_PEM="$(cat certs/server.crt)" INTEGRATION_TESTS=1 \
//!        cargo test -p chatter-core --features testing \
//!        --test e2e_anon_mode -- --nocapture --test-threads=1`

use crate::{common, skip_if_no_server};

use std::sync::Arc;
use std::time::{Duration, Instant};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::{ChannelId, UserId};
use chatter_core::domain::message::ConversationType;
use chatter_core::events::DomainEvent;
use chatter_core::state::ActiveConversation;

const SERVER_URL: &str = "https://localhost:50051";
const EVENT_TIMEOUT: Duration = Duration::from_secs(30);
// NOTE: The task spec mentions a PIN of `"123456"` (matching the constant
// in `e2e_server_dm_flow.rs`). However, `crate::crypto::svr::MIN_PIN_LENGTH`
// is 8 and `validate_pin` rejects shorter PINs at registration time
// (`Validation("PIN must be at least 8 characters")`). The existing
// `e2e_server_dm_flow.rs` is currently broken by the same constraint —
// since we are restricted from touching production code or other tests, we
// use an 8-char PIN here and document the divergence. The semantic
// behaviour under test is identical: a known-correct PIN registered with
// the user, plus a set of mismatching/empty/short PINs that must squash
// to `false` through `verify_pin_for_anon_mode`.
const PIN: &str = "Kf8mq2xpZ"; // non-blocklisted, clears validate_pin
const PASSWORD: &str = "TestPass123!";

// ============================================================================
// Helpers (mirror e2e_server_dm_flow.rs — kept self-contained per Rust
// integration-test conventions; no module sharing across tests/*.rs files).
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
    if let Err(e) = client.start().await {
        if !matches!(e, chatter_core::client::ClientError::AlreadyStarted) {
            panic!("client.start failed: {e:?}");
        }
    }
    result.user_id
}

/// Drive A→B server-friend setup: A sends `send_friend_request`, B accepts
/// via `respond_friend_request`. DMs are channel-free (Signal-style), so
/// there is no DM channel to return — the conversation is keyed by peer
/// user_id (`ChannelId::from(peer)`).
async fn server_friend(
    client_a: &Arc<ChatterClient>,
    uid_b: &UserId,
    client_b: &Arc<ChatterClient>,
    uid_a: &UserId,
) {
    client_a
        .send_friend_request(uid_b, Some("Let's chat!"))
        .await
        .expect("send_friend_request failed");
    tokio::time::sleep(Duration::from_secs(2)).await;

    client_b
        .respond_friend_request(uid_a, true)
        .await
        .expect("respond_friend_request failed");
}

// ============================================================================
// T14: Default state — anon mode off after register
// ============================================================================
//
// Pins the contract: every fresh client starts with `anon_mode = false`.
// Critical because the flag is non-persisted by design — restart MUST land
// in non-anon mode so privacy properties auto-revert.

#[tokio::test]
async fn t14_default_anon_mode_off_after_register() {
    skip_if_no_server!();

    let dir = tempfile::tempdir().unwrap();
    let client = build_client(&dir);

    let _uid = register_and_start(&client, &unique_user("t14"), "Alice").await;
    println!("  client registered + started");

    assert!(
        !client.anon_mode_enabled(),
        "Fresh client MUST default to anon_mode_enabled() == false"
    );
    println!("T14 PASS — default anon_mode_enabled() == false");

    client.shutdown().await;
}

// ============================================================================
// T15: Set anon mode on, then off
// ============================================================================
//
// Round-trip the in-memory flag. SDK contract: set/get pair is symmetric
// and observable in-thread without any other API calls.

#[tokio::test]
async fn t15_set_anon_mode_on_then_off() {
    skip_if_no_server!();

    let dir = tempfile::tempdir().unwrap();
    let client = build_client(&dir);

    let _uid = register_and_start(&client, &unique_user("t15"), "Alice").await;
    assert!(!client.anon_mode_enabled(), "baseline: off");

    client.set_anon_mode_enabled(true);
    assert!(
        client.anon_mode_enabled(),
        "after set_anon_mode_enabled(true), enabled() must be true"
    );
    println!("  anon_mode flipped on");

    client.set_anon_mode_enabled(false);
    assert!(
        !client.anon_mode_enabled(),
        "after set_anon_mode_enabled(false), enabled() must be false"
    );
    println!("  anon_mode flipped off");

    println!("T15 PASS — on/off round-trip works");

    client.shutdown().await;
}

// ============================================================================
// T16: Verify PIN with correct PIN succeeds
// ============================================================================
//
// `verify_pin_for_anon_mode` wraps `LinkDeviceService::verify_pin` and is
// the gate the UI uses before flipping anon mode ON. With the registration
// PIN, it must return true.

#[tokio::test]
async fn t16_verify_pin_correct_succeeds() {
    skip_if_no_server!();

    let dir = tempfile::tempdir().unwrap();
    let client = build_client(&dir);

    let _uid = register_and_start(&client, &unique_user("t16"), "Alice").await;
    println!("  registered with PIN={PIN}");

    assert!(
        client.verify_pin_for_anon_mode(PIN),
        "verify_pin_for_anon_mode(\"{PIN}\") must succeed for the registration PIN"
    );
    println!("T16 PASS — correct PIN verifies");

    client.shutdown().await;
}

// ============================================================================
// T17: Verify PIN with wrong / empty / short PIN fails
// ============================================================================
//
// All non-matching PIN inputs must squash to `false` — the SDK collapses
// "wrong PIN", "no PIN set", "too-short PIN", "storage error" into a single
// negative result so the UI doesn't leak distinctions it shouldn't surface.

#[tokio::test]
async fn t17_verify_pin_wrong_fails() {
    skip_if_no_server!();

    let dir = tempfile::tempdir().unwrap();
    let client = build_client(&dir);

    let _uid = register_and_start(&client, &unique_user("t17"), "Alice").await;

    assert!(
        !client.verify_pin_for_anon_mode("000000"),
        "wrong PIN must return false"
    );
    println!("  wrong PIN '000000' -> false");

    assert!(
        !client.verify_pin_for_anon_mode(""),
        "empty PIN must return false"
    );
    println!("  empty PIN -> false");

    assert!(
        !client.verify_pin_for_anon_mode("123"),
        "too-short PIN must return false (squashes validate_pin error)"
    );
    println!("  short PIN '123' -> false");

    println!("T17 PASS — all non-matching PIN inputs squash to false");

    client.shutdown().await;
}

// ============================================================================
// T18: Anon mode does NOT affect message delivery
// ============================================================================
//
// The crucial integration test: anon mode is purely visual; enabling it on
// the sender side must NOT change storage layout or stream transport for
// server-managed DMs. Mirrors `t9_server_friend_accept_spawns_stream_listener`
// for the wire-up; layers `set_anon_mode_enabled(true)` on top of A before
// the send.

#[tokio::test]
async fn t18_anon_mode_does_not_affect_message_delivery() {
    skip_if_no_server!();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let client_a = build_client(&dir_a);
    let client_b = build_client(&dir_b);

    let uid_a = register_and_start(&client_a, &unique_user("t18a"), "Alice").await;
    let uid_b = register_and_start(&client_b, &unique_user("t18b"), "Bob").await;

    server_friend(&client_a, &uid_b, &client_b, &uid_a).await;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Channel-free DM conversation keys: A stores under the recipient
    // (B), B stores under the sender (A).
    let a_conv_id = ChannelId::from(uid_b);
    let b_conv_id = ChannelId::from(uid_a);

    // ── Anon mode flip on A — the test point ───────────────────────────
    client_a.set_anon_mode_enabled(true);
    assert!(
        client_a.anon_mode_enabled(),
        "A: anon_mode must be on for this test"
    );
    assert!(
        !client_b.anon_mode_enabled(),
        "B: anon_mode must remain OFF (default)"
    );
    println!("  A enabled anon mode; B remains in normal mode");

    // Subscribe the event bus on B before sending.
    let mut events_b = client_b.subscribe_events();

    let test_message = "T18 — sender in anon mode, receiver normal";
    client_a
        .send_text(ActiveConversation::ServerDm { peer_id: uid_b }, test_message)
        .await
        .expect("A: send_text failed (anon mode must NOT block sends)");
    println!("  A sent: {:?}", test_message);

    // B must still receive — anon mode is a UI filter, not a transport
    // concern. B buckets by the sender's user_id (A).
    let received = wait_for_event(&mut events_b, EVENT_TIMEOUT, |event| {
        matches!(
            event,
            DomainEvent::MessageReceived { message }
                if message.conversation_id == b_conv_id
        )
    })
    .await;
    let evt = received.expect("B should receive MessageReceived even when A is in anon mode");
    if let DomainEvent::MessageReceived { message } = &evt {
        assert_eq!(
            message.as_text().unwrap_or("<not text>"),
            test_message,
            "delivered text must round-trip through the stream regardless of A's anon mode"
        );
        println!("  B received text via stream listener");
    }

    // ── Sender storage: the message MUST be in the local store. Anon
    //    mode does NOT drop persistence; it's UI-side only. ─────────────
    let a_stored = client_a
        .get_messages_local_asc(&a_conv_id, &ConversationType::ServerDm, None, 50)
        .expect("A: query ServerDm bucket");
    assert_eq!(
        a_stored.len(),
        1,
        "A: anon-mode sender must still persist outgoing message (got {})",
        a_stored.len()
    );
    assert_eq!(
        a_stored[0].as_text().unwrap_or("<not text>"),
        test_message,
        "A: stored text must match the sent text"
    );
    println!("  A storage: 1 message in ServerDm bucket (anon mode does NOT drop persistence)");

    // ── Receiver storage: B in normal mode also stores ────────────────
    let b_stored = client_b
        .get_messages_local_asc(&b_conv_id, &ConversationType::ServerDm, None, 50)
        .expect("B: query ServerDm bucket");
    assert_eq!(
        b_stored.len(),
        1,
        "B: normal-mode receiver must store the inbound message (got {})",
        b_stored.len()
    );
    assert_eq!(
        b_stored[0].as_text().unwrap_or("<not text>"),
        test_message,
        "B: stored text must match"
    );
    println!("  B storage: 1 message in ServerDm bucket");

    // A's anon_mode flag must be unchanged by the send (no side effect on
    // the toggle from the message pipeline).
    assert!(
        client_a.anon_mode_enabled(),
        "A: anon_mode flag must remain true after the send"
    );

    println!("T18 PASS — anon mode is presentation-only; transport + storage unaffected");

    client_a.shutdown().await;
    client_b.shutdown().await;
}

// ============================================================================
// T19: Toggle off doesn't require PIN
// ============================================================================
//
// SDK contract: `set_anon_mode_enabled(false)` is NOT gated by
// `verify_pin_for_anon_mode`. The PIN gate lives in the UI handler for the
// ON path. Disabling anon mode reduces the privacy posture, so the UI may
// gate it differently (or not at all); from the SDK's view, both directions
// of the toggle are unconditional.

#[tokio::test]
async fn t19_toggle_off_doesnt_require_pin() {
    skip_if_no_server!();

    let dir = tempfile::tempdir().unwrap();
    let client = build_client(&dir);

    let _uid = register_and_start(&client, &unique_user("t19"), "Alice").await;

    // Flip on (no PIN at SDK level — UI is responsible for gating).
    client.set_anon_mode_enabled(true);
    assert!(client.anon_mode_enabled(), "set(true) must take effect");
    println!("  anon_mode set on without any PIN call");

    // Flip off — also no PIN call.
    client.set_anon_mode_enabled(false);
    assert!(
        !client.anon_mode_enabled(),
        "set(false) must take effect without PIN gating at the SDK"
    );
    println!("  anon_mode set off without any PIN call");

    println!("T19 PASS — both toggle directions are ungated at the SDK layer");

    client.shutdown().await;
}

// ============================================================================
// T20: Wrong PIN does NOT silently flip the flag
// ============================================================================
//
// `verify_pin_for_anon_mode` is read-only. A failing PIN check must NOT
// have any side effect on the `anon_mode` flag. This catches the class of
// bug where a PIN-verification helper accidentally writes the flag in a
// success branch but leaves a stale write or partial update in the failure
// path.

#[tokio::test]
async fn t20_wrong_pin_does_not_silently_flip_flag() {
    skip_if_no_server!();

    let dir = tempfile::tempdir().unwrap();
    let client = build_client(&dir);

    let _uid = register_and_start(&client, &unique_user("t20"), "Alice").await;

    assert!(!client.anon_mode_enabled(), "baseline: anon_mode off");

    let ok = client.verify_pin_for_anon_mode("999999");
    assert!(!ok, "verify_pin_for_anon_mode(\"999999\") must return false");
    println!("  wrong PIN -> false (as expected)");

    // The crux: a failed verify MUST NOT have side effects on the flag.
    assert!(
        !client.anon_mode_enabled(),
        "wrong PIN check must NOT silently flip anon_mode on"
    );
    println!("  anon_mode still off after wrong PIN check (no side effect)");

    // Sanity: the toggle still works regardless of any PIN-state
    // residue. Even the correct PIN is not required at the SDK level —
    // the UI layer adds the gate.
    client.set_anon_mode_enabled(true);
    assert!(
        client.anon_mode_enabled(),
        "set_anon_mode_enabled(true) must work regardless of prior PIN-check state"
    );
    println!("  set_anon_mode_enabled(true) works after a failed PIN check");

    println!("T20 PASS — verify_pin is read-only; no side effects on the flag");

    client.shutdown().await;
}

// ============================================================================
// T21: Repeated correct PIN check is allowed (no SDK-level rate limit)
// ============================================================================
//
// PIN re-verification is cheap and not rate-limited at the SDK layer. The
// UI may add throttling later; that's a UX concern, not an SDK concern.

#[tokio::test]
async fn t21_repeated_correct_pin_check_allowed() {
    skip_if_no_server!();

    let dir = tempfile::tempdir().unwrap();
    let client = build_client(&dir);

    let _uid = register_and_start(&client, &unique_user("t21"), "Alice").await;

    for i in 1..=4 {
        assert!(
            client.verify_pin_for_anon_mode(PIN),
            "iteration {i}: correct PIN must verify successfully"
        );
        println!("  iteration {i}/4: PIN verified");
    }

    println!("T21 PASS — repeated correct PIN checks all succeed (no SDK rate limit)");

    client.shutdown().await;
}
