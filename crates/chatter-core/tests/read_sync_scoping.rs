//! INFRA-25: `mark_read_by_timestamp` ran an unscoped
//! `UPDATE messages SET is_read = 1 WHERE sent_at = ?` across the whole table.
//!
//! Its doc comment justified that with "the ns timestamp is unique". Migration
//! v061 exists because it is not: mesh `sent_at` is milliseconds widened to
//! nanoseconds, and two distinct messages from one sender inside one millisecond
//! collided on the `(conversation_type, conversation_id, sender_id, sent_at)`
//! unique index -- one of them silently discarded. The function immediately
//! below it in the same file documents "Never does an unscoped
//! `WHERE sent_at = ?` UPDATE" as an invariant.
//!
//! Each test here is one row that a colliding timestamp must NOT flip.

use std::sync::Arc;
use tempfile::TempDir;

use chatter_core::domain::ids::{ChannelId, UserId};
use chatter_core::domain::message::{ConversationType, Message};
use chatter_core::storage::{Database, MessageRepository};

fn setup() -> (TempDir, Arc<Database>) {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(&temp_dir.path().join("read_sync.db"), "test_passphrase").unwrap();
    db.run_migrations().unwrap();
    (temp_dir, Arc::new(db))
}

fn unread_from(
    repo: &MessageRepository,
    conversation: ChannelId,
    kind: ConversationType,
    sender: UserId,
    sent_at: u64,
) -> Message {
    let mut msg = Message::new_text(conversation, kind, Some(sender), "hi".to_string(), sent_at);
    msg.is_read = false;
    repo.insert(&msg).unwrap();
    msg
}

fn is_read(repo: &MessageRepository, msg: &Message) -> bool {
    repo.get(&msg.id).unwrap().unwrap().is_read
}

#[test]
fn a_read_sync_marks_the_peers_dm_read() {
    let (_tmp, db) = setup();
    let repo = MessageRepository::new(db.clone());

    let me = UserId::new();
    let peer = UserId::new();
    let ts = 1_700_000_000_000_000_000u64;
    let msg = unread_from(&repo, ChannelId::from(peer), ConversationType::ServerDm, peer, ts);

    assert!(repo.mark_read_by_timestamp(ts, &me).unwrap());
    assert!(is_read(&repo, &msg), "the receipt's own subject must be marked read");
}

#[test]
fn a_read_sync_never_marks_our_own_message_read() {
    let (_tmp, db) = setup();
    let repo = MessageRepository::new(db.clone());

    let me = UserId::new();
    let peer = UserId::new();
    let ts = 1_700_000_000_000_000_001u64;
    // An outbound message that happens to share the timestamp. We are the
    // sender; "unread" is not a state it can be in for us.
    let mine = unread_from(&repo, ChannelId::from(peer), ConversationType::ServerDm, me, ts);

    let _ = repo.mark_read_by_timestamp(ts, &me);
    assert!(!is_read(&repo, &mine), "our own message was flipped by a peer's receipt");
}

#[test]
fn a_dm_read_sync_never_touches_a_channel_message() {
    let (_tmp, db) = setup();
    let repo = MessageRepository::new(db.clone());

    let me = UserId::new();
    let peer = UserId::new();
    let other = UserId::new();
    let ts = 1_700_000_000_000_000_002u64;

    let channel_msg = unread_from(
        &repo,
        ChannelId::new(),
        ConversationType::Channel,
        other,
        ts,
    );
    let dm_msg = unread_from(&repo, ChannelId::from(peer), ConversationType::ServerDm, peer, ts);

    assert!(repo.mark_read_by_timestamp(ts, &me).unwrap());
    assert!(is_read(&repo, &dm_msg));
    assert!(
        !is_read(&repo, &channel_msg),
        "a DmReceipt is channel-free by construction; it must not reach a channel row"
    );
}

#[test]
fn a_dm_read_sync_never_touches_a_mesh_message() {
    let (_tmp, db) = setup();
    let repo = MessageRepository::new(db.clone());

    let me = UserId::new();
    let peer = UserId::new();
    let other = UserId::new();
    // Mesh sent_at is milliseconds widened to nanoseconds (v061), so this is
    // exactly the collision class that migration was written for.
    let ts = 1_700_000_000_000_000_000u64;

    let mesh_msg = unread_from(&repo, ChannelId::new(), ConversationType::Mesh, other, ts);
    let mesh_dm = unread_from(&repo, ChannelId::new(), ConversationType::MeshDm, other, ts);
    let dm_msg = unread_from(&repo, ChannelId::from(peer), ConversationType::ServerDm, peer, ts);

    assert!(repo.mark_read_by_timestamp(ts, &me).unwrap());
    assert!(is_read(&repo, &dm_msg));
    assert!(!is_read(&repo, &mesh_msg), "mesh row flipped by a server DM receipt");
    assert!(!is_read(&repo, &mesh_dm), "mesh DM row flipped by a server DM receipt");
}

#[test]
fn an_already_read_message_is_not_reported_as_newly_read() {
    let (_tmp, db) = setup();
    let repo = MessageRepository::new(db.clone());

    let me = UserId::new();
    let peer = UserId::new();
    let ts = 1_700_000_000_000_000_003u64;
    let msg = unread_from(&repo, ChannelId::from(peer), ConversationType::ServerDm, peer, ts);

    assert!(repo.mark_read_by_timestamp(ts, &me).unwrap());
    assert!(
        !repo.mark_read_by_timestamp(ts, &me).unwrap(),
        "a duplicate receipt must not report a second state change"
    );
    assert!(is_read(&repo, &msg));
}
