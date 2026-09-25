//! INFRA-25: the old read-sync (`mark_read_by_timestamp`) ran an unscoped
//! `UPDATE messages SET is_read = 1 WHERE sent_at = ?` across the whole table.
//!
//! Its doc comment justified that with "the ns timestamp is unique". Migration
//! v061 exists because it is not: mesh `sent_at` is milliseconds widened to
//! nanoseconds, and two distinct messages from one sender inside one millisecond
//! collided on the `(conversation_type, conversation_id, sender_id, sent_at)`
//! unique index -- one of them silently discarded.
//!
//! F55 replaced it with `ReadSync`, whose entries name conversation, author and
//! sent time (`mark_read_synced`). Each test here is one row that a colliding
//! timestamp must NOT flip.

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
    let conv = ChannelId::from(peer);
    let msg = unread_from(&repo, conv, ConversationType::ServerDm, peer, ts);

    assert_eq!(repo.mark_read_synced(&[(conv, peer, ts)], &me).unwrap(), vec![conv]);
    assert!(is_read(&repo, &msg), "the sync's own subject must be marked read");
}

#[test]
fn a_read_sync_never_marks_our_own_message_read() {
    let (_tmp, db) = setup();
    let repo = MessageRepository::new(db.clone());

    let me = UserId::new();
    let peer = UserId::new();
    let ts = 1_700_000_000_000_000_001u64;
    let conv = ChannelId::from(peer);
    // An outbound message that happens to share the timestamp. We are the
    // sender; "unread" is not a state it can be in for us.
    let mine = unread_from(&repo, conv, ConversationType::ServerDm, me, ts);

    assert!(repo.mark_read_synced(&[(conv, me, ts)], &me).unwrap().is_empty());
    assert!(!is_read(&repo, &mine), "our own message was flipped by a read sync");
}

#[test]
fn a_read_sync_never_touches_another_conversation() {
    let (_tmp, db) = setup();
    let repo = MessageRepository::new(db.clone());

    let me = UserId::new();
    let peer = UserId::new();
    let ts = 1_700_000_000_000_000_002u64;

    let channel_msg = unread_from(&repo, ChannelId::new(), ConversationType::Channel, peer, ts);
    let conv = ChannelId::from(peer);
    let dm_msg = unread_from(&repo, conv, ConversationType::ServerDm, peer, ts);

    assert_eq!(repo.mark_read_synced(&[(conv, peer, ts)], &me).unwrap(), vec![conv]);
    assert!(is_read(&repo, &dm_msg));
    assert!(
        !is_read(&repo, &channel_msg),
        "same author and timestamp in another conversation must stay unread"
    );
}

#[test]
fn a_read_sync_never_touches_a_mesh_message() {
    let (_tmp, db) = setup();
    let repo = MessageRepository::new(db.clone());

    let me = UserId::new();
    let peer = UserId::new();
    // Mesh sent_at is milliseconds widened to nanoseconds (v061), so this is
    // exactly the collision class that migration was written for. Same
    // conversation id, author and time: only the type tells them apart.
    let ts = 1_700_000_000_000_000_000u64;
    let conv = ChannelId::from(peer);

    let mesh_msg = unread_from(&repo, conv, ConversationType::Mesh, peer, ts);
    let mesh_dm = unread_from(&repo, conv, ConversationType::MeshDm, peer, ts);
    let dm_msg = unread_from(&repo, conv, ConversationType::ServerDm, peer, ts);

    assert_eq!(repo.mark_read_synced(&[(conv, peer, ts)], &me).unwrap(), vec![conv]);
    assert!(is_read(&repo, &dm_msg));
    assert!(!is_read(&repo, &mesh_msg), "mesh row flipped by a read sync");
    assert!(!is_read(&repo, &mesh_dm), "mesh DM row flipped by a read sync");
}

#[test]
fn an_already_read_message_is_not_reported_as_newly_read() {
    let (_tmp, db) = setup();
    let repo = MessageRepository::new(db.clone());

    let me = UserId::new();
    let peer = UserId::new();
    let ts = 1_700_000_000_000_000_003u64;
    let conv = ChannelId::from(peer);
    let msg = unread_from(&repo, conv, ConversationType::ServerDm, peer, ts);

    assert_eq!(repo.mark_read_synced(&[(conv, peer, ts)], &me).unwrap(), vec![conv]);
    assert!(
        repo.mark_read_synced(&[(conv, peer, ts)], &me).unwrap().is_empty(),
        "a duplicate sync must not report a second state change"
    );
    assert!(is_read(&repo, &msg));
}

/// What a local read names in its `ReadSync`: exactly the rows it flipped,
/// by author and sent time — not our own, not ones already read.
#[test]
fn a_local_read_returns_what_it_flipped() {
    let (_tmp, db) = setup();
    let repo = MessageRepository::new(db.clone());

    let me = UserId::new();
    let peer = UserId::new();
    let conv = ChannelId::from(peer);
    let first = unread_from(&repo, conv, ConversationType::ServerDm, peer, 10);
    unread_from(&repo, conv, ConversationType::ServerDm, peer, 11);
    let mut mine = Message::new_text(conv, ConversationType::ServerDm, Some(me), "mine".to_string(), 12);
    mine.is_read = true;
    repo.insert(&mine).unwrap();
    repo.mark_read(&first.id).unwrap();

    let (anchor, read) = repo.mark_conversation_read_returning(&conv, &ConversationType::ServerDm).unwrap();
    assert_eq!(anchor.unread_count, 1);
    assert_eq!(read, vec![(peer, 11)]);
    assert!(repo
        .mark_conversation_read_returning(&conv, &ConversationType::ServerDm)
        .unwrap()
        .1
        .is_empty());
}
