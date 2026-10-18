use super::*;

#[test]
fn test_group_mailbox_state_new() {
    let group_id = ChannelId::new();
    let state = GroupMailboxState::new(group_id);

    assert_eq!(state.group_id, group_id);
    assert!(state.our_mailbox.is_none());
    assert!(state.member_mailboxes.is_empty());
}

#[test]
fn test_group_mailbox_state_needs_mailbox() {
    let group_id = ChannelId::new();
    let state = GroupMailboxState::new(group_id);

    assert!(state.needs_mailbox());
    assert!(!state.is_ready_to_send());
    assert!(!state.is_ready_to_receive());
}

#[test]
fn test_member_mailbox_new() {
    let user_id = UserId::new();
    let public_key = [0u8; 32];
    let expires_at = Some(Utc::now() + Duration::days(7));

    let mailbox = MemberMailbox::new(user_id, public_key, expires_at);

    assert_eq!(mailbox.user_id, user_id);
    assert_eq!(mailbox.public_key, public_key);
    assert!(mailbox.expires_at.is_some());
    assert!(!mailbox.is_possibly_expired());
}

#[test]
fn test_member_mailbox_expired() {
    let user_id = UserId::new();
    let public_key = [0u8; 32];
    let expires_at = Some(Utc::now() - Duration::hours(1));

    let mailbox = MemberMailbox::new(user_id, public_key, expires_at);

    assert!(mailbox.is_possibly_expired());
}

#[test]
fn test_group_mailbox_expires_soon() {
    let mailbox = GroupMailbox {
        id: "test".to_string(),
        group_id: ChannelId::new(),
        public_key: [0u8; 32],
        private_key: [0u8; 64],
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
fn test_group_mailbox_is_expired() {
    let mailbox = GroupMailbox {
        id: "test".to_string(),
        group_id: ChannelId::new(),
        public_key: [0u8; 32],
        private_key: [0u8; 64],
        signature: vec![],
        token_id: "token".to_string(),
        created_at: Utc::now() - Duration::days(8),
        expires_at: Utc::now() - Duration::hours(1),
        last_polled_at: None,
    };

    assert!(mailbox.is_expired());
}

#[test]
fn test_group_mailbox_state_ready_states() {
    let group_id = ChannelId::new();
    let mut state = GroupMailboxState::new(group_id);

    // Add our mailbox
    state.our_mailbox = Some(GroupMailbox {
        id: "test".to_string(),
        group_id,
        public_key: [0u8; 32],
        private_key: [0u8; 64],
        signature: vec![],
        token_id: "token".to_string(),
        created_at: Utc::now(),
        expires_at: Utc::now() + Duration::days(7),
        last_polled_at: None,
    });

    assert!(!state.needs_mailbox());
    assert!(state.is_ready_to_receive());
    assert!(!state.is_ready_to_send()); // No member mailboxes yet

    // Add a member mailbox
    let member_id = UserId::new();
    state
        .member_mailboxes
        .insert(member_id, MemberMailbox::new(member_id, [0u8; 32], None));

    assert!(state.is_ready_to_send());
    assert_eq!(state.member_count(), 1);
    assert!(state.get_member_mailbox(&member_id).is_some());
}

#[test]
fn test_group_mailbox_state_mailbox_needs_refresh() {
    let group_id = ChannelId::new();
    let mut state = GroupMailboxState::new(group_id);

    // No mailbox - doesn't need refresh
    assert!(!state.mailbox_needs_refresh());

    // Add mailbox expiring soon
    state.our_mailbox = Some(GroupMailbox {
        id: "test".to_string(),
        group_id,
        public_key: [0u8; 32],
        private_key: [0u8; 64],
        signature: vec![],
        token_id: "token".to_string(),
        created_at: Utc::now(),
        expires_at: Utc::now() + Duration::hours(12),
        last_polled_at: None,
    });

    assert!(state.mailbox_needs_refresh());

    // Mailbox not expiring soon
    state.our_mailbox = Some(GroupMailbox {
        id: "test2".to_string(),
        group_id,
        public_key: [0u8; 32],
        private_key: [0u8; 64],
        signature: vec![],
        token_id: "token".to_string(),
        created_at: Utc::now(),
        expires_at: Utc::now() + Duration::days(7),
        last_polled_at: None,
    });

    assert!(!state.mailbox_needs_refresh());
}

// ========================================================================
// L4 group rotation: two-mailbox overlap semantics (DB-backed)
// ========================================================================

/// The token every test mailbox references (group_mailboxes.token_id FK →
/// mailbox_tokens.id → mailbox_server_keys.key_id). Seeded by `setup_db`.
const TEST_TOKEN_ID: &str = "test-token";

fn setup_db() -> Arc<Database> {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("group_mailbox_test.db");
    let db = Arc::new(Database::open(&db_path, "testpass").unwrap());
    db.run_migrations().unwrap();

    // Seed the FK parents so saving a mailbox (which references a token)
    // satisfies group_mailboxes.token_id → mailbox_tokens.id → server key.
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

fn make_group_mailbox(
    id: &str,
    group_id: ChannelId,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
) -> GroupMailbox {
    GroupMailbox {
        id: id.to_string(),
        group_id,
        // Distinct public key per id so callers can assert which row won.
        public_key: [id.as_bytes()[0]; 32],
        private_key: [7u8; 64],
        signature: vec![1, 2, 3],
        token_id: TEST_TOKEN_ID.to_string(),
        created_at,
        expires_at,
        last_polled_at: None,
    }
}

#[test]
fn test_two_group_mailboxes_both_polled() {
    // Two coexisting mailbox rows for one group → get_all_group_mailboxes
    // returns BOTH (overlap polling is automatic once two rows coexist).
    let db = setup_db();
    let mgr = GroupMailboxManager::new(db);
    let group = ChannelId::new();

    let now = Utc::now();
    let old = make_group_mailbox(
        "a",
        group,
        now - Duration::hours(2),
        now + Duration::hours(22),
    );
    let new = make_group_mailbox("b", group, now, now + Duration::hours(24));
    mgr.save_our_mailbox(&old).unwrap();
    mgr.save_our_mailbox(&new).unwrap();

    let active = mgr.get_all_group_mailboxes().unwrap();
    let ids: Vec<String> = active.iter().map(|(_, _, _, id, _)| id.clone()).collect();
    assert_eq!(active.len(), 2, "both non-expired rows must be polled");
    assert!(ids.contains(&"a".to_string()));
    assert!(ids.contains(&"b".to_string()));
}

#[test]
fn test_group_send_targets_newest_mailbox() {
    // load_group_state().our_mailbox (via load_our_mailbox_internal) returns
    // the NEWEST row (larger created_at) so sends target the rotated mailbox.
    let db = setup_db();
    let mgr = GroupMailboxManager::new(db);
    let group = ChannelId::new();

    let now = Utc::now();
    let old = make_group_mailbox(
        "a",
        group,
        now - Duration::hours(2),
        now + Duration::hours(22),
    );
    let new = make_group_mailbox("b", group, now, now + Duration::hours(24));
    // Insert old last to prove ordering is by created_at, not insertion order.
    mgr.save_our_mailbox(&new).unwrap();
    mgr.save_our_mailbox(&old).unwrap();

    let state = mgr.load_group_state(&group).unwrap().unwrap();
    let our = state.our_mailbox.expect("our mailbox present");
    assert_eq!(our.id, "b", "send mailbox must be the newest by created_at");
}

#[test]
fn test_expired_group_row_excluded() {
    // A row past its expires_at is excluded from active polling — the old
    // mailbox retires naturally once its TTL elapses.
    let db = setup_db();
    let mgr = GroupMailboxManager::new(db);
    let group = ChannelId::new();

    let now = Utc::now();
    let expired = make_group_mailbox(
        "a",
        group,
        now - Duration::hours(48),
        now - Duration::hours(1),
    );
    let live = make_group_mailbox("b", group, now, now + Duration::hours(24));
    mgr.save_our_mailbox(&expired).unwrap();
    mgr.save_our_mailbox(&live).unwrap();

    let active = mgr.get_all_group_mailboxes().unwrap();
    let ids: Vec<String> = active.iter().map(|(_, _, _, id, _)| id.clone()).collect();
    assert_eq!(active.len(), 1, "expired row must be excluded from polling");
    assert_eq!(ids, vec!["b".to_string()]);
}

#[test]
fn test_group_rotation_candidate_not_reselected_when_fresh() {
    // Re-rotation guard (gate G5): while only an expiring row exists the
    // group is a candidate; once a fresh (post-rotation) row coexists the
    // group is NO LONGER selected — so a lingering old row can't re-trigger
    // rotation every cycle and drain the token budget. Also proves the old
    // row survives the rotate (loss-free overlap).
    let db = setup_db();
    let mgr = GroupMailboxManager::new(db);
    let group = ChannelId::new();

    let now = Utc::now();
    // Threshold is 24h; this row expires in 12h → within the window.
    let expiring = make_group_mailbox(
        "a",
        group,
        now - Duration::hours(36),
        now + Duration::hours(12),
    );
    mgr.save_our_mailbox(&expiring).unwrap();

    let candidates = mgr.get_expiring_group_mailboxes(24).unwrap();
    assert_eq!(
        candidates,
        vec![group],
        "expiring group must be a rotation candidate"
    );

    // Rotation mints a fresh row alongside the old (both coexist).
    let fresh = make_group_mailbox("b", group, now, now + Duration::hours(48));
    mgr.save_our_mailbox(&fresh).unwrap();

    // Old row survives (loss-free overlap) …
    let active = mgr.get_all_group_mailboxes().unwrap();
    assert_eq!(active.len(), 2, "old row must survive rotation for overlap");

    // … but the group is no longer re-selected for rotation.
    let candidates = mgr.get_expiring_group_mailboxes(24).unwrap();
    assert!(
        candidates.is_empty(),
        "group with a fresh row must not be re-selected for rotation"
    );
}

#[test]
fn drop_departed_member_mailboxes_keeps_only_current_members() {
    let db = setup_db();
    let mgr = GroupMailboxManager::new(db);
    let group = ChannelId::new();
    let stayed = UserId::new();
    let departed = UserId::new();
    mgr.save_member_mailbox(&group, &MemberMailbox::new(stayed, [1u8; 32], None))
        .unwrap();
    mgr.save_member_mailbox(&group, &MemberMailbox::new(departed, [2u8; 32], None))
        .unwrap();

    let dropped = mgr
        .drop_departed_member_mailboxes(&group, &[stayed])
        .unwrap();
    assert_eq!(dropped, 1, "the departed member's mailbox row is removed");

    let state = mgr.load_group_state(&group).unwrap().unwrap();
    assert!(state.member_mailboxes.contains_key(&stayed));
    assert!(
        !state.member_mailboxes.contains_key(&departed),
        "a departed member must not keep an address we could fan the new key to"
    );
    assert_eq!(
        mgr.drop_departed_member_mailboxes(&group, &[]).unwrap(),
        0,
        "an empty membership read must not wipe the rows we still trust"
    );
    assert!(
        mgr.load_group_state(&group)
            .unwrap()
            .unwrap()
            .member_mailboxes
            .contains_key(&stayed)
    );
}
