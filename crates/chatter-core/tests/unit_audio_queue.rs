//! Forward message scan backing the persistent audio player's autoplay.
//!
//! The player chains to the next voice note after the current one ends, so
//! `list_after_asc` has to be strictly forward-looking, chronological, and
//! blind to deleted messages. Those three properties fail silently in SQL.

use std::sync::Arc;

use chatter_core::domain::ids::{ChannelId, UserId};
use chatter_core::domain::message::{ConversationType, Message};
use chatter_core::storage::Database;
use chatter_core::storage::repositories::MessageRepository;

fn repo() -> (MessageRepository, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Database::open(&dir.path().join("audio_queue.db"), "test_passphrase").expect("open db");
    db.run_migrations().expect("migrations");
    (MessageRepository::new(Arc::new(db)), dir)
}

fn insert_at(repo: &MessageRepository, conv: &ChannelId, sent_at: u64, deleted: bool) -> Message {
    let mut msg = Message::new_text(
        conv.clone(),
        ConversationType::Channel,
        Some(UserId::new()),
        format!("m{sent_at}"),
        sent_at,
    );
    msg.is_deleted = deleted;
    repo.insert(&msg).expect("insert");
    msg
}

#[test]
fn list_after_asc_is_strictly_forward_ordered_and_skips_deleted() {
    let (repo, _dir) = repo();
    let conv = ChannelId::new();

    let anchor = insert_at(&repo, &conv, 100, false);
    let deleted = insert_at(&repo, &conv, 150, true);
    let next = insert_at(&repo, &conv, 200, false);
    let later = insert_at(&repo, &conv, 300, false);

    let found = repo
        .list_after_asc(&conv, &ConversationType::Channel, anchor.sent_at, 200)
        .expect("list_after_asc");
    let ids: Vec<_> = found.iter().map(|m| m.id.to_string()).collect();

    // Strictly after: the anchor itself must not come back, or the player
    // would replay the track that just ended in a loop.
    assert!(!ids.contains(&anchor.id.to_string()), "anchor leaked in: {ids:?}");
    assert!(!ids.contains(&deleted.id.to_string()), "deleted leaked in: {ids:?}");
    assert_eq!(
        ids,
        vec![next.id.to_string(), later.id.to_string()],
        "expected chronological order"
    );
}

#[test]
fn list_after_asc_is_scoped_to_its_conversation() {
    let (repo, _dir) = repo();
    let conv = ChannelId::new();
    let other = ChannelId::new();

    let anchor = insert_at(&repo, &conv, 100, false);
    insert_at(&repo, &other, 200, false);
    let same_conv = insert_at(&repo, &conv, 300, false);

    let found = repo
        .list_after_asc(&conv, &ConversationType::Channel, anchor.sent_at, 200)
        .expect("list_after_asc");

    assert_eq!(found.len(), 1, "other conversation bled in");
    assert_eq!(found[0].id.to_string(), same_conv.id.to_string());
}

#[test]
fn list_after_asc_honours_its_limit() {
    let (repo, _dir) = repo();
    let conv = ChannelId::new();

    let anchor = insert_at(&repo, &conv, 100, false);
    for t in [200, 300, 400, 500] {
        insert_at(&repo, &conv, t, false);
    }

    let found = repo
        .list_after_asc(&conv, &ConversationType::Channel, anchor.sent_at, 2)
        .expect("list_after_asc");

    assert_eq!(found.len(), 2, "scan window not applied");
    assert_eq!(found[0].sent_at, 200, "limit must keep the nearest messages");
}
