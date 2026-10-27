use super::*;

#[tokio::test]
async fn blocked_channel_message_stays_absent_after_unblock_and_catch_up() {
    static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
    MOCK_KEYRING.call_once(|| {
        keyring::set_default_store(crate::system::keyring::test_credential_store());
    });
    let dir = tempfile::tempdir().unwrap();
    let client = ChatterClient::builder()
        .server_url("https://localhost:1")
        .data_dir(dir.path().to_str().unwrap())
        .build()
        .unwrap();
    let me = UserId::new();
    let sender = UserId::new();
    let messages = client.messages().clone();
    messages.settings_repo.set_string(
        crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
        &me.to_string(),
    ).unwrap();
    let users = &messages.user_repo;
    users.ensure_user_exists(&me).unwrap();
    users.ensure_user_exists(&sender).unwrap();
    let mut friendship = crate::domain::user::Friendship {
        requester_id: me,
        addressee_id: sender,
        status: crate::domain::user::FriendshipStatus::Blocked,
        created_at: chrono::Utc::now(),
    };
    users.insert_friendship(&friendship).unwrap();
    users.start_block_interval(&me, &sender).unwrap();

    let inbound = ChannelInbound::new(
        client.messages().clone(),
        client.attachments().clone(),
        client.users().clone(),
    );
    let channel = ChannelId::new();
    let id = crate::domain::ids::MessageId::new();
    let mut row = chat_v1::Message {
        id: id.to_string(),
        channel_id: channel.to_string(),
        author_id: sender.to_string(),
        timestamp: chrono::Utc::now().timestamp_nanos_opt().unwrap() as u64,
        content: Some(chat_v1::message::Content {
            r#type: Some(chat_v1::message::content::Type::Text("blocked".into())),
        }),
        ..Default::default()
    };
    let mut events = client.subscribe_events();
    assert_eq!(inbound.ingest(&row, channel, true).await, Ingested::Dropped);
    assert!(!client.messages().message_repo.is_tombstoned(&id).unwrap());
    assert!(client.messages().get_message_local(&id).unwrap().is_none());

    friendship.status = crate::domain::user::FriendshipStatus::Accepted;
    users.update_friendship(&friendship).unwrap();
    users.end_block_interval(&me, &sender).unwrap();
    assert_eq!(inbound.ingest(&row, channel, false).await, Ingested::Dropped);
    inbound.ingest_resynced(&row, channel).await.unwrap();
    assert!(client.messages().get_message_local(&id).unwrap().is_none());
    assert!(matches!(events.try_recv(), Err(tokio::sync::broadcast::error::TryRecvError::Empty)));

    row.id = crate::domain::ids::MessageId::new().to_string();
    row.timestamp = chrono::Utc::now().timestamp_nanos_opt().unwrap() as u64 + 1_000_000_000;
    assert_eq!(inbound.ingest(&row, channel, true).await, Ingested::Stored);
    assert!(matches!(events.try_recv(), Ok(DomainEvent::MessageReceived { .. })));
}

/// The catch-up sync re-fetches every message newer than its last run,
/// including ones that already arrived live and were read. Re-ingesting
/// one must not flip it back to unread.
#[tokio::test]
async fn reingesting_a_read_message_keeps_it_read() {
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
    let inbound = ChannelInbound::new(
        client.messages().clone(),
        client.attachments().clone(),
        client.users().clone(),
    );

    let channel_id = ChannelId::new();
    let proto = chat_v1::Message {
        id: crate::domain::ids::MessageId::new().to_string(),
        channel_id: channel_id.to_string(),
        author_id: UserId::new().to_string(),
        timestamp: 1_000,
        content: Some(chat_v1::message::Content {
            r#type: Some(chat_v1::message::content::Type::Text("hi".into())),
        }),
        ..Default::default()
    };
    let unread = || {
        client
            .messages()
            .message_repo
            .get_unread_count(&channel_id, &ConversationType::Channel)
            .unwrap()
    };

    assert_eq!(
        inbound.ingest(&proto, channel_id, false).await,
        Ingested::Stored
    );
    assert_eq!(
        unread(),
        1,
        "positive control: a received message starts unread"
    );

    client
        .mark_conversation_read(&channel_id, &ConversationType::Channel)
        .unwrap();
    assert_eq!(
        inbound.ingest(&proto, channel_id, false).await,
        Ingested::Stored
    );
    assert_eq!(unread(), 0);
}

/// A backfill re-ingests the ORIGINAL; it must not undo an edit already
/// applied through the edit path.
#[tokio::test]
async fn reingesting_an_original_keeps_its_applied_edit() {
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
    let inbound = ChannelInbound::new(
        client.messages().clone(),
        client.attachments().clone(),
        client.users().clone(),
    );

    let channel_id = ChannelId::new();
    let id = crate::domain::ids::MessageId::new();
    let proto = chat_v1::Message {
        id: id.to_string(),
        channel_id: channel_id.to_string(),
        author_id: UserId::new().to_string(),
        timestamp: 1_000,
        content: Some(chat_v1::message::Content {
            r#type: Some(chat_v1::message::content::Type::Text("original".into())),
        }),
        ..Default::default()
    };
    assert_eq!(
        inbound.ingest(&proto, channel_id, false).await,
        Ingested::Stored
    );

    let mut edited = client
        .messages()
        .get_message_local(&id)
        .unwrap()
        .expect("stored");
    edited.content = crate::domain::message::MessageContent::Text("edited".into());
    edited.is_edited = true;
    client.messages().update_message(&edited).unwrap();

    assert_eq!(
        inbound.ingest(&proto, channel_id, false).await,
        Ingested::Stored
    );
    let stored = client
        .messages()
        .get_message_local(&id)
        .unwrap()
        .expect("stored");
    assert!(
        matches!(stored.content, crate::domain::message::MessageContent::Text(ref t) if t == "edited")
    );
    assert!(stored.is_edited);
}

/// Catch-up can win while the newly subscribed live copy is buffered.
/// Announce its newly inserted row once; neither a repeated backfill nor
/// the encrypted stream duplicate may lose it or announce it again.
#[tokio::test]
async fn first_insert_announces_once_before_encrypted_stream_duplicate() {
    static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
    MOCK_KEYRING.call_once(|| {
        keyring::set_default_store(crate::system::keyring::test_credential_store());
    });
    let dir = tempfile::tempdir().unwrap();
    let client = ChatterClient::builder()
        .server_url("https://localhost:1")
        .data_dir(dir.path().to_str().unwrap())
        .build()
        .unwrap();
    let inbound = ChannelInbound::new(
        client.messages().clone(),
        client.attachments().clone(),
        client.users().clone(),
    );
    let channel = ChannelId::new();
    let mut events = client.subscribe_events();
    for first_live in [false, true] {
        let mut row = chat_v1::Message {
            id: crate::domain::ids::MessageId::new().to_string(),
            channel_id: channel.to_string(),
            author_id: UserId::new().to_string(),
            author_device_id: 1,
            timestamp: 12345,
            content: Some(chat_v1::message::Content {
                r#type: Some(chat_v1::message::content::Type::Text(
                    "first live message".into(),
                )),
            }),
            ..Default::default()
        };
        assert_eq!(
            inbound.ingest(&row, channel, first_live).await,
            Ingested::Stored
        );
        let DomainEvent::MessageReceived { message } = events.try_recv().unwrap() else {
            panic!("first insert winner must publish the stored message");
        };
        assert_eq!(message.id.to_string(), row.id);
        assert_eq!(message.as_text(), Some("first live message"));

        // An ordinary repeated catch-up also keeps existing history silent.
        assert_eq!(inbound.ingest(&row, channel, false).await, Ingested::Stored);
        // The cached decrypted row is sufficient to reject the duplicate
        // before a second sender-key decrypt (which would consume a key twice).
        row.content = Some(chat_v1::message::Content {
            r#type: Some(chat_v1::message::content::Type::Encrypted(
                chat_v1::EncryptedData {
                    ciphertext: GroupEncrypted {
                        channel_id: channel.to_string(),
                        timestamp: 12345,
                        ciphertext: vec![1, 2, 3],
                    }
                    .encode_to_vec(),
                    ..Default::default()
                },
            )),
        });
        assert_eq!(inbound.ingest(&row, channel, true).await, Ingested::Dropped);
        assert!(
            client
                .messages()
                .get_message_local(&message.id)
                .unwrap()
                .is_some()
        );
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }
}

/// Our own message from another device reaches storage by paths that skip
/// `ingest` (the SKDM retry, a DM or SyncSent copy); whichever path writes
/// it, it must not count as unread.
#[tokio::test]
async fn own_message_is_stored_read_on_any_path() {
    use crate::services::message::conversion::proto_to_domain_message_with_sender;
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
    let me = UserId::new();
    client
        .messages()
        .settings_repo
        .set_string(
            crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
            &me.to_string(),
        )
        .unwrap();

    let channel_id = ChannelId::new();
    let store = |sender: UserId| {
        let proto = chat_v1::Message {
            id: crate::domain::ids::MessageId::new().to_string(),
            channel_id: channel_id.to_string(),
            author_id: sender.to_string(),
            timestamp: 1_000,
            content: Some(chat_v1::message::Content {
                r#type: Some(chat_v1::message::content::Type::Text("hi".into())),
            }),
            ..Default::default()
        };
        let m =
            proto_to_domain_message_with_sender(&proto, Some(sender), ConversationType::Channel)
                .unwrap();
        assert!(!m.is_read);
        client.messages().store_message_local(&m).unwrap();
    };
    let unread = || {
        client
            .messages()
            .message_repo
            .get_unread_count(&channel_id, &ConversationType::Channel)
            .unwrap()
    };

    store(UserId::new());
    assert_eq!(
        unread(),
        1,
        "positive control: someone else's message is unread"
    );
    store(me);
    assert_eq!(unread(), 1, "our own message counted as unread");
}

/// Delete-for-me (and a peer's delete-for-everyone) removes the local row,
/// but the server still holds the message, so the next catch-up re-fetch
/// hands it back. It must stay deleted.
#[tokio::test]
async fn reingesting_a_deleted_message_keeps_it_deleted() {
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
    let inbound = ChannelInbound::new(
        client.messages().clone(),
        client.attachments().clone(),
        client.users().clone(),
    );

    let channel_id = ChannelId::new();
    let message_id = crate::domain::ids::MessageId::new();
    let proto = chat_v1::Message {
        id: message_id.to_string(),
        channel_id: channel_id.to_string(),
        author_id: UserId::new().to_string(),
        timestamp: 1_000,
        content: Some(chat_v1::message::Content {
            r#type: Some(chat_v1::message::content::Type::Text("hi".into())),
        }),
        ..Default::default()
    };

    assert_eq!(
        inbound.ingest(&proto, channel_id, false).await,
        Ingested::Stored
    );
    assert!(
        client
            .messages()
            .get_message_local(&message_id)
            .unwrap()
            .is_some(),
        "positive control: the message was stored"
    );

    client.messages().delete_message_local(&message_id).unwrap();
    let mut stale_malformed = proto.clone();
    stale_malformed.author_id = "bad-author".into();
    stale_malformed.parent_id = "bad-parent".into();
    stale_malformed.content = None;
    inbound
        .ingest_resynced(&stale_malformed, channel_id)
        .await
        .unwrap();
    inbound.ingest(&proto, channel_id, false).await;
    assert!(
        client
            .messages()
            .get_message_local(&message_id)
            .unwrap()
            .is_none(),
        "a re-fetched copy resurrected a deleted message"
    );
}
