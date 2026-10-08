//! Connected storage/action regression without client, network or keyring setup.
use super::*;
use crate::domain::channel::{Channel, ChannelType};
use crate::domain::message::Message;
use crate::domain::reaction::ReactionScope;
use crate::storage::{Database, MessageRepository};

#[test]
fn saved_lookup_menu_and_local_delete_preserve_colliding_channel_and_dm() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Database::open(&dir.path().join("saved.db"), "test-password").unwrap());
    db.run_migrations().unwrap();
    let repo = MessageRepository::new(db);
    let own = UserId::new();
    let id = ChannelId::from(own);
    let peer = UserId::new();
    let other_id = ChannelId::from(peer);
    let now = crate::util::time::TimestampNanos::now().as_u64();
    let channel = Channel::new(id, "Saved Messages".into(), ChannelType::SavedMessages, own);
    let mut records = Vec::new();
    for (record_id, kind, body) in [
        (id, ConversationType::SavedMessages, "saved"),
        (id, ConversationType::Channel, "same-id channel"),
        (id, ConversationType::ServerDm, "same-id dm"),
        (other_id, ConversationType::Channel, "normal channel"),
        (other_id, ConversationType::ServerDm, "normal dm"),
    ] {
        let mut message = Message::new_text(record_id, kind, Some(own), body.into(), now);
        message.delivery_status = "delivered".into();
        repo.insert(&message).unwrap();
        records.push(message);
    }
    let scope =
        message_storage_scope(&ActiveConversation::Channel(id), Some(own), Some(&channel)).unwrap();
    assert_eq!(scope, ReactionScope::of(&records[0]));
    let saved = scoped_stored_message(&repo, &scope, &own, now).unwrap();
    assert_eq!(saved.id, records[0].id);
    assert_eq!(saved.as_text(), Some("saved")); // copy reads this exact record
    let menu = stored_message_menu(&saved, true, now);
    assert!(menu.actions.copy && menu.actions.select && menu.actions.delete_for_me);
    assert!(!menu.actions.delete_for_everyone && !menu.actions.resend);
    assert!(menu.actions.edit && menu.actions.pin && !menu.actions.react);
    assert!(!menu.pinned);
    assert!(require_remote_message_kind(&saved.conversation_type).is_err());
    // Exactly the write `pin_saved_message` makes: the whole row back with
    // `is_pinned` flipped; `pinned_at` orders the list and is cleared by unpin.
    let pinned = pinned_saved_record(&saved, own, true).unwrap();
    assert!(pinned.is_pinned && pinned.id == saved.id && pinned.content == saved.content);
    assert!(pinned_saved_record(&saved, peer, true).is_err());
    assert!(pinned_saved_record(&records[1], own, true).is_err());
    repo.update(&pinned).unwrap();
    let listed = repo
        .list_pinned_all(&id, &ConversationType::SavedMessages)
        .unwrap();
    assert_eq!(
        listed.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![saved.id]
    );
    assert!(
        repo.list_pinned_all(&id, &ConversationType::Channel)
            .unwrap()
            .is_empty()
    );
    let stored = scoped_stored_message(&repo, &scope, &own, now).unwrap();
    assert!(stored_message_menu(&stored, true, now).pinned);
    repo.update(&pinned_saved_record(&stored, own, false).unwrap())
        .unwrap();
    assert!(
        repo.list_pinned_all(&id, &ConversationType::SavedMessages)
            .unwrap()
            .is_empty()
    );
    assert!(scoped_stored_message(&repo, &scope, &peer, now).is_err());
    assert!(scoped_stored_message(&repo, &scope, &own, now + 1).is_err());
    for (conversation, expected) in [
        (ActiveConversation::Channel(other_id), &records[3]),
        (ActiveConversation::ServerDm { peer_id: peer }, &records[4]),
    ] {
        let normal_scope = message_storage_scope(&conversation, Some(own), Some(&channel)).unwrap();
        let normal = scoped_stored_message(&repo, &normal_scope, &own, now).unwrap();
        assert_eq!(normal.id, expected.id);
        assert!(require_remote_message_kind(&normal.conversation_type).is_ok());
        assert!(
            stored_message_menu(&normal, true, now)
                .actions
                .delete_for_everyone
        );
    }
    // This is the exact repository deletion used by MessageService::delete_message_local.
    repo.delete(&saved.id).unwrap();
    assert!(scoped_stored_message(&repo, &scope, &own, now).is_err());
    for survivor in &records[1..] {
        assert_eq!(
            repo.get(&survivor.id).unwrap().unwrap().as_text(),
            survivor.as_text()
        );
    }
}

#[test]
fn saved_storage_requires_matching_own_identity_and_metadata() {
    let own = UserId::new();
    let id = ChannelId::from(own);
    let other = UserId::new();
    let saved = Channel::new(id, "Saved Messages".into(), ChannelType::SavedMessages, own);
    let public = Channel::new(id, "public".into(), ChannelType::Public, own);
    let wrong_id = Channel::new(
        ChannelId::from(other),
        "Saved Messages".into(),
        ChannelType::SavedMessages,
        other,
    );
    for (identity, metadata) in [
        (None, Some(&saved)),
        (Some(other), Some(&saved)),
        (Some(own), None),
        (Some(own), Some(&public)),
        (Some(own), Some(&wrong_id)),
    ] {
        assert_eq!(
            message_storage_scope(&ActiveConversation::Channel(id), identity, metadata)
                .unwrap()
                .kind,
            ConversationType::Channel
        );
    }
    assert_eq!(
        message_storage_scope(
            &ActiveConversation::ServerDm { peer_id: own },
            Some(own),
            Some(&saved)
        )
        .unwrap()
        .kind,
        ConversationType::ServerDm
    );
    assert!(message_storage_scope(&ActiveConversation::None, Some(own), Some(&saved)).is_err());
}

#[test]
fn saved_edit_rewrites_only_the_body_and_persists_the_whole_row() {
    use crate::domain::ids::{AttachmentId, MessageId};
    use crate::domain::message::{MessageContent, MixedContent, REMOTE_ACTION_WINDOW_NS};
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Database::open(&dir.path().join("saved.db"), "test-password").unwrap());
    db.run_migrations().unwrap();
    let repo = MessageRepository::new(db);
    let own = UserId::new();
    let other = UserId::new();
    let id = ChannelId::from(own);
    let now = crate::util::time::TimestampNanos::now().as_u64();
    // Older than any remote edit window: Saved notes are editable regardless.
    let sent = now - REMOTE_ACTION_WINDOW_NS * 3;
    let channel = Channel::new(id, "Saved Messages".into(), ChannelType::SavedMessages, own);
    let scope =
        message_storage_scope(&ActiveConversation::Channel(id), Some(own), Some(&channel)).unwrap();

    let mut saved = Message::new_text(
        id,
        ConversationType::SavedMessages,
        Some(own),
        "old".into(),
        sent,
    );
    saved.attachments = vec![AttachmentId::new()];
    saved.reply_to = Some(MessageId::new());
    saved.reply_to_author_id = Some(own);
    saved.reply_to_text = Some("quoted".into());
    repo.insert_outgoing(&saved).unwrap();
    // A same-id channel row with the same author/clock must stay untouched.
    let collider = Message::new_text(
        id,
        ConversationType::Channel,
        Some(own),
        "channel".into(),
        sent,
    );
    repo.insert(&collider).unwrap();

    assert!(stored_message_menu(&saved, true, now).actions.edit);
    let edited = edited_saved_record(&saved, own, "  new  ").unwrap();
    assert_eq!(edited.content, MessageContent::Text("new".into()));
    assert!(edited.is_edited && edited.edited_at.is_some());
    assert_eq!(
        (
            edited.id,
            edited.sent_at,
            &edited.attachments,
            edited.reply_to,
            edited.reply_to_author_id
        ),
        (
            saved.id,
            saved.sent_at,
            &saved.attachments,
            saved.reply_to,
            saved.reply_to_author_id
        )
    );
    // Exactly the write `edit_saved_message` makes.
    repo.update(&edited).unwrap();
    let stored = scoped_stored_message(&repo, &scope, &own, sent).unwrap();
    assert_eq!(stored.as_text(), Some("new"));
    assert!(stored.is_edited && stored.edited_at.is_some());
    assert_eq!(stored.attachments, saved.attachments);
    assert_eq!(stored.reply_to, saved.reply_to);
    assert_eq!(stored.reply_to_author_id, saved.reply_to_author_id);
    assert_eq!(stored.reply_to_text.as_deref(), Some("quoted"));
    assert_eq!(
        repo.get(&collider.id).unwrap().unwrap().as_text(),
        Some("channel")
    );

    // A Mixed body keeps its attachment ids and only its caption moves.
    let ids = vec![AttachmentId::new(), AttachmentId::new()];
    let mut mixed = saved.clone();
    mixed.content = MessageContent::Mixed(MixedContent::new("cap".into(), ids.clone()));
    assert_eq!(
        edited_saved_record(&mixed, own, "new cap").unwrap().content,
        MessageContent::Mixed(MixedContent {
            text: Some("new cap".into()),
            attachment_ids: ids,
        })
    );

    // Refused: empty text, another author, deleted, unsent, non-Saved kind.
    assert!(edited_saved_record(&saved, own, " \n").is_err());
    assert!(edited_saved_record(&saved, other, "x").is_err());
    let mut deleted = saved.clone();
    deleted.is_deleted = true;
    assert!(edited_saved_record(&deleted, own, "x").is_err());
    let mut failed = saved.clone();
    failed.delivery_status = "failed".into();
    assert!(!stored_message_menu(&failed, true, now).actions.edit);
    assert!(edited_saved_record(&failed, own, "x").is_err());
    assert!(edited_saved_record(&collider, own, "x").is_err());
}
