//! Unit tests for blocking and rate limiting in Anonymous DMs and Groups.
//!
//! Tests the security features added for abuse prevention:
//! - User blocking in Anonymous DMs (global and per-conversation)
//! - User blocking in Anonymous Groups (per-group)
//! - Group blocking/muting status
//! - Rate limiting for message sends

mod common;

use std::sync::Arc;
use chrono::{Duration, Utc};

use chatter_linux::domain::anon_dm::{AnonDm, AnonDmId, AnonDmStatus};
use chatter_linux::domain::anon_group::{AnonGroup, AnonGroupId, AnonGroupStatus};
use chatter_linux::domain::UserId;
use chatter_linux::storage::migrations::default_migrations;
use chatter_linux::storage::repositories::{AnonDmRepository, AnonGroupRepository};
use chatter_linux::storage::Database;

// ============================================================================
// Test Helpers
// ============================================================================

/// Create an in-memory test database with all migrations applied.
fn setup_test_db() -> Arc<Database> {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let db_path = temp_dir.path().join("test.db");

    // Create database with migrations
    let db = Database::open(&db_path, "test_passphrase")
        .expect("Failed to create test database");

    // Run migrations
    {
        let conn = db.connection().expect("Failed to get connection");
        default_migrations()
            .run(&conn)
            .expect("Failed to run migrations");
    }

    // Leak the temp_dir so it doesn't get deleted
    std::mem::forget(temp_dir);

    Arc::new(db)
}

/// Generate a random UserId for testing.
fn random_user_id() -> UserId {
    UserId::new()
}

/// Generate a random AnonDmId for testing.
fn random_anon_dm_id() -> AnonDmId {
    AnonDmId::new()
}

/// Generate a random AnonGroupId for testing.
fn random_anon_group_id() -> AnonGroupId {
    AnonGroupId::new()
}

/// Create a test AnonDm conversation.
fn create_test_anon_dm(peer_id: UserId) -> AnonDm {
    let now = Utc::now();
    AnonDm {
        id: random_anon_dm_id(),
        peer_id,
        initiated_by_us: true,
        status: AnonDmStatus::Active,
        peer_mailbox_pk: None,
        peer_profile_key: None,
        peer_display_name: None,
        peer_avatar: None,
        created_at: now,
        updated_at: now,
        last_message_at: None,
        unread_count: 0,
    }
}

/// Create a test AnonGroup using the constructor.
fn create_test_anon_group(name: &str) -> AnonGroup {
    AnonGroup::new_as_owner(name.to_string(), chatter_linux::domain::UserId::new())
}

// ============================================================================
// Anonymous DM Blocking Tests
// ============================================================================

#[test]
fn test_anon_dm_conversation_status_blocking() {
    let db = setup_test_db();
    let repo = AnonDmRepository::new(db.clone());

    let peer_id = random_user_id();
    let dm = create_test_anon_dm(peer_id.clone());

    // Insert conversation
    repo.insert_conversation(&dm).expect("Insert should succeed");

    // Verify initial status is Active
    let fetched = repo.get_conversation(&dm.id)
        .expect("Get should succeed")
        .expect("Conversation should exist");
    assert_eq!(fetched.status, AnonDmStatus::Active);

    // Block the conversation
    repo.update_status(&dm.id, AnonDmStatus::Blocked)
        .expect("Update status should succeed");

    // Verify blocked status
    let blocked = repo.get_conversation(&dm.id)
        .expect("Get should succeed")
        .expect("Conversation should exist");
    assert_eq!(blocked.status, AnonDmStatus::Blocked);

    // Blocked conversations should not appear in active list
    let active = repo.list_active_conversations()
        .expect("List should succeed");
    assert!(active.is_empty() || !active.iter().any(|c| c.id == dm.id));
}

#[test]
fn test_anon_dm_global_user_blocking() {
    let db = setup_test_db();
    let repo = AnonDmRepository::new(db.clone());

    let user1 = random_user_id();
    let user2 = random_user_id();

    // Initially no users blocked
    assert!(!repo.is_user_blocked(&user1).expect("Check should succeed"));
    assert!(!repo.is_user_blocked(&user2).expect("Check should succeed"));

    // Block user1
    repo.block_user(&user1, Some("Spam")).expect("Block should succeed");

    // Verify user1 is blocked, user2 is not
    assert!(repo.is_user_blocked(&user1).expect("Check should succeed"));
    assert!(!repo.is_user_blocked(&user2).expect("Check should succeed"));

    // Get blocked users list
    let blocked = repo.get_blocked_users().expect("Get should succeed");
    assert_eq!(blocked.len(), 1);
    assert!(blocked.contains(&user1));

    // Unblock user1
    repo.unblock_user(&user1).expect("Unblock should succeed");
    assert!(!repo.is_user_blocked(&user1).expect("Check should succeed"));

    // Blocked list should be empty
    let blocked = repo.get_blocked_users().expect("Get should succeed");
    assert!(blocked.is_empty());
}

#[test]
fn test_anon_dm_block_multiple_users() {
    let db = setup_test_db();
    let repo = AnonDmRepository::new(db.clone());

    let users: Vec<UserId> = (0..5).map(|_| random_user_id()).collect();

    // Block all users
    for (i, user) in users.iter().enumerate() {
        repo.block_user(user, Some(&format!("Reason {}", i)))
            .expect("Block should succeed");
    }

    // Verify all are blocked
    for user in &users {
        assert!(repo.is_user_blocked(user).expect("Check should succeed"));
    }

    // Get blocked users
    let blocked = repo.get_blocked_users().expect("Get should succeed");
    assert_eq!(blocked.len(), 5);

    // Unblock one
    repo.unblock_user(&users[2]).expect("Unblock should succeed");

    let blocked = repo.get_blocked_users().expect("Get should succeed");
    assert_eq!(blocked.len(), 4);
    assert!(!blocked.contains(&users[2]));
}

// ============================================================================
// Anonymous DM Rate Limiting Tests
// ============================================================================

#[test]
fn test_anon_dm_rate_limit_initial() {
    let db = setup_test_db();
    let repo = AnonDmRepository::new(db.clone());

    let dm_id = random_anon_dm_id();

    // No rate limit entry initially
    let entry = repo.get_rate_limit(&dm_id).expect("Get should succeed");
    assert!(entry.is_none());
}

#[test]
fn test_anon_dm_rate_limit_increment() {
    let db = setup_test_db();
    let repo = AnonDmRepository::new(db.clone());

    let peer_id = random_user_id();
    let dm = create_test_anon_dm(peer_id);
    repo.insert_conversation(&dm).expect("Insert should succeed");

    let window = Duration::minutes(1);

    // First increment creates entry
    let count = repo.increment_rate_limit(&dm.id, window)
        .expect("Increment should succeed");
    assert_eq!(count, 1);

    // Subsequent increments increase counter
    let count = repo.increment_rate_limit(&dm.id, window)
        .expect("Increment should succeed");
    assert_eq!(count, 2);

    let count = repo.increment_rate_limit(&dm.id, window)
        .expect("Increment should succeed");
    assert_eq!(count, 3);

    // Verify entry exists with correct count
    let entry = repo.get_rate_limit(&dm.id)
        .expect("Get should succeed")
        .expect("Entry should exist");
    assert_eq!(entry.message_count, 3);
}

#[test]
fn test_anon_dm_rate_limit_window_expiry() {
    let db = setup_test_db();
    let repo = AnonDmRepository::new(db.clone());

    let peer_id = random_user_id();
    let dm = create_test_anon_dm(peer_id);
    repo.insert_conversation(&dm).expect("Insert should succeed");

    // Use very short window for testing
    let short_window = Duration::milliseconds(1);

    // First increment
    let count = repo.increment_rate_limit(&dm.id, short_window)
        .expect("Increment should succeed");
    assert_eq!(count, 1);

    // Wait for window to expire
    std::thread::sleep(std::time::Duration::from_millis(10));

    // Next increment should reset window
    let count = repo.increment_rate_limit(&dm.id, short_window)
        .expect("Increment should succeed");
    assert_eq!(count, 1); // Reset to 1, not 2
}

// ============================================================================
// Anonymous Group Status Tests
// ============================================================================

#[test]
fn test_anon_group_status_active() {
    let db = setup_test_db();
    let repo = AnonGroupRepository::new(db.clone());

    let group = create_test_anon_group("Test Active Group");
    repo.insert_group(&group).expect("Insert should succeed");

    // Verify initial status
    let fetched = repo.get_group(&group.id)
        .expect("Get should succeed")
        .expect("Group should exist");
    assert_eq!(fetched.status, AnonGroupStatus::Active);
    assert!(!fetched.is_blocked());
    assert!(!fetched.is_muted());
}

#[test]
fn test_anon_group_status_blocked() {
    let db = setup_test_db();
    let repo = AnonGroupRepository::new(db.clone());

    let group = create_test_anon_group("Test Blocked Group");
    repo.insert_group(&group).expect("Insert should succeed");

    // Block the group
    repo.update_status(&group.id, AnonGroupStatus::Blocked)
        .expect("Update should succeed");

    // Verify blocked status
    let fetched = repo.get_group(&group.id)
        .expect("Get should succeed")
        .expect("Group should exist");
    assert_eq!(fetched.status, AnonGroupStatus::Blocked);
    assert!(fetched.is_blocked());
    assert!(!fetched.status.can_send());
    assert!(!fetched.status.can_receive());

    // Blocked groups should not appear in active list
    let active = repo.list_active_groups()
        .expect("List should succeed");
    assert!(!active.iter().any(|g| g.id == group.id));
}

#[test]
fn test_anon_group_status_muted() {
    let db = setup_test_db();
    let repo = AnonGroupRepository::new(db.clone());

    let group = create_test_anon_group("Test Muted Group");
    repo.insert_group(&group).expect("Insert should succeed");

    // Mute the group
    repo.update_status(&group.id, AnonGroupStatus::Muted)
        .expect("Update should succeed");

    // Verify muted status - can still send/receive but notifications suppressed
    let fetched = repo.get_group(&group.id)
        .expect("Get should succeed")
        .expect("Group should exist");
    assert_eq!(fetched.status, AnonGroupStatus::Muted);
    assert!(fetched.is_muted());
    assert!(fetched.status.can_send());
    assert!(fetched.status.can_receive());

    // Muted groups SHOULD appear in active list (they're still active, just muted)
    let active = repo.list_active_groups()
        .expect("List should succeed");
    assert!(active.iter().any(|g| g.id == group.id));
}

#[test]
fn test_anon_group_status_transitions() {
    let db = setup_test_db();
    let repo = AnonGroupRepository::new(db.clone());

    let group = create_test_anon_group("Transition Test");
    repo.insert_group(&group).expect("Insert should succeed");

    // Active -> Muted
    repo.update_status(&group.id, AnonGroupStatus::Muted)
        .expect("Update should succeed");
    let g = repo.get_group(&group.id).unwrap().unwrap();
    assert_eq!(g.status, AnonGroupStatus::Muted);

    // Muted -> Blocked
    repo.update_status(&group.id, AnonGroupStatus::Blocked)
        .expect("Update should succeed");
    let g = repo.get_group(&group.id).unwrap().unwrap();
    assert_eq!(g.status, AnonGroupStatus::Blocked);

    // Blocked -> Active (unblock)
    repo.update_status(&group.id, AnonGroupStatus::Active)
        .expect("Update should succeed");
    let g = repo.get_group(&group.id).unwrap().unwrap();
    assert_eq!(g.status, AnonGroupStatus::Active);
}

// ============================================================================
// Anonymous Group Per-User Blocking Tests
// ============================================================================

#[test]
fn test_anon_group_block_user() {
    let db = setup_test_db();
    let repo = AnonGroupRepository::new(db.clone());

    let group = create_test_anon_group("Block User Test");
    repo.insert_group(&group).expect("Insert should succeed");

    let user1 = random_user_id();
    let user2 = random_user_id();

    // Initially no users blocked
    assert!(!repo.is_user_blocked(&group.id, &user1).expect("Check should succeed"));
    assert!(!repo.is_user_blocked(&group.id, &user2).expect("Check should succeed"));

    // Block user1 in this group
    repo.block_user(&group.id, &user1, Some("Harassment"))
        .expect("Block should succeed");

    // Verify user1 blocked, user2 not
    assert!(repo.is_user_blocked(&group.id, &user1).expect("Check should succeed"));
    assert!(!repo.is_user_blocked(&group.id, &user2).expect("Check should succeed"));
}

#[test]
fn test_anon_group_block_user_per_group() {
    let db = setup_test_db();
    let repo = AnonGroupRepository::new(db.clone());

    let group1 = create_test_anon_group("Group 1");
    let group2 = create_test_anon_group("Group 2");
    repo.insert_group(&group1).expect("Insert should succeed");
    repo.insert_group(&group2).expect("Insert should succeed");

    let user = random_user_id();

    // Block user only in group1
    repo.block_user(&group1.id, &user, None).expect("Block should succeed");

    // User should be blocked in group1, not in group2
    assert!(repo.is_user_blocked(&group1.id, &user).expect("Check should succeed"));
    assert!(!repo.is_user_blocked(&group2.id, &user).expect("Check should succeed"));
}

#[test]
fn test_anon_group_unblock_user() {
    let db = setup_test_db();
    let repo = AnonGroupRepository::new(db.clone());

    let group = create_test_anon_group("Unblock Test");
    repo.insert_group(&group).expect("Insert should succeed");

    let user = random_user_id();

    // Block then unblock
    repo.block_user(&group.id, &user, Some("Mistake")).expect("Block should succeed");
    assert!(repo.is_user_blocked(&group.id, &user).expect("Check should succeed"));

    repo.unblock_user(&group.id, &user).expect("Unblock should succeed");
    assert!(!repo.is_user_blocked(&group.id, &user).expect("Check should succeed"));
}

#[test]
fn test_anon_group_get_blocked_users() {
    let db = setup_test_db();
    let repo = AnonGroupRepository::new(db.clone());

    let group = create_test_anon_group("Blocked Users List");
    repo.insert_group(&group).expect("Insert should succeed");

    let users: Vec<UserId> = (0..3).map(|_| random_user_id()).collect();

    // Block all users
    for user in &users {
        repo.block_user(&group.id, user, None).expect("Block should succeed");
    }

    // Get blocked users
    let blocked = repo.get_blocked_users(&group.id).expect("Get should succeed");
    assert_eq!(blocked.len(), 3);

    // Verify all users are in the blocked list
    for user in &users {
        assert!(blocked.contains(user));
    }
}

// ============================================================================
// Anonymous Group Rate Limiting Tests
// ============================================================================

#[test]
fn test_anon_group_rate_limit_initial() {
    let db = setup_test_db();
    let repo = AnonGroupRepository::new(db.clone());

    let group_id = random_anon_group_id();

    // No rate limit entry initially
    let entry = repo.get_rate_limit(&group_id).expect("Get should succeed");
    assert!(entry.is_none());
}

#[test]
fn test_anon_group_rate_limit_increment() {
    let db = setup_test_db();
    let repo = AnonGroupRepository::new(db.clone());

    let group = create_test_anon_group("Rate Limit Test");
    repo.insert_group(&group).expect("Insert should succeed");

    let window = Duration::minutes(1);

    // Increment multiple times
    for expected in 1..=5u32 {
        let count = repo.increment_rate_limit(&group.id, window)
            .expect("Increment should succeed");
        assert_eq!(count, expected);
    }

    // Verify entry
    let entry = repo.get_rate_limit(&group.id)
        .expect("Get should succeed")
        .expect("Entry should exist");
    assert_eq!(entry.message_count, 5);
}

#[test]
fn test_anon_group_rate_limit_separate_per_group() {
    let db = setup_test_db();
    let repo = AnonGroupRepository::new(db.clone());

    let group1 = create_test_anon_group("Group 1");
    let group2 = create_test_anon_group("Group 2");
    repo.insert_group(&group1).expect("Insert should succeed");
    repo.insert_group(&group2).expect("Insert should succeed");

    let window = Duration::minutes(1);

    // Increment group1 multiple times
    for _ in 0..5 {
        repo.increment_rate_limit(&group1.id, window).expect("Increment should succeed");
    }

    // Increment group2 fewer times
    for _ in 0..2 {
        repo.increment_rate_limit(&group2.id, window).expect("Increment should succeed");
    }

    // Verify separate counts
    let entry1 = repo.get_rate_limit(&group1.id).unwrap().unwrap();
    let entry2 = repo.get_rate_limit(&group2.id).unwrap().unwrap();

    assert_eq!(entry1.message_count, 5);
    assert_eq!(entry2.message_count, 2);
}

// ============================================================================
// AnonGroupStatus Enum Tests
// ============================================================================

#[test]
fn test_anon_group_status_enum_can_send() {
    assert!(AnonGroupStatus::Active.can_send());
    assert!(AnonGroupStatus::Muted.can_send());
    assert!(!AnonGroupStatus::Blocked.can_send());
}

#[test]
fn test_anon_group_status_enum_can_receive() {
    assert!(AnonGroupStatus::Active.can_receive());
    assert!(AnonGroupStatus::Muted.can_receive());
    assert!(!AnonGroupStatus::Blocked.can_receive());
}

#[test]
fn test_anon_group_status_enum_is_blocked() {
    assert!(!AnonGroupStatus::Active.is_blocked());
    assert!(!AnonGroupStatus::Muted.is_blocked());
    assert!(AnonGroupStatus::Blocked.is_blocked());
}

#[test]
fn test_anon_group_status_from_i32() {
    assert_eq!(AnonGroupStatus::from_i32(0), AnonGroupStatus::Active);
    assert_eq!(AnonGroupStatus::from_i32(1), AnonGroupStatus::Muted);
    assert_eq!(AnonGroupStatus::from_i32(2), AnonGroupStatus::Blocked);
    // Unknown values default to Active
    assert_eq!(AnonGroupStatus::from_i32(99), AnonGroupStatus::Active);
}

#[test]
fn test_anon_group_status_to_i32() {
    assert_eq!(AnonGroupStatus::Active.to_i32(), 0);
    assert_eq!(AnonGroupStatus::Muted.to_i32(), 1);
    assert_eq!(AnonGroupStatus::Blocked.to_i32(), 2);
}

// ============================================================================
// AnonDmStatus Tests
// ============================================================================

#[test]
fn test_anon_dm_status_can_send() {
    assert!(!AnonDmStatus::Pending.can_send());
    assert!(AnonDmStatus::Active.can_send());
    assert!(!AnonDmStatus::Blocked.can_send());
    assert!(!AnonDmStatus::Declined.can_send());
}

#[test]
fn test_anon_dm_status_from_i32() {
    assert_eq!(AnonDmStatus::from_i32(0), AnonDmStatus::Pending);
    assert_eq!(AnonDmStatus::from_i32(1), AnonDmStatus::Active);
    assert_eq!(AnonDmStatus::from_i32(2), AnonDmStatus::Blocked);
    assert_eq!(AnonDmStatus::from_i32(3), AnonDmStatus::Declined);
    // Unknown values default to Pending
    assert_eq!(AnonDmStatus::from_i32(99), AnonDmStatus::Pending);
}
