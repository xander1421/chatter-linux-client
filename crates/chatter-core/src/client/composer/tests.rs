use std::sync::Arc;

use super::*;
use crate::domain::ids::{AttachmentId, ChannelId};

fn client() -> (Arc<ChatterClient>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    // Persistent opt-out beside the database, before anything opens it.
    let db = crate::system::Directories::with_base_dir(dir.path().to_path_buf())
        .unwrap()
        .database_path();
    std::fs::write(
        db.parent()
            .unwrap()
            .join(crate::system::keyring::NO_KEYRING_MARKER),
        b"",
    )
    .unwrap();
    let client = client_in(&dir);
    client
        .auth_state()
        .set_logged_in(crate::domain::ids::UserId::new(), 1, "Test user".into());
    (client, dir)
}

/// A fresh process over the same data directory.
fn client_in(dir: &tempfile::TempDir) -> Arc<ChatterClient> {
    ChatterClient::builder()
        .server_url("https://localhost:1")
        .data_dir(dir.path().to_str().unwrap())
        .build()
        .expect("build client")
}

fn staged(name: &str) -> PendingAttachment {
    PendingAttachment::new(
        AttachmentId::new(),
        name.into(),
        Default::default(),
        1,
        "image/png".into(),
    )
}

/// Navigate as the app does: core state, then the UI's draft swap.
fn open(client: &ChatterClient, id: ChannelId) {
    client
        .chat_state()
        .set_active_conversation(ActiveConversation::Channel(id));
    client.swap_composer_draft("", ComposerDraft::default(), &id.to_string());
}

#[tokio::test]
async fn begin_freezes_the_composer_and_takes_what_is_staged() {
    let (client, _dir) = client();
    let a = ChannelId::new();
    open(&client, a);
    client
        .chat_state()
        .add_pending_attachment(staged("cat.png"));
    let send = client.begin_composer_send("caption".into(), None).unwrap();
    assert_eq!(send.conversation, ActiveConversation::Channel(a));
    assert_eq!(send.text, "caption");
    assert_eq!(send.attachments.len(), 1);
    assert!(
        client.chat_state().get_pending_attachments().is_empty(),
        "the staged file went to two sends"
    );

    // Later navigation and staging do not reach into the frozen send.
    open(&client, ChannelId::new());
    client
        .chat_state()
        .add_pending_attachment(staged("later.png"));
    assert_eq!(send.conversation, ActiveConversation::Channel(a));
    assert_eq!(send.attachments[0].filename, "cat.png");
}

#[tokio::test]
async fn an_expired_session_cannot_send_restore_or_recover_its_caption() {
    let (client, _dir) = client();
    let user = crate::domain::ids::UserId::new();
    client.auth_state().set_logged_in(user, 1, "A".into());
    let channel = ChannelId::new();
    open(&client, channel);
    let send = client
        .begin_composer_send("private caption".into(), None)
        .unwrap();
    assert!(client.recover_composer_send(send.clone(), false).is_none());

    // Sign-out as `logout` does it: the session's composer state goes
    // first. Even another login of the same account is a new session.
    client.clear_composer_session();
    client.auth_state().set_logged_out();
    client.auth_state().set_logged_in(user, 1, "A again".into());
    assert!(!client.composer_session_is_current(send.session_revision));
    assert!(
        client.send_composer_message(&send, None).await.is_none(),
        "expired intent entered the send path"
    );
    assert!(client.recover_composer_send(send, true).is_none());
    assert!(
        client.take_parked_send(true).is_none(),
        "old caption restored in a new session"
    );
}

#[tokio::test]
async fn sign_out_clears_account_local_composer_state() {
    let (client, _dir) = client();
    let channel = ChannelId::new();
    open(&client, channel);
    let send = client
        .begin_composer_send("failed caption".into(), None)
        .unwrap();
    assert!(client.recover_composer_send(send.clone(), false).is_none());
    client
        .chat_state()
        .add_pending_attachment(staged("private.png"));
    client.swap_composer_draft(
        &channel.to_string(),
        ComposerDraft {
            text: "private draft".into(),
            reply: None,
        },
        "elsewhere",
    );

    client.clear_composer_session();
    assert_eq!(
        client.chat_state().active_conversation(),
        ActiveConversation::None
    );
    assert!(client.chat_state().get_pending_attachments().is_empty());
    assert!(!client.composer_session_is_current(send.session_revision));
    client.swap_composer_draft(
        &channel.to_string(),
        ComposerDraft {
            text: "late old UI draft".into(),
            reply: None,
        },
        "",
    );
    client
        .auth_state()
        .set_logged_in(crate::domain::ids::UserId::new(), 1, "B".into());
    open(&client, channel);
    assert!(
        client
            .swap_composer_draft("", ComposerDraft::default(), &channel.to_string())
            .is_empty()
    );
    assert!(client.take_parked_send(true).is_none());
}

#[tokio::test]
async fn begin_with_no_conversation_consumes_nothing() {
    let (client, _dir) = client();
    client
        .chat_state()
        .add_pending_attachment(staged("cat.png"));
    assert!(client.begin_composer_send("x".into(), None).is_err());
    assert_eq!(client.chat_state().get_pending_attachments().len(), 1);
}

#[cfg(feature = "jarvis")]
#[tokio::test]
async fn begin_refuses_a_jarvis_request_before_consuming() {
    let (client, _dir) = client();
    open(&client, ChannelId::new());
    client
        .chat_state()
        .add_pending_attachment(staged("cat.png"));
    assert!(client.begin_composer_send("/ask why".into(), None).is_err());
    assert_eq!(client.chat_state().get_pending_attachments().len(), 1);
}

#[tokio::test]
async fn a_failure_is_restored_only_into_its_own_empty_composer() {
    let (client, _dir) = client();
    let (a, b) = (ChannelId::new(), ChannelId::new());
    open(&client, a);
    let send = client.begin_composer_send("caption".into(), None).unwrap();

    // Same chat, newer draft typed: kept, not written over it.
    assert!(client.recover_composer_send(send.clone(), false).is_none());
    assert!(
        client.take_parked_send(false).is_none(),
        "newer draft still there"
    );

    // Navigated to B: B's empty composer is no home for A's send.
    open(&client, b);
    assert!(
        client.take_parked_send(true).is_none(),
        "A's send restored into B"
    );

    // Back in A, composer empty but a file staged since: still kept.
    open(&client, a);
    client
        .chat_state()
        .add_pending_attachment(staged("new.png"));
    assert!(client.take_parked_send(true).is_none());
    client.chat_state().clear_pending_attachments();

    let back = client
        .take_parked_send(true)
        .expect("A's composer has room now");
    assert_eq!(back.text, "caption");
    assert!(client.take_parked_send(true).is_none(), "restored twice");

    // Failing while its own composer is empty restores at once.
    assert!(client.recover_composer_send(back, true).is_some());
}

#[tokio::test]
async fn a_failure_while_elsewhere_waits_for_its_conversation() {
    let (client, _dir) = client();
    let (a, b) = (ChannelId::new(), ChannelId::new());
    open(&client, a);
    let send = client.begin_composer_send("for A".into(), None).unwrap();
    open(&client, b);
    assert!(
        client.recover_composer_send(send, true).is_none(),
        "restored into B"
    );
    open(&client, a);
    assert_eq!(client.take_parked_send(true).unwrap().text, "for A");
}

#[tokio::test]
async fn staged_files_are_taken_only_for_the_conversation_still_open() {
    let (client, _dir) = client();
    let (a, b) = (ChannelId::new(), ChannelId::new());
    open(&client, b);
    client.chat_state().add_pending_attachment(staged("b.png"));
    assert!(
        client
            .chat_state()
            .take_pending_attachments_for(
                ActiveConversation::Channel(a),
                client.chat_state().get().attachment_revision,
            )
            .is_none()
    );
    assert_eq!(
        client.chat_state().get_pending_attachments().len(),
        1,
        "B's file was taken for A"
    );
}

#[tokio::test]
async fn begin_mid_switch_refuses_before_consuming() {
    let (client, _dir) = client();
    let (a, b) = (ChannelId::new(), ChannelId::new());
    open(&client, a);
    // Core has moved to B; the composer still holds A's draft.
    client
        .chat_state()
        .set_active_conversation(ActiveConversation::Channel(b));
    client.chat_state().add_pending_attachment(staged("a.png"));
    assert!(client.begin_composer_send("for A".into(), None).is_err());
    assert_eq!(client.chat_state().get_pending_attachments().len(), 1);
}

#[tokio::test]
async fn a_failure_mid_switch_is_kept_not_restored_into_the_old_composer() {
    let (client, _dir) = client();
    let (a, b) = (ChannelId::new(), ChannelId::new());
    open(&client, a);
    let send = client.begin_composer_send("for A".into(), None).unwrap();
    open(&client, b);
    // Back to A in core; the composer still shows B's (empty) draft.
    client
        .chat_state()
        .set_active_conversation(ActiveConversation::Channel(a));
    assert!(
        client.recover_composer_send(send, true).is_none(),
        "A's send restored into B's composer"
    );
    client.swap_composer_draft(&b.to_string(), ComposerDraft::default(), &a.to_string());
    assert_eq!(client.take_parked_send(true).unwrap().text, "for A");
}

#[tokio::test]
async fn drafts_survive_a_restart_for_their_account_only_until_sign_out() {
    let (client, dir) = client();
    let user = crate::domain::ids::UserId::new();
    client.auth_state().set_logged_in(user, 1, "A".into());
    let (a, b) = (ChannelId::new(), ChannelId::new());
    open(&client, a);
    client.autosave_composer_draft(ComposerDraft {
        text: "half a thought".into(),
        reply: Some(ComposerReply {
            sender_id: "peer".into(),
            timestamp: "1".into(),
            sender_name: "Peer".into(),
            preview: "quoted".into(),
            thumbnail: Some((vec![0; 4], 1, 1)),
        }),
    });
    // Switching away parks it; the parked copy is what the next process sees.
    client.swap_composer_draft(
        &a.to_string(),
        ComposerDraft {
            text: "half a thought".into(),
            reply: None,
        },
        &b.to_string(),
    );
    client.autosave_composer_draft(ComposerDraft {
        text: "for b".into(),
        reply: None,
    });
    drop(client);

    let client = client_in(&dir);
    client.auth_state().set_logged_in(user, 1, "A".into());
    let restored = client.swap_composer_draft("", ComposerDraft::default(), &b.to_string());
    assert_eq!(restored.text, "for b", "live text lost over restart");
    let restored = client.swap_composer_draft(&b.to_string(), restored, &a.to_string());
    assert_eq!(restored.text, "half a thought");
    assert!(restored.reply.is_none(), "parked copy had no reply");

    // Another account on the same installation sees none of it.
    client.auth_state().set_logged_out();
    client
        .auth_state()
        .set_logged_in(crate::domain::ids::UserId::new(), 1, "B".into());
    assert!(
        client
            .swap_composer_draft("", ComposerDraft::default(), &a.to_string())
            .is_empty()
    );

    // Back as A, the drafts are still there; explicit sign-out removes them.
    client.auth_state().set_logged_out();
    client.auth_state().set_logged_in(user, 1, "A".into());
    assert_eq!(
        client
            .swap_composer_draft("", ComposerDraft::default(), &a.to_string())
            .text,
        "half a thought"
    );
    client.clear_composer_session();
    drop(client);

    let client = client_in(&dir);
    client.auth_state().set_logged_in(user, 1, "A".into());
    assert!(
        client
            .swap_composer_draft("", ComposerDraft::default(), &a.to_string())
            .is_empty(),
        "drafts survived sign-out"
    );
}

#[tokio::test]
async fn a_reply_target_survives_a_restart_without_its_thumbnail() {
    let (client, dir) = client();
    let user = crate::domain::ids::UserId::new();
    client.auth_state().set_logged_in(user, 1, "A".into());
    let a = ChannelId::new();
    open(&client, a);
    client.autosave_composer_draft(ComposerDraft {
        text: String::new(),
        reply: Some(ComposerReply {
            sender_id: "peer".into(),
            timestamp: "1".into(),
            sender_name: "Peer".into(),
            preview: "quoted".into(),
            thumbnail: Some((vec![0; 4], 1, 1)),
        }),
    });
    drop(client);
    let client = client_in(&dir);
    client.auth_state().set_logged_in(user, 1, "A".into());
    let reply = client
        .swap_composer_draft("", ComposerDraft::default(), &a.to_string())
        .reply
        .expect("reply target lost over restart");
    assert_eq!(reply.preview, "quoted");
    assert!(reply.thumbnail.is_none());
}

#[tokio::test]
async fn an_autosave_after_sign_out_lands_nowhere() {
    let (client, _dir) = client();
    let a = ChannelId::new();
    open(&client, a);
    client.clear_composer_session();
    // The old UI's debounced autosave fires after core moved on.
    client.autosave_composer_draft(ComposerDraft {
        text: "stale".into(),
        reply: None,
    });
    client
        .auth_state()
        .set_logged_in(crate::domain::ids::UserId::new(), 1, "B".into());
    assert!(
        client
            .swap_composer_draft("", ComposerDraft::default(), &a.to_string())
            .is_empty()
    );
}

#[tokio::test]
async fn sign_out_before_any_conversation_opens_still_deletes_the_row() {
    let (client, dir) = client();
    let user = crate::domain::ids::UserId::new();
    client.auth_state().set_logged_in(user, 1, "A".into());
    let a = ChannelId::new();
    open(&client, a);
    client.autosave_composer_draft(ComposerDraft {
        text: "kept".into(),
        reply: None,
    });
    drop(client);

    // Next launch: signed in, drafts never loaded, sign out from the list.
    let client = client_in(&dir);
    client.auth_state().set_logged_in(user, 1, "A".into());
    client.clear_composer_session();
    drop(client);

    let client = client_in(&dir);
    client.auth_state().set_logged_in(user, 1, "A".into());
    assert!(
        client
            .swap_composer_draft("", ComposerDraft::default(), &a.to_string())
            .is_empty(),
        "row survived a sign-out that never loaded it"
    );
}

#[tokio::test]
async fn signing_out_another_account_keeps_the_last_loaded_owners_row() {
    let (client, dir) = client();
    let a_user = crate::domain::ids::UserId::new();
    client.auth_state().set_logged_in(a_user, 1, "A".into());
    let a = ChannelId::new();
    open(&client, a);
    client.autosave_composer_draft(ComposerDraft {
        text: "a's thought".into(),
        reply: None,
    });

    // B signs in and out again without ever touching the composer, so the
    // map is still the one loaded for A. Only B's row may go.
    client.auth_state().set_logged_out();
    client
        .auth_state()
        .set_logged_in(crate::domain::ids::UserId::new(), 1, "B".into());
    client.clear_composer_session();
    drop(client);

    let client = client_in(&dir);
    client.auth_state().set_logged_in(a_user, 1, "A".into());
    assert_eq!(
        client
            .swap_composer_draft("", ComposerDraft::default(), &a.to_string())
            .text,
        "a's thought",
        "another account's sign-out deleted A's drafts"
    );
}

/// A file the app produced, as the Android picker leaves it: in a
/// one-selection directory under the cache dir.
fn picked(dir: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
    let selection = dir.path().join("cache").join("picked").join("selection_1");
    std::fs::create_dir_all(&selection).unwrap();
    let path = selection.join(name);
    std::fs::write(&path, b"bytes").unwrap();
    path
}

fn stage(client: &ChatterClient, path: std::path::PathBuf) -> PendingAttachment {
    let path = client.stage_owned_file(path);
    let chat = client.chat_state().get();
    let pending = PendingAttachment::new(
        AttachmentId::new(),
        path.file_name().unwrap().to_str().unwrap().into(),
        path,
        5,
        "image/png".into(),
    );
    client.chat_state().add_pending_attachment_for(
        chat.active_conversation,
        chat.attachment_revision,
        pending.clone(),
    );
    // The UI's state watcher does this on every tray change.
    client.persist_composer_recovery();
    pending
}

#[tokio::test]
async fn an_owned_file_is_sheltered_under_the_account_and_a_users_file_is_not() {
    let (client, dir) = client();
    let a = ChannelId::new();
    open(&client, a);
    let cache_copy = picked(&dir, "cat.png");
    let staged = client.stage_owned_file(cache_copy.clone());
    assert_ne!(staged, cache_copy);
    assert!(staged.starts_with(client.staging_root()));
    assert!(staged.is_file() && !cache_copy.exists());
    assert!(
        !cache_copy.parent().unwrap().exists(),
        "the picker's selection directory was left behind"
    );

    // Release touches only what lives under the staging root.
    let original = dir.path().join("my-photo.png");
    std::fs::write(&original, b"mine").unwrap();
    let mine = PendingAttachment::new(
        AttachmentId::new(),
        "my-photo.png".into(),
        original.clone(),
        4,
        "image/png".into(),
    );
    let ours = PendingAttachment::new(
        AttachmentId::new(),
        "cat.png".into(),
        staged.clone(),
        5,
        "image/png".into(),
    );
    client.release_staged_files(&[mine, ours]);
    assert!(original.is_file(), "the user's original was deleted");
    assert!(!staged.exists() && !staged.parent().unwrap().exists());
}

#[tokio::test]
async fn staged_files_survive_a_restart_in_their_own_conversation_only() {
    let (client, dir) = client();
    let user = crate::domain::ids::UserId::new();
    client.auth_state().set_logged_in(user, 1, "A".into());
    let (a, b) = (ChannelId::new(), ChannelId::new());
    open(&client, a);
    let kept = stage(&client, picked(&dir, "for-a.png"));
    open(&client, b);
    // A GIF: pre-uploaded, no file behind it.
    client
        .chat_state()
        .add_pending_attachment(PendingAttachment::with_pointer(
            AttachmentId::new(),
            Default::default(),
            "fun.gif".into(),
            9,
            "image/gif".into(),
            crate::proto::content_v1::AttachmentPointer {
                cdn_key: "k".into(),
                ..Default::default()
            },
        ));
    client.persist_composer_recovery();
    drop(client);

    let client = client_in(&dir);
    client.auth_state().set_logged_in(user, 1, "A".into());
    open(&client, b);
    let tray = client.chat_state().get_pending_attachments();
    assert_eq!(tray.len(), 1, "B's GIF lost over restart");
    assert_eq!(tray[0].pointer.as_ref().unwrap().cdn_key, "k");
    open(&client, a);
    let tray = client.chat_state().get_pending_attachments();
    assert_eq!(tray.len(), 1, "A's file lost over restart");
    assert_eq!(tray[0].path, kept.path);
    assert!(kept.path.is_file());

    // Another account sees none of it, and its sign-out leaves A's files.
    client.auth_state().set_logged_out();
    client
        .auth_state()
        .set_logged_in(crate::domain::ids::UserId::new(), 1, "B".into());
    open(&client, a);
    assert!(client.chat_state().get_pending_attachments().is_empty());
    client.clear_composer_session();
    assert!(kept.path.is_file(), "B's sign-out deleted A's staged file");
}

#[tokio::test]
async fn a_file_deleted_from_disk_stays_as_evidence_and_unacknowledged_files_are_kept() {
    let (client, dir) = client();
    let user = crate::domain::ids::UserId::new();
    client.auth_state().set_logged_in(user, 1, "A".into());
    let a = ChannelId::new();
    open(&client, a);
    let gone = stage(&client, picked(&dir, "gone.png"));
    std::fs::remove_file(&gone.path).unwrap();
    // Staged, then killed before the tray was written.
    let orphan = client.stage_owned_file(picked(&dir, "orphan.png"));
    drop(client);

    let client = client_in(&dir);
    client.auth_state().set_logged_in(user, 1, "A".into());
    open(&client, a);
    // The chip comes back so the user sees what was staged; its send
    // fails whole at upload until they remove it.
    let tray = client.chat_state().get_pending_attachments();
    assert_eq!(tray.len(), 1);
    assert_eq!(tray[0].path, gone.path);
    // Not named by the row, but it may be a staging write whose row
    // update never landed: kept until removal, send or sign-out.
    assert!(orphan.exists(), "unacknowledged staged file was deleted");
}

#[tokio::test]
async fn a_parked_send_with_a_missing_file_keeps_the_file_in_the_restored_send() {
    let (client, dir) = client();
    let user = crate::domain::ids::UserId::new();
    client.auth_state().set_logged_in(user, 1, "A".into());
    let (a, b) = (ChannelId::new(), ChannelId::new());
    open(&client, a);
    let kept = stage(&client, picked(&dir, "kept.png"));
    let lost = stage(&client, picked(&dir, "lost.png"));
    let send = client.begin_composer_send("both".into(), None).unwrap();
    open(&client, b);
    assert!(client.recover_composer_send(send, true).is_none());
    std::fs::remove_file(&lost.path).unwrap();
    drop(client);

    let client = client_in(&dir);
    client.auth_state().set_logged_in(user, 1, "A".into());
    open(&client, a);
    let back = client.take_parked_send(true).expect("parked send lost");
    assert_eq!(back.text, "both");
    let paths: Vec<_> = back.attachments.iter().map(|a| a.path.clone()).collect();
    assert_eq!(
        paths,
        vec![kept.path.clone(), lost.path.clone()],
        "partial send prepared"
    );
    assert!(kept.path.is_file());
}

#[tokio::test]
async fn an_unreadable_recovery_row_is_neither_swept_nor_written_over() {
    let (client, dir) = client();
    let user = crate::domain::ids::UserId::new();
    client.auth_state().set_logged_in(user, 1, "A".into());
    let a = ChannelId::new();
    open(&client, a);
    let file = stage(&client, picked(&dir, "safe.png"));
    // Whatever wrote this row, this build cannot decode it.
    let key = ChatterClient::composer_recovery_key(user);
    client
        .settings_repo()
        .set(&key, &"not a recovery row")
        .unwrap();
    drop(client);

    let client = client_in(&dir);
    client.auth_state().set_logged_in(user, 1, "A".into());
    open(&client, a);
    assert!(client.chat_state().get_pending_attachments().is_empty());
    assert!(file.path.is_file(), "file of an unread row was swept");
    // A tray change this session must not replace the unread row.
    stage(&client, picked(&dir, "new.png"));
    let row: Option<String> = client.settings_repo().get(&key).unwrap();
    assert_eq!(row.as_deref(), Some("not a recovery row"));
    // Sign-out is the one explicit cleanup and still clears it.
    client.clear_composer_session();
    let row: Option<String> = client.settings_repo().get(&key).unwrap();
    assert!(row.is_none());
}

#[tokio::test]
async fn a_parked_failed_send_survives_a_restart_for_its_conversation() {
    let (client, dir) = client();
    let user = crate::domain::ids::UserId::new();
    client.auth_state().set_logged_in(user, 1, "A".into());
    let (a, b) = (ChannelId::new(), ChannelId::new());
    open(&client, a);
    let file = stage(&client, picked(&dir, "late.png"));
    let send = client
        .begin_composer_send(
            "caption".into(),
            Some(ComposerReply {
                sender_id: "peer".into(),
                timestamp: "1".into(),
                sender_name: "Peer".into(),
                preview: "quoted".into(),
                thumbnail: None,
            }),
        )
        .unwrap();
    // Failed while B is open: parked, and the parked copy is persisted.
    open(&client, b);
    assert!(client.recover_composer_send(send, true).is_none());
    drop(client);

    let client = client_in(&dir);
    client.auth_state().set_logged_in(user, 1, "A".into());
    open(&client, b);
    assert!(
        client.take_parked_send(true).is_none(),
        "A's send restored into B"
    );
    open(&client, a);
    let back = client
        .take_parked_send(true)
        .expect("parked send lost over restart");
    assert_eq!(back.text, "caption");
    assert_eq!(back.reply.unwrap().preview, "quoted");
    assert_eq!(back.attachments.len(), 1);
    assert_eq!(back.attachments[0].path, file.path);
    assert!(file.path.is_file(), "the parked send's file was swept");
    assert!(client.take_parked_send(true).is_none(), "restored twice");

    // Taken: the row no longer holds it.
    drop(client);
    let client = client_in(&dir);
    client.auth_state().set_logged_in(user, 1, "A".into());
    open(&client, a);
    assert!(
        client.take_parked_send(true).is_none(),
        "taken send came back"
    );
}

#[tokio::test]
async fn sign_out_removes_the_recovery_row_and_the_staged_files() {
    let (client, dir) = client();
    let user = crate::domain::ids::UserId::new();
    client.auth_state().set_logged_in(user, 1, "A".into());
    let a = ChannelId::new();
    open(&client, a);
    let file = stage(&client, picked(&dir, "private.png"));
    client.clear_composer_session();
    assert!(!file.path.exists(), "staged file survived sign-out");
    drop(client);

    let client = client_in(&dir);
    client.auth_state().set_logged_in(user, 1, "A".into());
    open(&client, a);
    assert!(client.chat_state().get_pending_attachments().is_empty());
}

#[tokio::test]
async fn a_thread_draft_survives_a_restart_and_clears_when_sent() {
    let (client, dir) = client();
    let user = crate::domain::ids::UserId::new();
    client.auth_state().set_logged_in(user, 1, "A".into());
    let a = ChannelId::new();
    open(&client, a);
    let root = crate::domain::ids::MessageId::new();
    client.set_thread_draft(&root, "half a reply");
    drop(client);

    let client = client_in(&dir);
    client.auth_state().set_logged_in(user, 1, "A".into());
    assert_eq!(client.thread_draft(&root), "half a reply");
    assert!(
        client
            .thread_draft(&crate::domain::ids::MessageId::new())
            .is_empty()
    );
    client.set_thread_draft(&root, "");
    drop(client);

    let client = client_in(&dir);
    client.auth_state().set_logged_in(user, 1, "A".into());
    assert!(
        client.thread_draft(&root).is_empty(),
        "sent reply came back"
    );

    // A reply that failed after the tap goes back to its thread, unless
    // something newer was typed there or the session is over.
    let revision = client.auth_state().get().session_revision;
    assert!(client.restore_thread_draft(&root, "failed reply", revision));
    assert_eq!(client.thread_draft(&root), "failed reply");
    client.set_thread_draft(&root, "typed since");
    // Newer text is busy: the failed reply is parked beside it, and a
    // second failure joins the first. Neither touches the draft.
    assert!(client.restore_thread_draft(&root, "failed reply", revision));
    assert!(client.restore_thread_draft(&root, "and again", revision));
    assert_eq!(client.thread_draft(&root), "typed since");
    assert!(client.has_failed_thread_reply(&root));
    assert!(
        client.take_failed_thread_reply(&root).is_none(),
        "took over typed text"
    );
    // The newer reply goes out: its draft clears, the parked one survives
    // a restart and comes back only into the empty draft.
    client.set_thread_draft(&root, "");
    drop(client);
    let client = client_in(&dir);
    client.auth_state().set_logged_in(user, 1, "A".into());
    assert_eq!(
        client.take_failed_thread_reply(&root).as_deref(),
        Some("failed reply\n\nand again")
    );
    assert_eq!(client.thread_draft(&root), "failed reply\n\nand again");
    assert!(
        client.take_failed_thread_reply(&root).is_none(),
        "restored twice"
    );
    client.set_thread_draft(&root, "");
    assert!(!client.restore_thread_draft(&root, "failed reply", revision + 1));
    assert!(client.thread_draft(&root).is_empty());
    assert!(!client.has_failed_thread_reply(&root));
}

#[tokio::test(flavor = "current_thread")]
async fn batch_file_preparation_preserves_order_owner_and_cleans_rejected_staging() {
    let (client, dir) = client();
    let a = ChannelId::new();
    let b = ChannelId::new();
    open(&client, a);
    let revision = client.chat_snapshot().attachment_revision;
    let conversation = ActiveConversation::Channel(a);
    // Navigate while the selection belongs to A: B must never receive it.
    open(&client, b);
    for index in 0..8 {
        let input = picked(&dir, &format!("photo-{index}.png"));
        let encrypted = client.stage_owned_file_async(input.clone()).await.unwrap();
        assert_ne!(encrypted, input);
        client.prepare_pending_file(encrypted, Some(5), conversation, revision).await.unwrap();
    }
    assert!(client.pending_attachments().is_empty());
    open(&client, a);
    assert_eq!(client.pending_attachments().iter().map(|a| a.filename.clone()).collect::<Vec<_>>(),
        (0..8).map(|i| format!("photo-{i}.png")).collect::<Vec<_>>());
    let original = dir.path().join("missing.png");
    assert!(client.prepare_pending_file(original, None, conversation, revision).await.is_err());
    assert_eq!(client.pending_attachments().len(), 8);

    let rejected = client.stage_owned_file_async(picked(&dir, "stale.png")).await.unwrap();
    client.chat_state().clear_attachment_trays();
    client.prepare_pending_file(rejected.clone(), Some(5), conversation, revision).await.unwrap();
    assert!(!rejected.exists(), "rejected selection leaked encrypted staging");
    assert!(client.pending_attachments().is_empty());
}

#[tokio::test]
async fn staged_tray_pixels_are_small_and_require_authenticated_ciphertext() {
    let (client, dir) = client();
    open(&client, ChannelId::new());
    let input = picked(&dir, "preview.png");
    image::DynamicImage::new_rgba8(640, 320).save(&input).unwrap();
    let path = client.stage_owned_file_async(input).await.unwrap();
    let reader = client.clone();
    let preview_path = path.clone();
    let pixels = crate::services::attachment::run_attachment_work(move || reader.pending_image_pixels(&preview_path))
        .await.unwrap().unwrap();
    assert_eq!(pixels.dimensions(), (160, 80));
    // Tamper only the authentication trailer: plaintext pixels would remain
    // decodable if preview authentication were accidentally bypassed.
    let mut ciphertext = std::fs::read(&path).unwrap();
    *ciphertext.last_mut().unwrap() ^= 1;
    std::fs::write(&path, ciphertext).unwrap();
    let reader = client.clone();
    let preview_path = path.clone();
    assert!(crate::services::attachment::run_attachment_work(move || reader.pending_image_pixels(&preview_path))
        .await.unwrap().is_none());
}

#[test]
fn preview_pixel_budget_does_not_drop_or_reorder_batch_attachments() {
    let files: Vec<_> = (0..100).map(|index| staged(&format!("{index}.png"))).collect();
    let candidates: Vec<_> = ChatterClient::pending_preview_files(&files).collect();
    assert_eq!(candidates.len(), 64);
    assert_eq!(candidates.iter().map(|(index, _)| *index).collect::<Vec<_>>(), (0..64).collect::<Vec<_>>());
    assert_eq!(files.len(), 100);
}

#[tokio::test]
async fn downloaded_preview_cannot_adopt_session_when_first_polled_late() {
    let (client, _dir) = client();
    let guard = client.session_guard();
    let pending = client.prepare_downloaded_image_preview(AttachmentId::new(), guard);
    client.auth_state().invalidate_session();
    let error = match pending.await {
        Err(error) => error,
        Ok(_) => panic!("stale preview admitted"),
    };
    assert!(
        matches!(error, crate::services::attachment::AttachmentError::InvalidAttachment(ref detail) if detail == "image preview owner changed")
    );
}

#[tokio::test]
async fn overlapping_batches_wait_in_accept_order_even_if_polled_backwards() {
    let (client, _dir) = client();
    open(&client, ChannelId::new());
    client.chat_state().add_pending_attachment(staged("first.jpg"));
    let first = client.begin_composer_send("first".into(), None).unwrap();
    client.chat_state().add_pending_attachment(staged("second.jpg"));
    let second = client.begin_composer_send("second".into(), None).unwrap();
    let third = client.begin_composer_send("third".into(), None).unwrap();
    assert_eq!(first.attachments[0].filename, "first.jpg");
    assert_eq!(second.attachments[0].filename, "second.jpg");
    assert_ne!(first.attempt_id, second.attempt_id);
    assert!(!first.is_upload_queued());
    assert!(second.is_upload_queued());
    assert!(tokio::time::timeout(std::time::Duration::from_millis(10), client.await_composer_turn(&third)).await.is_err());
    assert!(client.await_composer_turn(&first).await);
    drop(first);
    assert!(client.await_composer_turn(&second).await);
    assert!(tokio::time::timeout(std::time::Duration::from_millis(10), client.await_composer_turn(&third)).await.is_err());
    drop(second);
    assert!(client.await_composer_turn(&third).await);
}

#[tokio::test]
async fn cancelled_middle_batch_cannot_let_a_later_batch_pass_the_active_send() {
    let (client, _dir) = client();
    open(&client, ChannelId::new());
    let first = client.begin_composer_send("first".into(), None).unwrap();
    let middle = client.begin_composer_send("cancelled".into(), None).unwrap();
    let last = client.begin_composer_send("last".into(), None).unwrap();
    drop(middle);
    assert!(tokio::time::timeout(std::time::Duration::from_millis(10), client.await_composer_turn(&last)).await.is_err());
    drop(first);
    assert!(client.await_composer_turn(&last).await);
}

#[tokio::test]
async fn queued_sends_are_bounded_without_consuming_the_next_draft() {
    let (client, _dir) = client();
    open(&client, ChannelId::new());
    let sends: Vec<_> = (0..32).map(|i| client.begin_composer_send(i.to_string(), None).unwrap()).collect();
    client.chat_state().add_pending_attachment(staged("kept.jpg"));
    assert!(client.begin_composer_send("kept".into(), None).unwrap_err().contains("queue is full"));
    assert_eq!(client.pending_attachments()[0].filename, "kept.jpg");
    drop(sends);
}

#[tokio::test]
async fn another_conversation_does_not_wait_and_a_retired_session_cannot_submit() {
    let (client, _dir) = client();
    open(&client, ChannelId::new());
    let first = client.begin_composer_send("first".into(), None).unwrap();
    let queued = client.begin_composer_send("queued".into(), None).unwrap();
    open(&client, ChannelId::new());
    let independent = client.begin_composer_send("independent".into(), None).unwrap();
    assert!(client.await_composer_turn(&independent).await);
    client.auth_state().invalidate_session();
    assert!(!client.await_composer_turn(&queued).await);
    drop(first);
}

#[tokio::test]
async fn queued_batches_survive_restart_as_unsent_owner_local_drafts() {
    let (client, dir) = client();
    let owner = client.auth_state().get().current_user_id.unwrap();
    let channel = ChannelId::new();
    open(&client, channel);
    client.chat_state().add_pending_attachment(staged("first.jpg"));
    let first = client.begin_composer_send("first caption".into(), None).unwrap();
    client.chat_state().add_pending_attachment(staged("second.jpg"));
    let second = client.begin_composer_send("second caption".into(), None).unwrap();
    drop((first, second, client));
    let client = client_in(&dir);
    client.auth_state().set_logged_in(owner, 1, "Test user".into());
    open(&client, channel);
    let first = client.take_parked_send(true).unwrap();
    assert_eq!(first.text, "first caption");
    assert_eq!(first.attachments[0].filename, "first.jpg");
    assert!(first.queue_ticket.is_none());
    // Simulate consuming the recovered tray before asking for the next draft.
    client.chat_state().clear_pending_attachments();
    let second = client.take_parked_send(true).unwrap();
    assert_eq!(second.text, "second caption");
    assert_eq!(second.attachments[0].filename, "second.jpg");
    assert!(second.queue_ticket.is_none());
    assert!(client.take_parked_send(true).is_none());
}

#[tokio::test]
async fn a_live_upload_claim_accepts_the_next_batch_but_does_not_start_it() {
    let (client, _dir) = client();
    let conversation = ActiveConversation::Channel(ChannelId::new());
    open(&client, conversation.channel_id().unwrap());
    let first = client.begin_composer_send("first".into(), None).unwrap();
    let key = client.retain_send_attempt(first.attempt_id, conversation, &first.text,
        &first.attachments, None, None).unwrap();
    assert!(client.ensure_send_available(conversation).is_err());
    client.chat_state().add_pending_attachment(staged("next.jpg"));
    let second = client.begin_composer_send("next".into(), None).unwrap();
    assert_eq!(second.attachments[0].filename, "next.jpg");
    assert!(second.is_upload_queued());
    assert!(tokio::time::timeout(std::time::Duration::from_millis(10), client.await_composer_turn(&second)).await.is_err());
    crate::client::send_attempt::settle_failure(&client.services.database, &key,
        first.attempt_id, false, None, crate::services::gateway::SendError::NotLoggedIn).unwrap();
    drop(first);
    assert!(client.await_composer_turn(&second).await);
    client.ensure_send_available(conversation).unwrap();
}

#[tokio::test]
async fn queued_intents_are_cleared_at_signout_and_cancelled_with_their_conversation() {
    let (client, _dir) = client();
    let owner = client.auth_state().get().current_user_id.unwrap();
    let channel = ChannelId::new();
    open(&client, channel);
    let first = client.begin_composer_send("first".into(), None).unwrap();
    let original = _dir.path().join("queued-original.jpg");
    std::fs::write(&original, b"fixture original").unwrap();
    let staging_input = _dir.path().join("picker-owned.jpg");
    std::fs::copy(&original, &staging_input).unwrap();
    let sheltered = client.stage_owned_file(staging_input);
    assert_ne!(sheltered, original);
    let mut attachment = staged("queued-original.jpg");
    attachment.path = sheltered.clone();
    client.chat_state().add_pending_attachment(attachment);
    let queued = client.begin_composer_send("queued".into(), None).unwrap();
    client.clear_composer_conversation(ActiveConversation::Channel(channel));
    assert!(!sheltered.exists());
    assert!(original.exists());
    assert!(!client.await_composer_turn(&first).await);
    assert!(!client.await_composer_turn(&queued).await);
    assert!(client.recover_composer_send(queued.clone(), true).is_none());
    assert!(client.parked_sends.lock().is_empty(), "cancelled UI callback restored a deleted conversation");
    let count = || {
        let conn = client.services.database.read_connection().unwrap();
        conn.query_row("SELECT count(*) FROM settings WHERE key GLOB ?1", rusqlite::params![format!("ui.composer_queue.{owner}:*")], |r| r.get::<_, i64>(0)).unwrap()
    };
    assert_eq!(count(), 0);
    open(&client, ChannelId::new());
    let other = client.begin_composer_send("other".into(), None).unwrap();
    assert_eq!(count(), 1);
    client.clear_composer_session();
    assert_eq!(count(), 0);
    assert!(!client.await_composer_turn(&other).await);
}

#[tokio::test]
async fn queue_storage_failure_keeps_the_staged_attachment_and_does_not_claim_a_turn() {
    let (client, _dir) = client();
    open(&client, ChannelId::new());
    client.chat_state().add_pending_attachment(staged("kept.jpg"));
    client.services.database.connection().unwrap().execute_batch(
        "CREATE TRIGGER reject_composer_queue BEFORE INSERT ON settings WHEN NEW.key GLOB 'ui.composer_queue.*' BEGIN SELECT RAISE(ABORT,'injected queue write failure'); END;"
    ).unwrap();
    assert!(client.begin_composer_send("kept caption".into(), None).is_err());
    assert_eq!(client.pending_attachments()[0].filename, "kept.jpg");
    client.services.database.connection().unwrap().execute_batch("DROP TRIGGER reject_composer_queue").unwrap();
    let send = client.begin_composer_send("kept caption".into(), None).unwrap();
    assert_eq!(send.attachments[0].filename, "kept.jpg");
    assert!(!send.is_upload_queued());
}

#[tokio::test]
async fn parking_a_completed_failed_send_releases_the_next_batches_turn() {
    let (client, _dir) = client();
    let conversation = ActiveConversation::Channel(ChannelId::new());
    open(&client, conversation.channel_id().unwrap());
    let first = client.begin_composer_send("failed first".into(), None).unwrap();
    let key = client.retain_send_attempt(first.attempt_id, conversation, &first.text,
        &first.attachments, None, None).unwrap();
    let second = client.begin_composer_send("second".into(), None).unwrap();
    client.chat_state().add_pending_attachment(staged("newer-draft.jpg"));
    crate::client::send_attempt::settle_failure(&client.services.database, &key,
        first.attempt_id, false, None, crate::services::gateway::SendError::NotLoggedIn).unwrap();
    assert!(client.recover_composer_send(first, false).is_none());
    assert!(tokio::time::timeout(std::time::Duration::from_millis(250), client.await_composer_turn(&second)).await.unwrap());
    assert_eq!(client.pending_attachments()[0].filename, "newer-draft.jpg");
    let parked = client.parked_sends.lock();
    assert_eq!(parked.len(), 1);
    assert!(parked[0].queue_ticket.is_none());
}
