use super::*;
use crate::domain::UserId;
use crate::storage::Database;
use chrono::{DateTime, Duration, Utc};
use std::sync::Arc;

#[test]
fn test_dm_conversation_new() {
    let peer_id = UserId::new();
    let conv = DmConversation::new(peer_id);

    assert_eq!(conv.peer_id, peer_id);
    assert!(conv.our_mailbox.is_none());
    assert!(conv.peer_mailboxes.is_empty());
    assert!(conv.conversation_key.is_none());
}

#[test]
fn test_dm_conversation_needs_bootstrap() {
    let peer_id = UserId::new();
    let conv = DmConversation::new(peer_id);

    assert!(conv.needs_bootstrap());
    assert!(!conv.is_ready_to_send());
    assert!(!conv.is_ready_to_receive());
}

#[test]
fn test_peer_mailbox_new() {
    let public_key = [0u8; 32];
    let expires_at = Some(Utc::now() + Duration::days(7));

    let mailbox = PeerMailbox::new(public_key, 1, [1u8; 32], expires_at);

    assert_eq!(mailbox.public_key, public_key);
    assert!(mailbox.expires_at.is_some());
    assert!(!mailbox.is_possibly_expired());
}

#[test]
fn test_peer_mailbox_expired() {
    let public_key = [0u8; 32];
    let expires_at = Some(Utc::now() - Duration::hours(1));

    let mailbox = PeerMailbox::new(public_key, 1, [1u8; 32], expires_at);

    assert!(mailbox.is_possibly_expired());
}

#[test]
fn test_conversation_key_new() {
    let key = ConversationKey::new();

    assert_eq!(key.version, 1);
    assert!(key.their_ephemeral_public.is_none());
    assert!(key.shared_secret.is_none());
    assert!(!key.is_established());
    assert!(key.established_at.is_none());
}

#[test]
fn test_conversation_key_derive_shared_secret() {
    let mut key1 = ConversationKey::new();
    let mut key2 = ConversationKey::new();

    // Exchange public keys
    let pub1 = key1.our_ephemeral_public;
    let pub2 = key2.our_ephemeral_public;

    // Derive shared secrets
    key1.derive_shared_secret(pub2).unwrap();
    key2.derive_shared_secret(pub1).unwrap();

    // Both should derive the same shared secret
    assert!(key1.is_established());
    assert!(key2.is_established());
    assert_eq!(key1.shared_secret, key2.shared_secret);
    assert!(key1.established_at.is_some());
}

#[test]
fn test_conversation_key_with_version() {
    let key = ConversationKey::new_with_version(5);
    assert_eq!(key.version, 5);
}

#[test]
fn test_dm_mailbox_expires_soon() {
    let mailbox = DmMailbox {
        id: "test".to_string(),
        public_key: [0u8; 32],
        private_key: [0u8; 64],
        encryption_private_key: [0u8; 32],
        signature: vec![],
        token_id: "token".to_string(),
        created_at: Utc::now(),
        expires_at: Utc::now() + Duration::hours(12),
        last_polled_at: None,
    };

    assert!(!mailbox.is_expired());
    assert!(mailbox.expires_soon(Duration::hours(24)));
    assert!(!mailbox.expires_soon(Duration::hours(6)));
}

#[test]
fn test_dm_mailbox_is_expired() {
    let mailbox = DmMailbox {
        id: "test".to_string(),
        public_key: [0u8; 32],
        private_key: [0u8; 64],
        encryption_private_key: [0u8; 32],
        signature: vec![],
        token_id: "token".to_string(),
        created_at: Utc::now() - Duration::days(8),
        expires_at: Utc::now() - Duration::hours(1),
        last_polled_at: None,
    };

    assert!(mailbox.is_expired());
}

#[test]
fn test_dm_conversation_ready_states() {
    let peer_id = UserId::new();
    let mut conv = DmConversation::new(peer_id);

    // Add our mailbox
    conv.our_mailbox = Some(DmMailbox {
        id: "test".to_string(),
        public_key: [0u8; 32],
        private_key: [0u8; 64],
        encryption_private_key: [0u8; 32],
        signature: vec![],
        token_id: "token".to_string(),
        created_at: Utc::now(),
        expires_at: Utc::now() + Duration::days(7),
        last_polled_at: None,
    });

    assert!(!conv.needs_bootstrap());
    assert!(conv.is_ready_to_receive());
    assert!(!conv.is_ready_to_send()); // Still need peer mailbox and key

    // A peer device's half, but not ours yet: no secret to send under.
    let theirs = ConversationKey::new();
    conv.peer_mailboxes.push(PeerMailbox::new(
        [0u8; 32],
        1,
        theirs.our_ephemeral_public,
        None,
    ));
    assert!(!conv.is_ready_to_send());

    // Our half: that device's secret derives, and it is ready.
    conv.conversation_key = Some(ConversationKey::new());
    assert!(conv.is_ready_to_send());
    let (device, send_key) = conv.ready_devices()[0];
    assert_eq!(device.device_id, 1);
    let mut peer = theirs.clone();
    peer.derive_shared_secret(conv.conversation_key.as_ref().unwrap().our_ephemeral_public)
        .unwrap();
    assert_eq!(
        Some(send_key),
        peer.recv_key(),
        "the device's recv key opens what we send it"
    );
}

// ========================================================================
// L4 rotation: two-mailbox overlap semantics (DB-backed)
// ========================================================================

/// The token every test mailbox references (dm_mailboxes.token_id FK →
/// mailbox_tokens.id → mailbox_server_keys.key_id). Seeded by `setup_db`.
const TEST_TOKEN_ID: &str = "test-token";

fn setup_db() -> Arc<Database> {
    // tempfile + SQLCipher (matches call_repo tests): Database::open needs a
    // real path, so a temp file is the closest to in-memory available.
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("dm_conversation_test.db");
    let db = Arc::new(Database::open(&db_path, "testpass").unwrap());
    db.run_migrations().unwrap();

    // Seed the FK parents so saving a mailbox (which references a token)
    // satisfies dm_mailboxes.token_id → mailbox_tokens.id → server key.
    let conn = db.connection().unwrap();
    conn.execute(
        "INSERT INTO mailbox_server_keys (key_id, public_key_der, valid_until, created_at)
             VALUES ('test-key', X'00', 0, ?1)",
        rusqlite::params![Utc::now().to_rfc3339()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO mailbox_tokens
             (id, ephemeral_public_key, ephemeral_private_key, signature, server_key_id, created_at)
             VALUES (?1, X'01', X'02', X'03', 'test-key', ?2)",
        rusqlite::params![TEST_TOKEN_ID, Utc::now().to_rfc3339()],
    )
    .unwrap();
    drop(conn);

    crate::test_tempdir::hold(temp_dir);
    db
}

fn make_mailbox(id: &str, created_at: DateTime<Utc>, expires_at: DateTime<Utc>) -> DmMailbox {
    DmMailbox {
        id: id.to_string(),
        // Distinct public key per id so callers can assert which row won.
        public_key: [id.as_bytes()[0]; 32],
        private_key: [7u8; 64],
        encryption_private_key: [7u8; 32],
        signature: vec![1, 2, 3],
        token_id: TEST_TOKEN_ID.to_string(),
        created_at,
        expires_at,
        last_polled_at: None,
    }
}

#[test]
fn test_two_mailboxes_both_polled() {
    // (a) Two coexisting mailbox rows for one conversation → get_active_mailbox_keys
    // returns BOTH (overlap polling is automatic once two rows coexist).
    let db = setup_db();
    let mgr = DmConversationManager::new(db);
    let peer = UserId::new();

    let now = Utc::now();
    let old = make_mailbox("a", now - Duration::hours(2), now + Duration::hours(22));
    let new = make_mailbox("b", now, now + Duration::hours(24));
    mgr.save_our_mailbox(&peer, &old).unwrap();
    mgr.save_our_mailbox(&peer, &new).unwrap();

    let active = mgr.get_active_mailbox_keys().unwrap();
    let ids: Vec<String> = active.iter().map(|(_, _, _, id)| id.clone()).collect();
    assert_eq!(active.len(), 2, "both non-expired rows must be polled");
    assert!(ids.contains(&"a".to_string()));
    assert!(ids.contains(&"b".to_string()));
}

#[test]
fn test_send_targets_newest_mailbox() {
    // (b) load_conversation().our_mailbox (via load_our_mailbox_internal) returns
    // the NEWEST row (larger created_at) so sends target the rotated mailbox.
    let db = setup_db();
    let mgr = DmConversationManager::new(db);
    let peer = UserId::new();

    let now = Utc::now();
    let old = make_mailbox("a", now - Duration::hours(2), now + Duration::hours(22));
    let new = make_mailbox("b", now, now + Duration::hours(24));
    // Insert old last to prove ordering is by created_at, not insertion order.
    mgr.save_our_mailbox(&peer, &new).unwrap();
    mgr.save_our_mailbox(&peer, &old).unwrap();

    let conv = mgr.load_conversation(&peer).unwrap().unwrap();
    let our = conv.our_mailbox.expect("our mailbox present");
    assert_eq!(our.id, "b", "send mailbox must be the newest by created_at");
}

#[test]
fn test_expired_row_excluded_from_active_polling() {
    // (c) A row past its expires_at is excluded from active polling — the old
    // mailbox retires naturally once its TTL elapses.
    let db = setup_db();
    let mgr = DmConversationManager::new(db);
    let peer = UserId::new();

    let now = Utc::now();
    let expired = make_mailbox("a", now - Duration::hours(48), now - Duration::hours(1));
    let live = make_mailbox("b", now, now + Duration::hours(24));
    mgr.save_our_mailbox(&peer, &expired).unwrap();
    mgr.save_our_mailbox(&peer, &live).unwrap();

    let active = mgr.get_active_mailbox_keys().unwrap();
    let ids: Vec<String> = active.iter().map(|(_, _, _, id)| id.clone()).collect();
    assert_eq!(active.len(), 1, "expired row must be excluded from polling");
    assert_eq!(ids, vec!["b".to_string()]);
}

#[test]
fn test_dm_rotation_candidate_not_reselected_when_fresh() {
    // Re-rotation guard (over-rotation rider): while only an expiring row
    // exists the conversation is a candidate; once a fresh (post-rotation)
    // row coexists the conversation is NO LONGER selected — a lingering old
    // row can't re-trigger rotation every cycle and drain the token budget.
    let db = setup_db();
    let mgr = DmConversationManager::new(db);
    let peer = UserId::new();

    let now = Utc::now();
    // Threshold is 24h; this row expires in 12h → within the window.
    let expiring = make_mailbox("a", now - Duration::hours(36), now + Duration::hours(12));
    mgr.save_our_mailbox(&peer, &expiring).unwrap();

    let candidates = mgr.get_expiring_mailboxes(24).unwrap();
    assert_eq!(
        candidates,
        vec![peer],
        "expiring conversation must be a rotation candidate"
    );

    // Rotation mints a fresh row alongside the old (loss-free overlap).
    let fresh = make_mailbox("b", now, now + Duration::hours(48));
    mgr.save_our_mailbox(&peer, &fresh).unwrap();

    // Old row survives …
    assert_eq!(mgr.get_active_mailbox_keys().unwrap().len(), 2);
    // … but the conversation is no longer re-selected for rotation.
    let candidates = mgr.get_expiring_mailboxes(24).unwrap();
    assert!(
        candidates.is_empty(),
        "conversation with a fresh row must not be re-selected for rotation"
    );
}

/// Our half with `peer`, saved, and a peer device's key to answer it with.
fn seed_our_half(mgr: &DmConversationManager, peer: &UserId) -> ConversationKey {
    let now = Utc::now();
    mgr.save_our_mailbox(peer, &make_mailbox("o", now, now + Duration::hours(40)))
        .unwrap();
    let ours = ConversationKey::new();
    mgr.save_conversation_key(peer, &ours).unwrap();
    ours
}

/// F54: each peer device that answers keeps its own half. Device 8
/// answering hours after device 6 adds a second mailbox and secret; it
/// does not take device 6's over, and re-answering (F53) replaces only
/// its own.
#[test]
fn a_late_device_adds_its_half_without_taking_anothers() {
    let mgr = DmConversationManager::new(setup_db());
    let peer = UserId::new();
    let ours = seed_our_half(&mgr, &peer);
    let (six, eight) = (ConversationKey::new(), ConversationKey::new());

    mgr.record_peer_device(&peer, 6, [6u8; 32], six.our_ephemeral_public)
        .unwrap();
    mgr.record_peer_device(&peer, 8, [8u8; 32], eight.our_ephemeral_public)
        .unwrap();
    let conv = mgr.load_conversation(&peer).unwrap().unwrap();
    let ready: Vec<_> = conv
        .ready_devices()
        .iter()
        .map(|(m, k)| (m.device_id, m.public_key, *k))
        .collect();
    assert_eq!(ready.len(), 2);
    assert_eq!(
        (ready[0].0, ready[0].1),
        (6, [6u8; 32]),
        "device 6 keeps its mailbox"
    );
    assert_eq!((ready[1].0, ready[1].1), (8, [8u8; 32]));
    assert_ne!(ready[0].2, ready[1].2, "one secret per device");
    let mut six_side = six.clone();
    six_side
        .derive_shared_secret(ours.our_ephemeral_public)
        .unwrap();
    assert_eq!(Some(ready[0].2), six_side.recv_key());

    // Device 8 re-bootstraps: only its own row moves.
    let eight_again = ConversationKey::new();
    mgr.record_peer_device(&peer, 8, [0x88u8; 32], eight_again.our_ephemeral_public)
        .unwrap();
    let conv = mgr.load_conversation(&peer).unwrap().unwrap();
    let rows: Vec<_> = conv
        .peer_mailboxes
        .iter()
        .map(|m| (m.device_id, m.public_key))
        .collect();
    assert_eq!(rows, vec![(6, [6u8; 32]), (8, [0x88u8; 32])]);

    // A low-order ephemeral is refused and leaves the row alone.
    assert!(
        mgr.record_peer_device(&peer, 6, [9u8; 32], [0u8; 32])
            .is_err()
    );
    assert_eq!(
        mgr.load_conversation(&peer)
            .unwrap()
            .unwrap()
            .peer_mailboxes[0]
            .public_key,
        [6u8; 32]
    );

    // A rotation announce moves only the rotating device's mailbox.
    assert!(mgr.rotate_peer_mailbox(&peer, 6, [0x66u8; 32]).unwrap());
    assert!(!mgr.rotate_peer_mailbox(&peer, 7, [0x77u8; 32]).unwrap());
    let rows: Vec<_> = mgr
        .load_conversation(&peer)
        .unwrap()
        .unwrap()
        .peer_mailboxes
        .iter()
        .map(|m| (m.device_id, m.public_key))
        .collect();
    assert_eq!(rows, vec![(6, [0x66u8; 32]), (8, [0x88u8; 32])]);
}

/// A new ephemeral of ours drops every device's half (each was paired
/// with the old one); re-saving the same one keeps them.
#[test]
fn a_new_ephemeral_of_ours_drops_every_device_half() {
    let mgr = DmConversationManager::new(setup_db());
    let peer = UserId::new();
    let ours = seed_our_half(&mgr, &peer);
    mgr.record_peer_device(
        &peer,
        6,
        [6u8; 32],
        ConversationKey::new().our_ephemeral_public,
    )
    .unwrap();

    mgr.save_conversation_key(&peer, &ours).unwrap();
    assert_eq!(
        mgr.load_conversation(&peer)
            .unwrap()
            .unwrap()
            .peer_mailboxes
            .len(),
        1
    );

    mgr.save_conversation_key(&peer, &ConversationKey::new())
        .unwrap();
    let conv = mgr.load_conversation(&peer).unwrap().unwrap();
    assert!(conv.peer_mailboxes.is_empty());
    assert!(!conv.is_ready_to_send());
}

/// Copies owed to a device drain for that device only, in order; a
/// forgotten (retired) device loses its half and what it was owed.
#[test]
fn owed_copies_are_per_device() {
    let mgr = DmConversationManager::new(setup_db());
    let peer = UserId::new();
    seed_our_half(&mgr, &peer);
    mgr.record_peer_device(
        &peer,
        2,
        [2u8; 32],
        ConversationKey::new().our_ephemeral_public,
    )
    .unwrap();

    mgr.queue_for_device(&peer, 8, b"one").unwrap();
    mgr.queue_for_device(&peer, 8, b"two").unwrap();
    mgr.queue_for_device(&peer, 2, b"other").unwrap();

    assert_eq!(
        mgr.drain_device_queue(&peer, 8).unwrap(),
        vec![b"one".to_vec(), b"two".to_vec()]
    );
    assert!(mgr.drain_device_queue(&peer, 8).unwrap().is_empty());

    // An ask is remembered until the device answers.
    assert!(!mgr.was_device_asked(&peer, 8).unwrap());
    mgr.mark_device_asked(&peer, 8).unwrap();
    assert!(mgr.was_device_asked(&peer, 8).unwrap());
    mgr.record_peer_device(
        &peer,
        8,
        [8u8; 32],
        ConversationKey::new().our_ephemeral_public,
    )
    .unwrap();
    assert!(
        !mgr.was_device_asked(&peer, 8).unwrap(),
        "answering clears the ask"
    );

    mgr.forget_peer_device(&peer, 2).unwrap();
    assert!(mgr.drain_device_queue(&peer, 2).unwrap().is_empty());
    let left: Vec<u32> = mgr
        .load_conversation(&peer)
        .unwrap()
        .unwrap()
        .peer_mailboxes
        .iter()
        .map(|m| m.device_id)
        .collect();
    assert_eq!(left, vec![8], "only the retired device is gone");
}

// CRYPTO-10: every other X25519 site in the tree rejects a non-contributory
// ECDH; this one did not. A peer offering a low-order public key forced an
// all-zero DH output, and HKDF over a known input is a known conversation
// key — the attacker decrypts and forges the whole conversation.
#[test]
fn derive_shared_secret_rejects_low_order_peer_keys() {
    // RFC 7748 §6.1 / the standard small-order point set: each of these
    // drives the DH output to a constant.
    const LOW_ORDER: [[u8; 32]; 7] = [
        // 0 (order 1)
        [0u8; 32],
        // 1 (order 4)
        [
            1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0,
        ],
        [
            0xe0, 0xeb, 0x7a, 0x7c, 0x3b, 0x41, 0xb8, 0xae, 0x16, 0x56, 0xe3, 0xfa, 0xf1, 0x9f,
            0xc4, 0x6a, 0xda, 0x09, 0x8d, 0xeb, 0x9c, 0x32, 0xb1, 0xfd, 0x86, 0x62, 0x05, 0x16,
            0x5f, 0x49, 0xb8, 0x00,
        ],
        [
            0x5f, 0x9c, 0x95, 0xbc, 0xa3, 0x50, 0x8c, 0x24, 0xb1, 0xd0, 0xb1, 0x55, 0x9c, 0x83,
            0xef, 0x5b, 0x04, 0x44, 0x5c, 0xc4, 0x58, 0x1c, 0x8e, 0x86, 0xd8, 0x22, 0x4e, 0xdd,
            0xd0, 0x9f, 0x11, 0x57,
        ],
        // p-1, p, p+1
        [
            0xec, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0x7f,
        ],
        [
            0xed, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0x7f,
        ],
        [
            0xee, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0x7f,
        ],
    ];

    for (i, peer) in LOW_ORDER.iter().enumerate() {
        let mut key = ConversationKey::new();
        assert!(
            key.derive_shared_secret(*peer).is_err(),
            "low-order peer key #{i} was accepted"
        );
        assert!(
            !key.is_established(),
            "a rejected DH still established a key"
        );
    }

    // An honest peer key still works.
    let peer = ConversationKey::new();
    let mut ours = ConversationKey::new();
    ours.derive_shared_secret(peer.our_ephemeral_public)
        .expect("contributory ECDH must succeed");
    assert!(ours.is_established());
}
