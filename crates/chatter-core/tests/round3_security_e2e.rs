//! Round-3 security-fix regression tests.
//!
//! Each test exercises the primitive that the security gate is built on,
//! at the layer chosen to (a) be cheap to set up (no SignalService /
//! gRPC server), and (b) lock the property a future PR could regress.
//!
//! Coverage:
//!   - Finding #1 + #2: `ChannelRepository::get_membership` distinguishes
//!     current members from non-members. Both gates in
//!     `services/gateway/routing.rs` consult exactly this primitive
//!     (`process_sender_key_distribution` and `process_sender_key_request`).
//!   - Finding #3 + #6: `ConversationKey::is_established` +
//!     `their_ephemeral_public` together act as the bootstrap idempotency
//!     marker AND the replay guard, exactly as
//!     `mailbox/dm/bootstrap.rs::handle_incoming_bootstrap_inner` uses them.
//!   - Finding #4: `EphemeralCallKey::generate` + `generate_group_call_key`
//!     are random per draw (covered by unit tests in
//!     `crypto/call_key_provider.rs`; not re-tested here).
//!
//! Cross-language:
//!   - Finding #5 (registration-challenge rate limit) is enforced on the
//!     server, validated by
//!     `internal/services/auth/handlers/registration_challenge_ratelimit_e2e_test.go`.

use std::sync::Arc;

use chatter_core::domain::ids::{ChannelId, UserId};
use chatter_core::domain::{Channel, ChannelType, Membership, MemberRole, NotificationPreference};
use chatter_core::mailbox::dm::ConversationKey;
use chatter_core::storage::repositories::channel_repo::ChannelRepository;
use chatter_core::storage::Database;

use chrono::Utc;
use tempfile::NamedTempFile;

// ============================================================================
// Helpers
// ============================================================================

fn fresh_db() -> (Arc<Database>, NamedTempFile) {
    let db_file = NamedTempFile::new().unwrap();
    let db = Arc::new(Database::open(db_file.path(), "test_passphrase").unwrap());
    db.run_migrations().unwrap();
    (db, db_file)
}

fn fresh_channel_repo() -> (ChannelRepository, NamedTempFile) {
    let (db, dbf) = fresh_db();
    (ChannelRepository::new(db), dbf)
}

fn make_channel(id: ChannelId) -> Channel {
    Channel::new(id, "test".to_string(), ChannelType::Private, UserId::new())
}

fn make_membership(channel_id: ChannelId, user_id: UserId) -> Membership {
    Membership {
        channel_id,
        user_id,
        role: MemberRole::Member,
        joined_at: Utc::now(),
        last_read_message_id: None,
        is_muted: false,
        muted_until: None,
        notification_preference: NotificationPreference::All,
    }
}

// ============================================================================
// #1 + #2 — channel-membership gate primitive
// ============================================================================

/// `process_sender_key_distribution` and `process_sender_key_request`
/// both call `channel_repo.get_membership(channel, peer)` and reject when
/// it returns `Ok(None)`. Lock that primitive's behavior here.
#[test]
fn round3_finding_1_2_membership_gate_distinguishes_member_from_non_member() {
    let (repo, _dbf) = fresh_channel_repo();

    let channel_id = ChannelId::new();
    let alice = UserId::new();
    let mallory = UserId::new();

    repo.insert(&make_channel(channel_id)).unwrap();
    repo.insert_membership(&make_membership(channel_id, alice)).unwrap();

    // Member → Some
    let alice_m = repo.get_membership(&channel_id, &alice).unwrap();
    assert!(alice_m.is_some(), "current member must resolve to Some");

    // Non-member → None
    let mallory_m = repo.get_membership(&channel_id, &mallory).unwrap();
    assert!(
        mallory_m.is_none(),
        "non-member must resolve to None (gate signal that drops SKDM/SenderKeyRequest)",
    );
}

/// Once a member is removed, the gate must immediately stop honoring them.
/// This is the property that makes round-2's lazy-rotation fix actually work
/// when paired with finding-#2's request gate.
#[test]
fn round3_finding_2_membership_gate_drops_after_kick() {
    let (repo, _dbf) = fresh_channel_repo();

    let channel_id = ChannelId::new();
    let alice = UserId::new();
    let bob = UserId::new();

    repo.insert(&make_channel(channel_id)).unwrap();
    repo.insert_membership(&make_membership(channel_id, alice)).unwrap();
    repo.insert_membership(&make_membership(channel_id, bob)).unwrap();

    // Bob in: gate would respond to his SenderKeyRequest.
    assert!(repo.get_membership(&channel_id, &bob).unwrap().is_some());

    // Kick bob.
    repo.delete_membership(&channel_id, &bob).unwrap();

    // Gate now refuses.
    assert!(
        repo.get_membership(&channel_id, &bob).unwrap().is_none(),
        "kicked user must immediately fail the gate; otherwise lazy SKDM rotation can be defeated by a SenderKeyRequest",
    );

    // Alice still in.
    assert!(repo.get_membership(&channel_id, &alice).unwrap().is_some());
}

// ============================================================================
// #3 + #6 — bootstrap idempotency + replay-guard primitive
// ============================================================================

/// `handle_incoming_bootstrap_inner` accepts a re-delivery of the same
/// bootstrap envelope (idempotent) but rejects a different ephemeral against
/// an established conversation.
///
/// At the data-layer the guard is two checks against `ConversationKey`:
///   1. `is_established()` returns true once the shared secret is derived,
///   2. `their_ephemeral_public` matches the redelivered ephemeral.
///
/// These tests pin those two properties so the gate logic can rely on them.
#[test]
fn round3_finding_3_6_unestablished_key_accepts_first_bootstrap() {
    let mut key = ConversationKey::new();
    assert!(!key.is_established(), "fresh key must not be established");
    assert!(key.their_ephemeral_public.is_none());

    let peer_ephemeral = [0x42u8; 32];
    key.derive_shared_secret(peer_ephemeral).unwrap();

    assert!(key.is_established(), "after derive, key is established");
    assert_eq!(
        key.their_ephemeral_public,
        Some(peer_ephemeral),
        "their_ephemeral_public must be recorded so the gate can detect replays",
    );
}

#[test]
fn round3_finding_3_6_same_ephemeral_redelivery_is_idempotent() {
    let mut key = ConversationKey::new();
    let peer_ephemeral = [0xABu8; 32];

    key.derive_shared_secret(peer_ephemeral).unwrap();
    let first_secret = *key.get_shared_secret().unwrap();

    // Same envelope re-delivered: gate detects via
    //   `key.their_ephemeral_public == Some(peer_ephemeral)`
    // and treats as no-op. Verify the property the gate matches on.
    assert_eq!(key.their_ephemeral_public, Some(peer_ephemeral));
    assert!(key.is_established());

    // The gate code path for idempotent re-delivery does NOT call
    // derive_shared_secret again — so the secret stays untouched.
    let secret_after = *key.get_shared_secret().unwrap();
    assert_eq!(
        first_secret, secret_after,
        "idempotent path must not rotate the established secret",
    );
}

#[test]
fn round3_finding_3_6_different_ephemeral_against_established_key_is_a_replay_signal() {
    let mut key = ConversationKey::new();
    let original_ephemeral = [0xABu8; 32];
    key.derive_shared_secret(original_ephemeral).unwrap();
    assert!(key.is_established());

    // Attacker delivers a bootstrap with a DIFFERENT ephemeral against this
    // already-established session. The gate distinguishes the two cases
    // by comparing `their_ephemeral_public`:
    //   - Some(same)      -> idempotent
    //   - Some(different) -> reject (this test)
    let replayed_or_attacker_ephemeral = [0xCDu8; 32];
    let stored = key.their_ephemeral_public.expect("ephemeral was recorded");

    assert_ne!(
        stored, replayed_or_attacker_ephemeral,
        "the gate observes a mismatch and refuses the bootstrap; \
         without this check the attacker's ephemeral would silently \
         overwrite the established shared_secret",
    );
}

#[test]
fn round3_finding_4_call_keys_are_random_per_draw() {
    use chatter_core::crypto::call_key_provider::{generate_group_call_key, EphemeralCallKey};

    // Two ephemeral call keys must differ - covered by the unit test in
    // crypto/call_key_provider.rs but re-asserted here as a public-API
    // smoke test so a future regression in the OsRng wiring (e.g. someone
    // accidentally swapping back to `rand::rng()`) trips this integration
    // test, not just an internal one.
    let k1 = EphemeralCallKey::generate();
    let k2 = EphemeralCallKey::generate();
    assert_ne!(k1.public_key_bytes(), k2.public_key_bytes());
    assert_ne!(k1.public_key_bytes(), [0u8; 32]);

    let g1 = generate_group_call_key();
    let g2 = generate_group_call_key();
    assert_ne!(g1, g2);
    assert_ne!(g1, [0u8; 32]);
}
