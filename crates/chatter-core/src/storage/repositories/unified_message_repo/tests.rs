use super::*;
use crate::domain::message::{EncryptedContent, EncryptedMessageType};

#[test]
fn delivery_receipt_batch_rolls_back_when_a_later_update_fails() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Database::open(&dir.path().join("receipts.db"), "fixture-only").unwrap());
    db.run_migrations().unwrap();
    let repo = UnifiedMessageRepository::new(db.clone());
    let conversation = ChannelId::new();
    let kind = ConversationType::Channel;
    for sent_at in [100, 200] {
        repo.insert(&Message::new_text(
            conversation,
            kind.clone(),
            Some(UserId::new()),
            "receipt".into(),
            sent_at,
        ))
        .unwrap();
    }
    let original: String = db
        .read_connection()
        .unwrap()
        .query_row("SELECT delivery_status FROM messages WHERE sent_at=100", [], |row| row.get(0))
        .unwrap();
    db.connection()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_second_receipt BEFORE UPDATE OF delivery_status ON messages
             WHEN NEW.sent_at=200 BEGIN SELECT RAISE(ABORT, 'receipt failure'); END;",
        )
        .unwrap();

    assert!(repo
        .update_delivery_status_by_timestamps(&conversation, &kind, &[100, 200], "delivered")
        .is_err());
    let after: String = db
        .read_connection()
        .unwrap()
        .query_row("SELECT delivery_status FROM messages WHERE sent_at=100", [], |row| row.get(0))
        .unwrap();
    assert_eq!(after, original);
}

#[test]
fn corrupt_message_row_does_not_hide_good_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Database::open(&dir.path().join("rows.db"), "fixture-only").unwrap());
    db.run_migrations().unwrap();
    let repo = UnifiedMessageRepository::new(db.clone());
    for kind in [
        ConversationType::Channel,
        ConversationType::ServerDm,
        ConversationType::AnonDm,
        ConversationType::AnonGroup,
        ConversationType::SavedMessages,
    ] {
        let conversation = ChannelId::new();
        let good = Message::new_text(
            conversation,
            kind.clone(),
            Some(UserId::new()),
            "good".into(),
            100,
        );
        let bad = Message::new_text(
            conversation,
            kind.clone(),
            Some(UserId::new()),
            "bad".into(),
            200,
        );
        repo.insert(&good).unwrap();
        repo.insert(&bad).unwrap();
        for column in [
            "attachments_blob",
            "reaction_counts_blob",
            "current_user_reactions_blob",
            "mentioned_integrations_blob",
            "content_json",
        ] {
            let conn = db.connection().unwrap();
            if column == "content_json" {
                conn.execute(
                    "UPDATE messages SET content_json = '{\"Unknown\":null}' WHERE id = ?1",
                    [bad.id.to_string()],
                )
                .unwrap();
            } else {
                // Trailing bytes reproduce RC114 without decoding any old format.
                conn.execute(
                    &format!("UPDATE messages SET {column} = X'0001' WHERE id = ?1"),
                    [bad.id.to_string()],
                )
                .unwrap();
            }
            drop(conn);
            assert!(repo.get(&bad.id).is_err());
            assert_eq!(
                repo.get_latest(&conversation, &kind).unwrap().unwrap().id,
                good.id
            );
            assert_eq!(
                repo.list(&conversation, &kind, None, 50)
                    .unwrap()
                    .iter()
                    .map(|m| m.id)
                    .collect::<Vec<_>>(),
                vec![good.id]
            );
            assert_eq!(
                repo.list_by_conversation_type(&kind)
                    .unwrap()
                    .iter()
                    .map(|m| m.id)
                    .collect::<Vec<_>>(),
                vec![good.id]
            );
            db.connection()
                .unwrap()
                .execute("DELETE FROM messages WHERE id = ?1", [bad.id.to_string()])
                .unwrap();
            repo.insert(&bad).unwrap();
        }
    }
}

#[test]
fn pin_added_order_survives_duplicate_pin_and_message_edits() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Database::open(&dir.path().join("pins.db"), "fixture-only").unwrap());
    db.run_migrations().unwrap();
    let repo = UnifiedMessageRepository::new(db.clone());
    for kind in [
        ConversationType::Channel,
        ConversationType::ServerDm,
        ConversationType::AnonDm,
        ConversationType::AnonGroup,
        ConversationType::SavedMessages,
    ] {
        let conversation = ChannelId::new();
        let newer_message = Message::new_text(
            conversation,
            kind.clone(),
            Some(UserId::new()),
            "newer original".into(),
            200,
        );
        let older_message = Message::new_text(
            conversation,
            kind.clone(),
            Some(UserId::new()),
            "older original".into(),
            100,
        );
        repo.insert(&newer_message).unwrap();
        repo.insert(&older_message).unwrap();
        repo.pin(&newer_message.id).unwrap();
        repo.set_pinned_by_timestamp(&conversation, &kind, 100, true)
            .unwrap();
        let order = || {
            repo.list_pinned(&conversation, &kind)
                .unwrap()
                .into_iter()
                .map(|m| m.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(order(), vec![older_message.id, newer_message.id]);
        let clock = |id: MessageId| {
            db.read_connection()
                .unwrap()
                .query_row(
                    "SELECT pinned_at FROM messages WHERE id=?1",
                    [id.to_string()],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap()
        };
        let original_clock = clock(newer_message.id);
        repo.pin(&newer_message.id).unwrap();
        let pinned = repo.get(&newer_message.id).unwrap().unwrap();
        repo.update(&pinned).unwrap();
        assert_eq!(clock(newer_message.id), original_clock);
        assert_eq!(order(), vec![older_message.id, newer_message.id]);
        repo.unpin(&newer_message.id).unwrap();
        repo.pin(&newer_message.id).unwrap();
        assert_eq!(order(), vec![newer_message.id, older_message.id]);
    }
}

#[test]
fn stored_timestamp_quote_resolves_when_parent_arrives_in_same_conversation() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE messages (id TEXT, sender_id TEXT, conversation_id TEXT, conversation_type TEXT, sent_at INTEGER)").unwrap();
    let author = UserId::new();
    let other_author = UserId::new();
    let conversation = ChannelId::new();
    let other_conversation = ChannelId::new();
    let parent = MessageId::new();
    let reference = MessageId::from_timestamp(123456789);
    let kind = ConversationType::Channel;
    let resolve =
        || resolve_timestamp_quote(&conn, &reference, &author, &conversation, &kind).unwrap();
    assert_eq!(resolve(), None);
    for (sender, channel) in [(other_author, conversation), (author, other_conversation)] {
        conn.execute(
            "INSERT INTO messages VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                MessageId::new().to_string(),
                sender.to_string(),
                channel.to_string(),
                kind.as_str(),
                123456789i64
            ],
        )
        .unwrap();
    }
    assert_eq!(resolve(), None);
    conn.execute(
        "INSERT INTO messages VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            parent.to_string(),
            author.to_string(),
            conversation.to_string(),
            kind.as_str(),
            123456789i64
        ],
    )
    .unwrap();
    assert_eq!(resolve(), Some(parent));
    assert_eq!(
        resolve_timestamp_quote(
            &conn,
            &MessageId::from_timestamp(99),
            &author,
            &conversation,
            &kind
        )
        .unwrap(),
        None
    );
}

#[test]
fn test_content_type_discriminant_covers_all_variants() {
    // Ensure every MessageContent variant has a discriminant
    let text = MessageContent::Text("hello".to_string());
    assert_eq!(content_type_discriminant(&text), 0);

    let encrypted = MessageContent::Encrypted(EncryptedContent {
        ciphertext: vec![1, 2, 3],
        sender_key_id: None,
        message_type: EncryptedMessageType::Signal,
    });
    assert_eq!(content_type_discriminant(&encrypted), 1);
}

/// Deleting a server DM (singly or batched) remembers its
/// (conversation, sender, sent_at) for the signed-in account only; other
/// conversation types, senders and times, and other accounts, are not held.
#[test]
fn deleted_server_dm_is_tombstoned_by_sender_and_time_for_its_account() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Database::open(&dir.path().join("dm-tomb.db"), "testpass").unwrap());
    db.run_migrations().unwrap();
    let repo = UnifiedMessageRepository::new(db.clone());
    let set_owner = |owner: UserId| {
        db.connection()
            .unwrap()
            .execute(
                "INSERT OR REPLACE INTO settings(key,value,updated_at) VALUES(?1,?2,'now')",
                params![
                    CURRENT_USER_KEY,
                    serde_json::to_string(&owner.to_string()).unwrap()
                ],
            )
            .unwrap();
    };
    let (me, other_account, peer) = (UserId::new(), UserId::new(), UserId::new());
    set_owner(me);
    let conversation = ChannelId::new();
    let dm = |sent_at| {
        Message::new_text(
            conversation,
            ConversationType::ServerDm,
            Some(peer),
            "x".into(),
            sent_at,
        )
    };
    let (one, two) = (dm(100), dm(200));
    let channel_post = Message::new_text(
        conversation,
        ConversationType::Channel,
        Some(peer),
        "x".into(),
        300,
    );
    for m in [&one, &two, &channel_post] {
        repo.insert(m).unwrap();
    }
    repo.delete(&one.id).unwrap();
    repo.delete_batch(&[two.id, channel_post.id]).unwrap();
    let held = |sender: &UserId, sent_at| {
        repo.is_dm_receipt_tombstoned(&conversation, sender, sent_at)
            .unwrap()
    };
    assert!(held(&peer, 100));
    assert!(held(&peer, 200));
    assert!(!held(&peer, 300), "only server DMs");
    assert!(!held(&peer, 101));
    assert!(!held(&me, 100));
    assert!(
        !repo
            .is_dm_receipt_tombstoned(&ChannelId::new(), &peer, 100)
            .unwrap()
    );
    set_owner(other_account);
    assert!(!held(&peer, 100), "account scoped");
}

#[test]
fn single_reaction_replaces_and_rejects_delayed_events_in_every_scope() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Database::open(&dir.path().join("one-reaction.db"), "testpass").unwrap());
    db.run_migrations().unwrap();
    let repo = UnifiedMessageRepository::new(db);
    let actor = UserId::new();
    let peer = UserId::new();
    let author = UserId::new();
    for kind in [
        ConversationType::ServerDm,
        ConversationType::Channel,
        ConversationType::AnonDm,
        ConversationType::AnonGroup,
    ] {
        let msg = Message::new_text(ChannelId::new(), kind, Some(author), "keep".into(), 100);
        repo.insert(&msg).unwrap();
        let scope = ReactionScope::of(&msg);
        assert!(
            repo.add_reaction_at(&scope, &author, msg.sent_at, &actor, &actor, "👍", 10)
                .unwrap()
        );
        assert!(
            repo.add_reaction_at(&scope, &author, msg.sent_at, &peer, &actor, "👍", 11)
                .unwrap()
        );
        assert!(
            repo.add_reaction_at(&scope, &author, msg.sent_at, &actor, &actor, "❤️", 20)
                .unwrap()
        );
        let saved = repo
            .get_by_reaction_scope(&scope, &author, msg.sent_at)
            .unwrap()
            .unwrap();
        assert_eq!(saved.current_user_reactions, vec!["❤️"]);
        assert_eq!(
            saved.reaction_counts.iter().map(|r| r.count).sum::<u32>(),
            2
        );
        assert!(
            !repo
                .add_reaction_at(&scope, &author, msg.sent_at, &actor, &actor, "👍", 15)
                .unwrap()
        );
        assert!(
            !repo
                .remove_reaction_at(&scope, &author, msg.sent_at, &actor, &actor, "👍", 21)
                .unwrap()
        );
        assert_eq!(
            repo.get_by_reaction_scope(&scope, &author, msg.sent_at)
                .unwrap()
                .unwrap()
                .current_user_reactions,
            vec!["❤️"]
        );
        assert!(
            repo.remove_reaction_at(&scope, &author, msg.sent_at, &actor, &actor, "❤️", 30)
                .unwrap()
        );
        assert!(
            !repo
                .add_reaction_at(&scope, &author, msg.sent_at, &actor, &actor, "😂", 15)
                .unwrap()
        );
        assert!(
            repo.add_reaction_at(&scope, &author, msg.sent_at, &actor, &actor, "😂", 40)
                .unwrap()
        );
        let saved = repo
            .get_by_reaction_scope(&scope, &author, msg.sent_at)
            .unwrap()
            .unwrap();
        assert_eq!(saved.current_user_reactions, vec!["😂"]);
        assert_eq!(
            saved.reaction_counts.iter().map(|r| r.count).sum::<u32>(),
            2
        );
    }
}

/// §112w: a row read before a reaction landed (or rebuilt from a server
/// proto with no reactions) and written back with `update` must not put
/// its stale counts over the vote table; what a reload reads stays the
/// authoritative count.
#[test]
fn update_with_a_stale_row_keeps_the_reaction_projection() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Database::open(&dir.path().join("stale.db"), "testpass").unwrap());
    db.run_migrations().unwrap();
    let repo = UnifiedMessageRepository::new(db);
    let (me, peer, author) = (UserId::new(), UserId::new(), UserId::new());
    let msg = Message::new_text(
        ChannelId::new(),
        ConversationType::Channel,
        Some(author),
        "hi".into(),
        100,
    );
    repo.insert(&msg).unwrap();
    let scope = ReactionScope::of(&msg);
    let mut stale = repo.get(&msg.id).unwrap().unwrap();
    repo.add_reaction_at(&scope, &author, 100, &me, &me, "👍", 10)
        .unwrap();
    repo.add_reaction_at(&scope, &author, 100, &peer, &me, "👍", 11)
        .unwrap();

    // Drifted the other way too: a caller holding inflated counts.
    stale.reaction_counts = vec![ReactionCount::new("👍".into(), 7)];
    stale.is_pinned = true;
    repo.update(&stale).unwrap();
    let stored = repo.get(&msg.id).unwrap().unwrap();
    assert!(stored.is_pinned, "the update itself applied");
    assert_eq!(
        stored.reaction_counts,
        vec![ReactionCount::new("👍".into(), 2)]
    );
    assert_eq!(stored.current_user_reactions, vec!["👍"]);

    // An emptied row (server proto) does not wipe them either.
    let mut emptied = stored.clone();
    emptied.reaction_counts.clear();
    emptied.current_user_reactions.clear();
    repo.update(&emptied).unwrap();
    repo.remove_reaction_at(&scope, &author, 100, &me, &me, "👍", 20)
        .unwrap();
    let stored = repo.get(&msg.id).unwrap().unwrap();
    assert_eq!(
        stored.reaction_counts,
        vec![ReactionCount::new("👍".into(), 1)]
    );
    assert!(stored.current_user_reactions.is_empty());
}

#[test]
fn single_reaction_delayed_remove_of_previous_emoji_preserves_new_selection() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Database::open(&dir.path().join("reordered.db"), "testpass").unwrap());
    db.run_migrations().unwrap();
    let repo = UnifiedMessageRepository::new(db);
    let actor = UserId::new();
    for remove_first in [true, false] {
        let msg = Message::new_text(
            ChannelId::new(),
            ConversationType::ServerDm,
            Some(actor),
            "order".into(),
            100,
        );
        repo.insert(&msg).unwrap();
        let scope = ReactionScope::of(&msg);
        repo.add_reaction_at(&scope, &actor, msg.sent_at, &actor, &actor, "👍", 10)
            .unwrap();
        if remove_first {
            repo.remove_reaction_at(&scope, &actor, msg.sent_at, &actor, &actor, "👍", 30)
                .unwrap();
        }
        assert!(
            repo.add_reaction_at(&scope, &actor, msg.sent_at, &actor, &actor, "❤️", 20)
                .unwrap()
        );
        if !remove_first {
            repo.remove_reaction_at(&scope, &actor, msg.sent_at, &actor, &actor, "👍", 30)
                .unwrap();
        }
        assert_eq!(
            repo.get_by_reaction_scope(&scope, &actor, msg.sent_at)
                .unwrap()
                .unwrap()
                .current_user_reactions,
            vec!["❤️"]
        );
        repo.remove_reaction_at(&scope, &actor, msg.sent_at, &actor, &actor, "❤️", 40)
            .unwrap();
        assert!(
            !repo
                .add_reaction_at(&scope, &actor, msg.sent_at, &actor, &actor, "❤️", 35)
                .unwrap()
        );
        assert!(
            !repo
                .add_reaction_at(&scope, &actor, msg.sent_at, &actor, &actor, "👍", 15)
                .unwrap()
        );
    }
}

#[test]
fn test_like_pattern_escaping() {
    // LIKE special chars are escaped so they match literally
    let query = "100% done_now";
    let pattern = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
    assert_eq!(pattern, "%100\\% done\\_now%");
}
