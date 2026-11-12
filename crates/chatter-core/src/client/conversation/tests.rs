use super::*;
use crate::domain::attachment::{Attachment, FileMetadata};
use crate::domain::ids::AttachmentId;

fn received_photo() -> Attachment {
    // The shape a received attachment has locally: no file on this disk,
    // only the CDN blob and the material to decrypt it.
    let mut att = Attachment::new(
        AttachmentId::from_cdn_key("cdn-key-1"),
        FileMetadata::new("photo.jpg".into(), "image/jpeg".into(), 4096),
    );
    att.remote_url = Some("cdn-key-1".into());
    att.encryption_key = Some(vec![7u8; 64]);
    att.encryption_iv = Some(vec![9u8; 32]);
    att
}

/// Forwarding a photo must re-send the CDN pointer it already has. Before
/// this, `forward_to` was text-only: an attachment-only message resolved to
/// no text and the forward failed silently.
#[test]
fn forwarded_attachment_reuses_its_existing_pointer() {
    let out = forwardable_attachments(&[received_photo()]);
    assert_eq!(out.len(), 1, "the photo was dropped from the forward");
    let pointer = out[0].pointer.as_ref().expect("forward must not re-upload");
    assert_eq!(pointer.cdn_key, "cdn-key-1");
    assert_eq!(pointer.file_name, "photo.jpg");
    assert_eq!(pointer.plaintext_size, 4096);
    assert!(
        out[0].path.as_os_str().is_empty(),
        "there is no local file to upload"
    );
}

/// A record that can no longer be re-pointed (no digest, so the receiver
/// could not verify the blob) is dropped rather than shipped broken.
#[test]
fn unpointable_attachment_is_dropped() {
    let mut att = received_photo();
    att.encryption_iv = None;
    assert!(forwardable_attachments(&[att]).is_empty());
}

// ========================================================================
// Secret-chat forwarding
// ========================================================================

/// The message menu offers no Forward in a secret chat, and still does in
/// an ordinary DM.
#[test]
fn secret_chat_messages_offer_no_forward() {
    let peer = UserId::new();
    for (ct, forward) in [
        (ConversationType::AnonDm, false),
        (ConversationType::AnonGroup, false),
        (ConversationType::ServerDm, true),
    ] {
        let mut message = crate::domain::message::Message::new_text(
            ChannelId::new(),
            ct,
            Some(peer),
            "hello".into(),
            1,
        );
        message.delivery_status = "delivered".into();
        let menu = stored_message_menu(&message, false, 2);
        assert_eq!(
            menu.actions.forward, forward,
            "{:?}",
            message.conversation_type
        );
    }
}

/// Nothing is forwarded out of a secret chat, and its picker is empty —
/// refused before any lookup or send.
#[tokio::test]
async fn nothing_forwards_out_of_a_secret_chat() {
    let (client, _dir) = test_client();
    let source = client.conversation(ActiveConversation::AnonDm(
        crate::domain::anon_dm::AnonDmId::new(),
    ));
    assert!(!source.can_forward_from());
    assert!(
        source
            .forward_destinations(&[(UserId::new(), 1)], "")
            .is_empty()
    );
    let err = source
        .forward_to(
            &UserId::new(),
            1,
            ActiveConversation::ServerDm {
                peer_id: UserId::new(),
            },
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("secret chat"), "{err}");
}

/// An attachment is never forwarded into a secret chat: its pointer names
/// a CDN object minted outside it. Refused before anything is sent.
#[tokio::test]
async fn attachments_do_not_forward_into_a_secret_chat() {
    let (client, _dir) = test_client();
    let peer = UserId::new();
    let attachment = AttachmentId::from_cdn_key("forward-into-secret");
    let mut message = crate::domain::message::Message::new_text(
        ChannelId::from(peer),
        ConversationType::ServerDm,
        Some(peer),
        "look".into(),
        7,
    );
    message.attachments = vec![attachment];
    client.messages().message_repo.insert(&message).unwrap();

    let source = client.conversation(ActiveConversation::ServerDm { peer_id: peer });
    assert!(source.can_forward_from());
    let err = source
        .forward_to(
            &peer,
            7,
            ActiveConversation::AnonDm(crate::domain::anon_dm::AnonDmId::new()),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("attachments"), "{err}");
}

// ========================================================================
// Read state
// ========================================================================

/// A client with a temp datastore and nothing behind its server URL — the
/// storage half of `mark_as_read` is what these tests are about. Keyring is
/// pointed at its in-memory mock (same reason as `client::tests`).
fn test_client() -> (Arc<ChatterClient>, tempfile::TempDir) {
    static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
    MOCK_KEYRING.call_once(|| {
        keyring::set_default_store(crate::system::keyring::test_credential_store());
    });

    let dir = tempfile::tempdir().unwrap();
    let client = ChatterClient::builder()
        .server_url("https://localhost:1")
        .data_dir(dir.path().to_str().unwrap())
        .build()
        .expect("build client");
    // Turn the wire half off at its privacy gate — there is no server here.
    client
        .set_read_receipts_enabled(false)
        .expect("read-receipt setting");
    (client, dir)
}

fn store_peer_message(
    client: &ChatterClient,
    conversation_id: ChannelId,
    conversation_type: ConversationType,
    peer: UserId,
    n: u64,
) {
    let message = crate::domain::message::Message::new_text(
        conversation_id,
        conversation_type,
        Some(peer),
        format!("message {n}"),
        n,
    );
    client
        .messages()
        .message_repo
        .insert(&message)
        .expect("store message");
}

fn unread(client: &ChatterClient, id: &ChannelId, conv_type: &ConversationType) -> u64 {
    client
        .messages()
        .message_repo
        .get_unread_count(id, conv_type)
        .expect("unread count")
}

#[tokio::test]
async fn live_receive_marks_only_the_foreground_conversation_read() {
    let (client, _dir) = test_client();
    let peer = UserId::new();
    let id: ChannelId = peer.into();
    let message = crate::domain::message::Message::new_text(
        id,
        ConversationType::ServerDm,
        Some(peer),
        "live".into(),
        1,
    );
    client.messages().message_repo.insert(&message).unwrap();
    client
        .chat_state()
        .set_active_conversation(ActiveConversation::Channel(ChannelId::new()));
    ConversationHandle::mark_received_as_read(&client, &message)
        .await
        .unwrap();
    assert_eq!(unread(&client, &id, &ConversationType::ServerDm), 1);

    client
        .chat_state()
        .set_active_conversation(ActiveConversation::ServerDm { peer_id: peer });
    client.chat_state().set_app_visible(false);
    ConversationHandle::mark_received_as_read(&client, &message)
        .await
        .unwrap();
    assert_eq!(unread(&client, &id, &ConversationType::ServerDm), 1);

    client.chat_state().set_app_visible(true);
    ConversationHandle::mark_received_as_read(&client, &message)
        .await
        .unwrap();
    assert_eq!(unread(&client, &id, &ConversationType::ServerDm), 0);

    // A subsequent arrival is read without reopening the conversation,
    // even when sending peer read receipts is disabled.
    store_peer_message(&client, id, ConversationType::ServerDm, peer, 2);
    ConversationHandle::mark_received_as_read(&client, &message)
        .await
        .unwrap();
    assert_eq!(unread(&client, &id, &ConversationType::ServerDm), 0);
}

/// A friend DM's transport is Shield mode's call, made in one place.
/// Shield: the mailbox — here first contact with no cached device, so the
/// mailbox bootstrap refuses (`NotReachablePrivately`) and it never falls
/// back to sealed-sender streaming. Normal: sealed sender, and nothing
/// touches the mailbox queue.
#[tokio::test]
async fn friend_dm_transport_follows_shield_mode() {
    use crate::storage::repositories::settings_repo::keys;
    use std::sync::atomic::Ordering;

    let (client, _dir) = test_client();
    let ms = client.messages();
    // We are the higher UUID: the Shield send would ask the friend to
    // initiate — but no device of theirs is cached yet.
    let me = UserId::from_uuid(uuid::Uuid::max());
    let friend = UserId::from_uuid(uuid::Uuid::from_u128(1));
    ms.settings_repo
        .set_string(keys::CURRENT_USER_ID, &me.to_string())
        .unwrap();
    ms.settings_repo
        .set(keys::CURRENT_DEVICE_ID, &1u32)
        .unwrap();
    let content = || crate::services::build_text_content("hi", 1);

    ms.shield_mode.store(true, Ordering::Release);
    let shield = ms.send_friend_dm_content(&friend, content()).await;
    assert!(
        matches!(
            shield,
            Err(crate::services::message::MessageError::NotReachablePrivately(_))
        ),
        "Shield must take the mailbox, got {shield:?}"
    );
    assert!(ms.pending_dms.drain(&friend).unwrap().is_empty());

    ms.shield_mode.store(false, Ordering::Release);
    let normal = ms.send_friend_dm_content(&friend, content()).await;
    assert!(
        !matches!(
            normal,
            Err(crate::services::message::MessageError::BootstrapInitiated)
        ),
        "normal mode must not take the mailbox"
    );
    assert!(ms.pending_dms.drain(&friend).unwrap().is_empty());
}

/// Opening a DM has to leave the sidebar badge at zero. The badge is a
/// `COUNT(is_read = 0)` rebuilt from storage on every refresh and every
/// restart, so a `mark_as_read` that only put a receipt on the wire left it
/// lit forever — the DM stuck on 5 that reopening would not clear.
#[tokio::test]
async fn opening_a_server_dm_clears_its_unread_count() {
    let (client, _dir) = test_client();
    let peer = UserId::new();
    let conversation_id: ChannelId = peer.into();
    for n in 1..=5 {
        store_peer_message(
            &client,
            conversation_id,
            ConversationType::ServerDm,
            peer,
            n,
        );
    }
    assert_eq!(
        unread(&client, &conversation_id, &ConversationType::ServerDm),
        5,
        "positive control: the peer's messages must start unread"
    );

    client
        .conversation(ActiveConversation::ServerDm { peer_id: peer })
        .mark_as_read()
        .await
        .expect("mark as read");

    assert_eq!(
        unread(&client, &conversation_id, &ConversationType::ServerDm),
        0,
        "opening the DM must persist the read state, not only receipt it"
    );
}

/// Same defect, same fix, on the channel path.
#[tokio::test]
async fn opening_a_channel_clears_its_unread_count() {
    let (client, _dir) = test_client();
    let channel_id = ChannelId::new();
    let peer = UserId::new();
    for n in 1..=3 {
        store_peer_message(&client, channel_id, ConversationType::Channel, peer, n);
    }
    assert_eq!(unread(&client, &channel_id, &ConversationType::Channel), 3);

    client
        .conversation(ActiveConversation::Channel(channel_id))
        .mark_as_read()
        .await
        .expect("mark as read");

    assert_eq!(
        unread(&client, &channel_id, &ConversationType::Channel),
        0,
        "opening the channel must persist the read state"
    );
}

/// A message we sent is read by definition. Stored unread, it badged the
/// conversation the user was typing in — invisibly until the next sidebar
/// rebuild, then permanently.
#[tokio::test]
async fn our_own_sent_message_never_counts_as_unread() {
    let (client, _dir) = test_client();
    let me = UserId::new();
    let conversation_id: ChannelId = UserId::new().into();
    let message = crate::domain::message::Message::new_text(
        conversation_id,
        ConversationType::ServerDm,
        Some(me),
        "sent by us".to_string(),
        1,
    );
    client
        .messages()
        .message_repo
        .insert_outgoing(&message)
        .expect("store local echo");

    assert_eq!(
        unread(&client, &conversation_id, &ConversationType::ServerDm),
        0,
        "our own local echo must not badge the conversation"
    );
}

/// Deleting an anon DM takes its whole local footprint: messages, the
/// attachments only it carried (row and file), the conversation row. An
/// attachment a forward put in another conversation stays, and a blocked
/// peer keeps their name in Friends › Blocked.
#[tokio::test]
async fn delete_anon_dm_removes_messages_and_files_and_keeps_blocked_name() {
    use crate::domain::anon_dm::AnonDm;
    use crate::domain::ids::ChannelId;
    use crate::domain::message::{ConversationType, Message};

    static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
    MOCK_KEYRING.call_once(|| {
        keyring::set_default_store(crate::system::keyring::test_credential_store());
    });
    let dir = tempfile::tempdir().unwrap();
    let client = ChatterClient::builder()
        .server_url("https://localhost:1")
        .data_dir(dir.path().to_str().unwrap())
        .build()
        .expect("build client");

    let peer = UserId::from_uuid(uuid::Uuid::from_u128(7));
    let mut dm = AnonDm::new_initiated(peer);
    dm.peer_display_name = Some("Robin".into());
    client
        .anon_dms()
        .anon_dm_repo()
        .insert_conversation(&dm)
        .unwrap();
    client.anon_dms().block_user(&peer, None).unwrap();

    let attachments = client.attachments().attachment_repo();
    let stored = |name: &str| {
        let id = AttachmentId::new();
        let path = dir.path().join(format!("{id}.enc"));
        std::fs::write(&path, b"ciphertext").unwrap();
        let att = Attachment::new(id, FileMetadata::new(name.into(), "image/jpeg".into(), 10));
        attachments.insert_with_file_path(&att, &path).unwrap();
        (id, path)
    };
    let (only_here, only_here_path) = stored("a.jpg");
    let (forwarded, forwarded_path) = stored("b.jpg");

    let conv: ChannelId = dm.id.into();
    let mut msg = Message::new_text(conv, ConversationType::AnonDm, Some(peer), "hi".into(), 1);
    msg.attachments = vec![only_here, forwarded];
    client.messages().message_repo.insert(&msg).unwrap();
    let elsewhere: ChannelId = UserId::from_uuid(uuid::Uuid::from_u128(8)).into();
    let mut fwd = Message::new_text(
        elsewhere,
        ConversationType::ServerDm,
        None,
        String::new(),
        2,
    );
    fwd.attachments = vec![forwarded];
    client.messages().message_repo.insert(&fwd).unwrap();

    client
        .delete_conversation(ActiveConversation::AnonDm(dm.id))
        .await
        .unwrap();

    assert!(
        client
            .anon_dms()
            .get_conversation(&dm.id)
            .unwrap()
            .is_none()
    );
    assert!(
        client
            .messages()
            .message_repo
            .get(&msg.id)
            .unwrap()
            .is_none()
    );
    assert!(attachments.get(&only_here).unwrap().is_none());
    assert!(
        !only_here_path.exists(),
        "the deleted chat's file stayed on disk"
    );
    assert!(attachments.get(&forwarded).unwrap().is_some());
    assert!(
        forwarded_path.exists(),
        "a file another chat still shows was deleted"
    );
    assert!(
        client
            .messages()
            .message_repo
            .get(&fwd.id)
            .unwrap()
            .is_some()
    );
    assert_eq!(client.display_name(&peer), "Robin");
    assert!(client.is_profile_user_blocked(&peer));
}

/// Deleting a friend DM empties its history and removes the attachment
/// files only it carried; a file a forward shares elsewhere stays.
#[tokio::test]
async fn delete_friend_dm_removes_messages_and_own_files() {
    use crate::domain::message::Message;

    static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
    MOCK_KEYRING.call_once(|| {
        keyring::set_default_store(crate::system::keyring::test_credential_store());
    });
    let dir = tempfile::tempdir().unwrap();
    let client = ChatterClient::builder()
        .server_url("https://localhost:1")
        .data_dir(dir.path().to_str().unwrap())
        .build()
        .expect("build client");

    let attachments = client.attachments().attachment_repo();
    let stored = |name: &str| {
        let id = AttachmentId::new();
        let path = dir.path().join(format!("{id}.enc"));
        std::fs::write(&path, b"ciphertext").unwrap();
        let att = Attachment::new(id, FileMetadata::new(name.into(), "image/jpeg".into(), 10));
        attachments.insert_with_file_path(&att, &path).unwrap();
        (id, path)
    };
    let (only_here, only_here_path) = stored("a.jpg");
    let (forwarded, forwarded_path) = stored("b.jpg");

    let peer = UserId::from_uuid(uuid::Uuid::from_u128(11));
    let conv: ChannelId = peer.into();
    let mut msg = Message::new_text(conv, ConversationType::ServerDm, Some(peer), "hi".into(), 1);
    msg.attachments = vec![only_here, forwarded];
    client.messages().message_repo.insert(&msg).unwrap();
    let elsewhere: ChannelId = UserId::from_uuid(uuid::Uuid::from_u128(12)).into();
    let mut fwd = Message::new_text(
        elsewhere,
        ConversationType::ServerDm,
        None,
        String::new(),
        2,
    );
    fwd.attachments = vec![forwarded];
    client.messages().message_repo.insert(&fwd).unwrap();

    client
        .delete_conversation(ActiveConversation::ServerDm { peer_id: peer })
        .await
        .unwrap();

    assert!(
        client
            .messages()
            .message_repo
            .get(&msg.id)
            .unwrap()
            .is_none()
    );
    assert!(attachments.get(&only_here).unwrap().is_none());
    assert!(
        !only_here_path.exists(),
        "the deleted chat's file stayed on disk"
    );
    assert!(attachments.get(&forwarded).unwrap().is_some());
    assert!(
        forwarded_path.exists(),
        "a file another chat still shows was deleted"
    );
    assert!(
        client
            .messages()
            .message_repo
            .get(&fwd.id)
            .unwrap()
            .is_some()
    );
}
#[tokio::test]
async fn reaction_snapshot_is_scoped_and_repairs_persisted_add_remove_duplicates() {
    use crate::domain::anon_dm::AnonDmId;
    use crate::domain::anon_group::AnonGroupId;
    let (client, _dir) = test_client();
    let own = UserId::new();
    let author = UserId::new();
    let other = UserId::new();
    client
        .auth_state()
        .set_logged_in(own, 1, "reaction fixture".into());
    crate::storage::repositories::settings_repo::SettingsRepository::new(
        client.messages().database(),
    )
    .set_string(
        crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
        &own.to_string(),
    )
    .unwrap();
    let encrypted_channel = ChannelId::new();
    let scopes = [
        (
            ActiveConversation::Channel(ChannelId::new()),
            ConversationType::Channel,
        ),
        (
            ActiveConversation::Channel(encrypted_channel),
            ConversationType::AnonGroup,
        ),
        (
            ActiveConversation::ServerDm {
                peer_id: UserId::new(),
            },
            ConversationType::ServerDm,
        ),
        (
            ActiveConversation::AnonDm(AnonDmId::new()),
            ConversationType::AnonDm,
        ),
        (
            ActiveConversation::AnonGroup(AnonGroupId::new()),
            ConversationType::AnonGroup,
        ),
    ];
    for (index, (scope, kind)) in scopes.into_iter().enumerate() {
        let sent_at = 400 + index as u64;
        let id = scope.channel_id().unwrap();
        store_peer_message(&client, id, kind.clone(), author, sent_at);
        let handle = client.conversation(scope);
        let snapshot = handle.reaction_snapshot(&author, sent_at).unwrap().unwrap();
        assert!(snapshot.counts.is_empty());
        assert_eq!(snapshot.conversation, scope);
        assert_eq!(snapshot.author, author);
        assert_eq!(snapshot.sent_at, sent_at);
        for reactor in [own, own, other] {
            client
                .messages()
                .record_reaction(
                    &crate::domain::reaction::ReactionScope::new(id, kind.clone()),
                    &author,
                    sent_at,
                    &reactor,
                    "👍",
                    false,
                )
                .unwrap();
        }
        let snapshot = handle.reaction_snapshot(&author, sent_at).unwrap().unwrap();
        assert_eq!(snapshot.counts[0].count, 2);
        assert_eq!(snapshot.own_emojis, vec!["👍".to_string()]);
        for reactor in [own, own] {
            client
                .messages()
                .record_reaction(
                    &crate::domain::reaction::ReactionScope::new(id, kind.clone()),
                    &author,
                    sent_at,
                    &reactor,
                    "👍",
                    true,
                )
                .unwrap();
        }
        let snapshot = handle.reaction_snapshot(&author, sent_at).unwrap().unwrap();
        assert_eq!(snapshot.counts[0].count, 1);
        assert!(snapshot.own_emojis.is_empty());
        client
            .messages()
            .record_reaction(
                &crate::domain::reaction::ReactionScope::new(id, kind.clone()),
                &author,
                sent_at,
                &other,
                "👍",
                true,
            )
            .unwrap();
        let snapshot = handle.reaction_snapshot(&author, sent_at).unwrap().unwrap();
        assert!(snapshot.counts.is_empty());
        assert!(snapshot.summary.is_empty());
        assert!(
            client
                .conversation(ActiveConversation::Channel(ChannelId::new()))
                .reaction_snapshot(&author, sent_at)
                .unwrap()
                .is_none()
        );
        assert!(handle.reaction_snapshot(&other, sent_at).unwrap().is_none());
        assert!(
            handle
                .reaction_snapshot(&author, sent_at + 100)
                .unwrap()
                .is_none()
        );
    }
    // Same UUID does not make an anonymous group into a friend DM.
    assert!(
        client
            .conversation(ActiveConversation::ServerDm {
                peer_id: encrypted_channel.to_string().parse().unwrap()
            })
            .reaction_snapshot(&author, 401)
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn reaction_toggle_does_not_invent_success_for_missing_target() {
    let (client, _dir) = test_client();
    let own = UserId::new();
    client
        .auth_state()
        .set_logged_in(own, 1, "reaction fixture".into());
    crate::storage::repositories::settings_repo::SettingsRepository::new(
        client.messages().database(),
    )
    .set_string(
        crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
        &own.to_string(),
    )
    .unwrap();
    let handle = client.conversation(ActiveConversation::Channel(ChannelId::new()));
    assert!(handle.toggle_reaction("👍", &own, 700).await.is_err());
}
