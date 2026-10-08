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
    assert!(menu.actions.edit && !menu.actions.pin && !menu.actions.react);
    assert!(require_remote_message_kind(&saved.conversation_type).is_err());
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
fn saved_edit_keeps_row_identity_attachments_and_mixed_ids() {
    use crate::domain::ids::AttachmentId;
    use crate::domain::message::{MessageContent, MixedContent};
    let own = UserId::new();
    let other = UserId::new();
    let id = ChannelId::from(own);
    let now = crate::util::time::TimestampNanos::now().as_u64();
    let mut saved = Message::new_text(
        id,
        ConversationType::SavedMessages,
        Some(own),
        "old".into(),
        now,
    );
    saved.attachments = vec![AttachmentId::new()];
    saved.reply_to = Some(crate::domain::ids::MessageId::new());
    saved.reply_to_author_id = Some(own);

    // Text row: body replaced, trimmed; row fields are not part of the content.
    assert_eq!(
        saved_edit_content(&saved, own, now, "  new  ").unwrap(),
        MessageContent::Text("new".into())
    );
    // Mixed row: only the text moves, attachment ids stay.
    let ids = vec![AttachmentId::new(), AttachmentId::new()];
    saved.content = MessageContent::Mixed(MixedContent::new("cap".into(), ids.clone()));
    assert_eq!(
        saved_edit_content(&saved, own, now, "new cap").unwrap(),
        MessageContent::Mixed(MixedContent {
            text: Some("new cap".into()),
            attachment_ids: ids,
        })
    );
    // Refused: empty text, another author, deleted, past the window, non-Saved kind.
    assert!(saved_edit_content(&saved, own, now, " \n").is_err());
    assert!(saved_edit_content(&saved, other, now, "x").is_err());
    let mut deleted = saved.clone();
    deleted.is_deleted = true;
    assert!(saved_edit_content(&deleted, own, now, "x").is_err());
    let late = now + crate::domain::message::REMOTE_ACTION_WINDOW_NS + 1;
    assert!(saved_edit_content(&saved, own, late, "x").is_err());
    let mut channel = saved.clone();
    channel.conversation_type = ConversationType::Channel;
    assert!(saved_edit_content(&channel, own, now, "x").is_err());
}
