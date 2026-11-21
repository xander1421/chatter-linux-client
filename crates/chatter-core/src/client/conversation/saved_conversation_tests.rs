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
    assert!(scope.kind.uses_plain_delete_confirmation());
    let saved = scoped_stored_message(&repo, &scope, &own, now).unwrap();
    assert_eq!(saved.id, records[0].id);
    assert_eq!(saved.as_text(), Some("saved")); // copy reads this exact record
    let menu = stored_message_menu(&saved, true, now);
    assert!(menu.actions.copy && menu.actions.select && menu.actions.delete_for_me);
    assert!(!menu.actions.delete_for_everyone && !menu.actions.resend);
    assert!(menu.actions.edit && menu.actions.pin && menu.actions.react);
    assert!(!menu.pinned);
    assert!(require_remote_message_kind(&saved.conversation_type).is_err());
    // Exactly the write `pin_saved_message` makes: the pin columns at the
    // Saved key; `pinned_at` orders the list and is cleared by unpin. The
    // same-id channel and DM rows with the same author and clock are not it.
    let pinned = pinned_saved_record(&saved, own, true).unwrap();
    assert!(pinned.is_pinned && pinned.id == saved.id && pinned.content == saved.content);
    assert!(pinned_saved_record(&saved, peer, true).is_err());
    assert!(pinned_saved_record(&records[1], own, true).is_err());
    assert!(repo.set_pinned_scoped(&scope, &own, now, true).unwrap());
    assert!(!repo.set_pinned_scoped(&scope, &peer, now, true).unwrap());
    for untouched in &records[1..] {
        assert!(!repo.get(&untouched.id).unwrap().unwrap().is_pinned);
    }
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
    let unpinned = pinned_saved_record(&stored, own, false).unwrap();
    assert!(
        repo.set_pinned_scoped(&scope, &own, now, unpinned.is_pinned)
            .unwrap()
    );
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
        assert!(!normal_scope.kind.uses_plain_delete_confirmation());
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
fn saved_edit_rewrites_only_the_body_and_keeps_a_concurrent_pin() {
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
    // A pin landing between the edit's read and its write survives it: the
    // edit writes body columns only, at the Saved key.
    assert!(repo.set_pinned_scoped(&scope, &own, sent, true).unwrap());
    // Exactly the write `edit_saved_message` makes.
    assert!(
        repo.set_body_scoped(
            &scope,
            &own,
            sent,
            &edited.content,
            edited.edited_at.unwrap()
        )
        .unwrap()
    );
    assert!(
        !repo
            .set_body_scoped(
                &scope,
                &other,
                sent,
                &edited.content,
                edited.edited_at.unwrap()
            )
            .unwrap()
    );
    let stored = scoped_stored_message(&repo, &scope, &own, sent).unwrap();
    assert_eq!(stored.as_text(), Some("new"));
    assert!(stored.is_edited && stored.edited_at.is_some());
    assert!(stored.is_pinned);
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

#[test]
fn saved_reactions_persist_replace_and_remove_without_touching_colliding_scopes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("saved-reactions.db");
    let db = Arc::new(Database::open(&path, "test-password").unwrap());
    db.run_migrations().unwrap();
    let repo = MessageRepository::new(db);
    let own = UserId::new();
    let id = ChannelId::from(own);
    let now = crate::util::time::TimestampNanos::now().as_u64();
    let scope = ReactionScope::new(id, ConversationType::SavedMessages);
    let mut rows = Vec::new();
    for kind in [ConversationType::SavedMessages, ConversationType::Channel, ConversationType::ServerDm] {
        let mut row = Message::new_text(id, kind, Some(own), "note".into(), now);
        row.delivery_status = "delivered".into();
        repo.insert(&row).unwrap();
        rows.push(row);
    }
    let react = |emoji: &str, remove| super::reactions::persist_saved_reaction(
        &repo, &scope, own, &own, now, emoji, remove,
    );
    react("👍", false).unwrap();
    assert_eq!(scoped_stored_message(&repo, &scope, &own, now).unwrap().current_user_reactions, vec!["👍"]);
    react("❤️", false).unwrap();
    let stored = scoped_stored_message(&repo, &scope, &own, now).unwrap();
    assert_eq!(stored.current_user_reactions, vec!["❤️"]);
    assert_eq!(stored.reaction_counts.len(), 1);
    for row in &rows[1..] {
        let untouched = repo.get(&row.id).unwrap().unwrap();
        assert!(untouched.current_user_reactions.is_empty() && untouched.reaction_counts.is_empty());
    }
    // A fresh database handle sees the same persisted projection.
    let reopened = MessageRepository::new(Arc::new(Database::open(&path, "test-password").unwrap()));
    assert_eq!(scoped_stored_message(&reopened, &scope, &own, now).unwrap().current_user_reactions, vec!["❤️"]);
    react("❤️", true).unwrap();
    let stored = scoped_stored_message(&repo, &scope, &own, now).unwrap();
    assert!(stored.current_user_reactions.is_empty() && stored.reaction_counts.is_empty());
    assert!(super::reactions::persist_saved_reaction(&repo, &scope, UserId::new(), &own, now, "👍", false).is_err());
    assert!(super::reactions::persist_saved_reaction(&repo, &scope, own, &UserId::new(), now, "👍", false).is_err());
    assert!(super::reactions::persist_saved_reaction(&repo, &ReactionScope::of(&rows[1]), own, &own, now, "👍", false).is_err());
    assert!(react("", false).is_err());
    repo.delete(&rows[0].id).unwrap();
    assert!(react("👍", false).is_err());
}

#[tokio::test]
async fn saved_reaction_handle_resolves_saved_scope_and_toggles_without_transport() {
    let (client, _dir) = super::tests::test_client();
    let own = UserId::new();
    let id = ChannelId::from(own);
    client.auth_state().set_logged_in(own, 1, "reaction fixture".into());
    let channel = Channel::new(id, "Saved Messages".into(), ChannelType::SavedMessages, own);
    client.channels().channel_repo.insert(&channel).unwrap();
    let now = crate::util::time::TimestampNanos::now().as_u64();
    for kind in [ConversationType::SavedMessages, ConversationType::Channel, ConversationType::ServerDm] {
        let mut row = Message::new_text(id, kind, Some(own), "note".into(), now);
        row.delivery_status = "delivered".into();
        client.messages().message_repo.insert(&row).unwrap();
    }
    let handle = client.conversation(ActiveConversation::Channel(id));
    let snapshot = handle.reaction_snapshot(&own, now).unwrap().unwrap();
    assert_eq!(snapshot.scope.kind, ConversationType::SavedMessages);
    assert!(handle.toggle_reaction("👍", &own, now).await.unwrap());
    assert_eq!(handle.reaction_snapshot(&own, now).unwrap().unwrap().own_emojis, vec!["👍"]);
    assert!(!handle.toggle_reaction("👍", &own, now).await.unwrap());
    assert!(handle.reaction_snapshot(&own, now).unwrap().unwrap().own_emojis.is_empty());
    let foreign = ReactionScope::new(id, ConversationType::Channel);
    assert!(handle.toggle_reaction_for_scope(&foreign, "👍", &own, now).await.is_err());
}

#[tokio::test]
async fn saved_delete_recovery_queues_every_selected_note_after_failed_fanout() {
    use crate::storage::repositories::{SavedSyncRepository, settings_repo::keys};

    let (client, _dir) = super::tests::test_client();
    let owner = UserId::new();
    let channel_id = ChannelId::from(owner);
    client
        .auth_state()
        .set_logged_in(owner, 1, "fixture".into());
    let messages = client.messages();
    messages
        .settings_repo
        .set(keys::CURRENT_USER_ID, &owner.to_string())
        .unwrap();
    messages
        .settings_repo
        .set(keys::CURRENT_DEVICE_ID, &1u32)
        .unwrap();
    client
        .channels()
        .channel_repo
        .insert(&Channel::new(
            channel_id,
            "Saved Messages".into(),
            ChannelType::SavedMessages,
            owner,
        ))
        .unwrap();
    let notes: Vec<_> = [1, 2]
        .into_iter()
        .map(|sent_at| {
            let note = Message::new_text(
                channel_id,
                ConversationType::SavedMessages,
                Some(owner),
                "note".into(),
                sent_at,
            );
            messages.message_repo.insert(&note).unwrap();
            note
        })
        .collect();

    let (completed, _) = tokio::sync::mpsc::unbounded_channel();
    let result = client
        .conversation(ActiveConversation::Channel(channel_id))
        .delete_selected(
            &notes
                .iter()
                .map(|note| note.id.to_string())
                .collect::<Vec<_>>(),
            SelectedDeleteScope::ForMe,
            owner,
            client.auth_state().get().session_revision,
            completed,
        )
        .await;

    assert!(result.deleted_ids.is_empty());
    assert_eq!(result.failed_ids.len(), notes.len());
    let journal = SavedSyncRepository::new(messages.database.clone());
    for note in notes {
        assert_eq!(
            journal.pending_delete(&owner, &note.id).unwrap().unwrap().1,
            note.sent_at
        );
    }
}

#[tokio::test]
async fn saved_delete_recovery_publishes_the_persisted_id_to_the_ui() {
    use crate::inbox::events::InboxEvent;
    use crate::inbox::handlers::DomainEvent;
    use crate::storage::repositories::settings_repo::keys;

    let (client, _dir) = super::tests::test_client();
    let owner = UserId::new();
    let id = crate::domain::ids::MessageId::new();
    let messages = client.messages();
    messages
        .settings_repo
        .set(keys::CURRENT_USER_ID, &owner.to_string())
        .unwrap();
    let conversation = owner.to_string();
    let target = crate::services::inbox_processor::sync::persist_saved_delete_for_me_sync(
        &messages.message_repo,
        Some(owner),
        owner,
        &conversation,
        &conversation,
        1,
        &id.to_string(),
        true,
    )
    .unwrap();
    let mut ui_events = client.subscribe_events();
    client
        .inbox_event_processor()
        .process(&InboxEvent::SavedDeletionApplied {
            owner,
            message_id: target,
        })
        .await;
    assert!(matches!(
        ui_events.try_recv().unwrap(),
        DomainEvent::DeleteForMeSync {
            conversation_id, target_message_id, saved_deletion_persisted: true, ..
        } if conversation_id == ChannelId::from(owner) && target_message_id == id
    ));
}

#[tokio::test]
async fn saved_messages_sidebar_preview_tracks_visible_history() {
    use crate::domain::attachment::{Attachment, FileMetadata};
    use crate::domain::ids::AttachmentId;

    let (client, dir) = super::tests::test_client();
    let owner = UserId::new();
    let id = ChannelId::from(owner);
    client.auth_state().set_logged_in(owner, 1, "fixture".into());
    let repo = &client.messages().message_repo;
    let empty = (String::new(), String::new());
    assert_eq!(client.saved_messages_sidebar_preview(), empty);

    let note = Message::new_text(
        id,
        ConversationType::SavedMessages,
        Some(owner),
        "a note".into(),
        1,
    );
    repo.insert(&note).unwrap();
    let note_preview = (
        crate::domain::message::format_preview_body("", "a note", None),
        crate::ui::format::sidebar_time(note.created_at),
    );
    assert_eq!(client.saved_messages_sidebar_preview(), note_preview);

    let photo_id = AttachmentId::new();
    let photo = Attachment::new(
        photo_id,
        FileMetadata::new("pic.jpg".into(), "image/jpeg".into(), 10),
    );
    let path = dir.path().join("pic.enc");
    std::fs::write(&path, b"fixture").unwrap();
    client
        .attachments()
        .attachment_repo()
        .insert_with_file_path(&photo, &path)
        .unwrap();
    let photo_at = 86_400_000_000_001;
    let mut newest = Message::new_text(
        id,
        ConversationType::SavedMessages,
        Some(owner),
        String::new(),
        photo_at,
    );
    newest.attachments = vec![photo_id];
    repo.insert(&newest).unwrap();
    let collider = Message::new_text(
        id,
        ConversationType::ServerDm,
        Some(owner),
        "other chat".into(),
        photo_at + 1,
    );
    repo.insert(&collider).unwrap();
    let (preview, time) = client.saved_messages_sidebar_preview();
    assert!(preview.contains("Photo"), "{preview:?}");
    assert!(!preview.contains("You:"), "{preview:?}");
    assert_eq!(time, crate::ui::format::sidebar_time(newest.created_at));

    repo.soft_delete_batch(&[newest.id]).unwrap();
    assert_eq!(client.saved_messages_sidebar_preview(), note_preview);
    client
        .auth_state()
        .set_logged_in(UserId::new(), 1, "next account".into());
    assert_eq!(client.saved_messages_sidebar_preview(), empty);
    client.auth_state().set_logged_in(owner, 1, "fixture".into());
    client.auth_state().set_logged_out();
    assert_eq!(client.saved_messages_sidebar_preview(), empty);
    client.auth_state().set_logged_in(owner, 1, "fixture".into());
    repo.delete(&note.id).unwrap();
    assert_eq!(client.saved_messages_sidebar_preview(), empty);
}
