use super::*;

/// A server `message_updated` edit goes through the same edit logic as a
/// sender-key one: author only, ordered by the edit's time, within 48h,
/// held until its message arrives, and a re-delivery changes nothing.
#[tokio::test]
async fn server_channel_edits_follow_the_edit_rules() {
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
    // The resync path reads the current user (own-message handling).
    let settings = &client.messages().settings_repo;
    settings
        .set(
            crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
            &UserId::new().to_string(),
        )
        .unwrap();
    settings
        .set(
            crate::storage::repositories::settings_repo::keys::CURRENT_DEVICE_ID,
            &1u32,
        )
        .unwrap();
    let inbound = ChannelInbound::new(
        client.messages().clone(),
        client.attachments().clone(),
        client.users().clone(),
    );
    let mut events = client.messages().event_bus.subscribe_domain();
    let mut edits = || {
        let mut n = 0;
        while let Ok(e) = events.try_recv() {
            n += usize::from(matches!(e, DomainEvent::MessageEdited { .. }));
        }
        n
    };
    let window = MessageService::EDIT_MAX_AGE_NS;
    let (channel_id, author, stranger) = (ChannelId::new(), UserId::new(), UserId::new());
    let sent = 1_700_000_000_000_000_000u64;
    let message = |id: &crate::domain::ids::MessageId, by: UserId, body: &str, edited_at: u64| {
        chat_v1::Message {
            id: id.to_string(),
            channel_id: channel_id.to_string(),
            author_id: by.to_string(),
            timestamp: sent,
            edited: edited_at > 0,
            edited_at,
            content: Some(chat_v1::message::Content {
                r#type: Some(chat_v1::message::content::Type::Text(body.into())),
            }),
            ..Default::default()
        }
    };
    let id = crate::domain::ids::MessageId::new();
    let body = || {
        client
            .messages()
            .get_message_local(&id)
            .unwrap()
            .map(|m| m.content)
    };
    let text = |s: &str| Some(crate::domain::message::MessageContent::Text(s.into()));

    // Early: held, then applied when the message arrives.
    assert_eq!(
        inbound
            .ingest_edit(&message(&id, author, "early", sent + 10), channel_id)
            .await,
        Ingested::Dropped
    );
    assert_eq!(body(), None, "an edit alone does not create the message");
    assert_eq!(
        inbound
            .ingest(&message(&id, author, "original", 0), channel_id, false)
            .await,
        Ingested::Stored
    );
    assert_eq!(body(), text("early"));
    edits();

    assert_eq!(
        inbound
            .ingest_edit(&message(&id, author, "newer", sent + 30), channel_id)
            .await,
        Ingested::Stored
    );
    assert_eq!(edits(), 1);
    // Older than the edit shown, a re-delivery of it, another author's,
    // and one past 48h: all ignored, nothing announced.
    inbound
        .ingest_edit(&message(&id, author, "older", sent + 20), channel_id)
        .await;
    inbound
        .ingest_edit(&message(&id, author, "newer", sent + 30), channel_id)
        .await;
    inbound
        .ingest_edit(&message(&id, stranger, "hijack", sent + 40), channel_id)
        .await;
    inbound
        .ingest_edit(&message(&id, author, "late", sent + window + 1), channel_id)
        .await;
    assert_eq!(edits(), 0);
    let stored = client.messages().get_message_local(&id).unwrap().unwrap();
    assert_eq!(Some(stored.content), text("newer"));
    assert_eq!(
        stored.edited_at,
        Some(chrono::DateTime::from_timestamp_nanos((sent + 30) as i64))
    );

    // A resync re-fetch of the applied edit: no event, no edited_at churn.
    inbound
        .ingest_resynced(&message(&id, author, "newer", sent + 30), channel_id)
        .await
        .unwrap();
    assert_eq!(edits(), 0);
    assert_eq!(
        client
            .messages()
            .get_message_local(&id)
            .unwrap()
            .unwrap()
            .edited_at,
        stored.edited_at
    );
    // Resync must not turn a mismatched cached target or malformed
    // authoritative reaction snapshot into successful application.
    assert!(matches!(
        inbound
            .ingest_resynced(&message(&id, author, "newer", sent + 30), ChannelId::new())
            .await,
        Err(MessageError::InvalidContent(_))
    ));
    let mut bad_reactions = message(&id, author, "newer", sent + 30);
    bad_reactions.reactions.push(chat_v1::ReactionUpdate {
        target_author_aci: stranger.to_string(),
        target_sent_timestamp: sent,
        reactor_id: stranger.to_string(),
        emoji: "👍".into(),
        ..Default::default()
    });
    assert!(matches!(
        inbound.ingest_resynced(&bad_reactions, channel_id).await,
        Err(MessageError::InvalidContent(_))
    ));
    assert_eq!(edits(), 0);
    // A cached row still validates its envelope before clearing reactions.
    client
        .messages()
        .record_reaction(
            &crate::domain::reaction::ReactionScope::new(channel_id, ConversationType::Channel),
            &author,
            sent,
            &stranger,
            "👍",
            false,
        )
        .unwrap();
    let replay = message(&id, author, "newer", sent + 30);
    let mut malformed = Vec::new();
    let mut bad = replay.clone();
    bad.author_id = "bad-author".into();
    malformed.push(bad);
    let mut bad = replay.clone();
    bad.author_id = stranger.to_string();
    malformed.push(bad);
    let mut bad = replay.clone();
    bad.parent_id = "bad-parent".into();
    malformed.push(bad);
    let mut bad = replay.clone();
    bad.parent_id = crate::domain::ids::MessageId::new().to_string();
    malformed.push(bad);
    let mut bad = replay.clone();
    bad.timestamp += 1;
    malformed.push(bad);
    let mut bad = replay.clone();
    bad.channel_id = ChannelId::new().to_string();
    malformed.push(bad);
    let mut bad = replay.clone();
    bad.content = None;
    malformed.push(bad);
    let mut bad = replay.clone();
    bad.content = Some(chat_v1::message::Content { r#type: None });
    malformed.push(bad);
    let mut bad = replay.clone();
    bad.author_device_id = 1;
    bad.content = Some(chat_v1::message::Content {
        r#type: Some(chat_v1::message::content::Type::Encrypted(
            chat_v1::EncryptedData {
                ciphertext: vec![0xff],
                ..Default::default()
            },
        )),
    });
    malformed.push(bad);
    // A valid first reaction must not mutate the ledger before a later invalid entry.
    let mut bad = replay.clone();
    bad.reactions.push(chat_v1::ReactionUpdate {
        target_author_aci: author.to_string(),
        target_sent_timestamp: sent,
        reactor_id: stranger.to_string(),
        emoji: "❤".into(),
        ..Default::default()
    });
    bad.reactions.push(chat_v1::ReactionUpdate {
        target_author_aci: author.to_string(),
        target_sent_timestamp: sent,
        reactor_id: "bad-reactor".into(),
        emoji: "👍".into(),
        ..Default::default()
    });
    malformed.push(bad);
    for bad in malformed {
        assert!(matches!(
            inbound.ingest_resynced(&bad, channel_id).await,
            Err(MessageError::InvalidContent(_))
        ));
        let ledger = client
            .messages()
            .message_repo
            .get_reactions(
                &crate::domain::reaction::ReactionScope::new(channel_id, ConversationType::Channel),
                &author,
                sent,
            )
            .unwrap();
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0].emoji.value, "👍");
        assert_eq!(body(), text("newer"));
    }
    // Cached ciphertext is inspected structurally, never decrypted again.
    // The transport timestamp need not equal the stored DataMessage clock.
    let mut encrypted_replay = replay.clone();
    encrypted_replay.author_device_id = 1;
    encrypted_replay.timestamp = sent + 100;
    encrypted_replay.content = Some(chat_v1::message::Content {
        r#type: Some(chat_v1::message::content::Type::Encrypted(
            chat_v1::EncryptedData {
                ciphertext: GroupEncrypted {
                    channel_id: channel_id.to_string(),
                    ciphertext: vec![1, 2, 3],
                    timestamp: sent,
                }
                .encode_to_vec(),
                ..Default::default()
            },
        )),
    });
    for group in [
        GroupEncrypted::default(),
        GroupEncrypted {
            channel_id: ChannelId::new().to_string(),
            ciphertext: vec![1],
            timestamp: sent,
        },
        GroupEncrypted {
            channel_id: channel_id.to_string(),
            ciphertext: Vec::new(),
            timestamp: sent,
        },
    ] {
        let mut bad = encrypted_replay.clone();
        bad.content = Some(chat_v1::message::Content {
            r#type: Some(chat_v1::message::content::Type::Encrypted(
                chat_v1::EncryptedData {
                    ciphertext: group.encode_to_vec(),
                    ..Default::default()
                },
            )),
        });
        assert!(matches!(
            inbound.ingest_resynced(&bad, channel_id).await,
            Err(MessageError::InvalidContent(_))
        ));
        assert_eq!(
            client
                .messages()
                .message_repo
                .get_reactions(
                    &crate::domain::reaction::ReactionScope::new(
                        channel_id,
                        ConversationType::Channel
                    ),
                    &author,
                    sent
                )
                .unwrap()
                .len(),
            1
        );
    }
    encrypted_replay.author_id = author.to_string().to_uppercase();
    inbound
        .ingest_resynced(&encrypted_replay, channel_id)
        .await
        .unwrap();
    // Replies use the same parent UUID in the server envelope and local row.
    let reply_id = crate::domain::ids::MessageId::new();
    let mut reply = message(&reply_id, author, "reply", 0);
    reply.parent_id = id.to_string();
    reply.timestamp = sent + 3;
    assert_eq!(
        inbound.ingest(&reply, channel_id, false).await,
        Ingested::Stored
    );
    reply.content = encrypted_replay.content.clone();
    reply.author_device_id = 1;
    reply.timestamp = sent + 103;
    inbound.ingest_resynced(&reply, channel_id).await.unwrap();
    assert_eq!(
        client
            .messages()
            .get_message_local(&reply_id)
            .unwrap()
            .unwrap()
            .thread_id,
        Some(id)
    );
    assert_eq!(body(), text("newer"));
    assert!(
        client
            .messages()
            .message_repo
            .get_reactions(
                &crate::domain::reaction::ReactionScope::new(channel_id, ConversationType::Channel),
                &author,
                sent
            )
            .unwrap()
            .len()
            == 1
    );
    // Anonymous cached rows cannot bypass malformed reaction validation.
    let anonymous_id = crate::domain::ids::MessageId::new();
    let mut anonymous = message(&anonymous_id, author, "anonymous", 0);
    anonymous.author_id.clear();
    anonymous.timestamp = sent + 2;
    assert_eq!(
        inbound.ingest(&anonymous, channel_id, false).await,
        Ingested::Stored
    );
    inbound
        .ingest_resynced(&anonymous, channel_id)
        .await
        .unwrap();
    anonymous.reactions.push(chat_v1::ReactionUpdate {
        target_author_aci: "bad-target".into(),
        target_sent_timestamp: sent + 2,
        reactor_id: "bad-reactor".into(),
        emoji: "👍".into(),
        ..Default::default()
    });
    assert!(matches!(
        inbound.ingest_resynced(&anonymous, channel_id).await,
        Err(MessageError::InvalidContent(_))
    ));
    anonymous.reactions[0].reactor_id = stranger.to_string();
    assert!(matches!(
        client
            .messages()
            .reconcile_channel_reactions(&anonymous, channel_id),
        Err(MessageError::InvalidContent(_))
    ));
    // Same channel UUID/author/clock is insufficient authority for a
    // channel snapshot to replace a foreign conversation-kind ledger.
    for kind in [
        ConversationType::ServerDm,
        ConversationType::AnonDm,
        ConversationType::AnonGroup,
    ] {
        let mut foreign = Message::new_text(
            channel_id,
            kind.clone(),
            Some(author),
            "foreign history".into(),
            sent,
        );
        let scope = crate::domain::reaction::ReactionScope::of(&foreign);
        client.messages().store_message_local(&foreign).unwrap();
        client
            .messages()
            .record_reaction(&scope, &author, sent, &stranger, "👍", false)
            .unwrap();
        let plain = message(&foreign.id, author, "foreign history", 0);
        let encrypted = chat_v1::Message {
            author_device_id: 1,
            content: encrypted_replay.content.clone(),
            ..plain.clone()
        };
        for envelope in [&plain, &encrypted] {
            assert!(matches!(
                inbound.ingest_resynced(envelope, channel_id).await,
                Err(MessageError::InvalidContent(_))
            ));
            assert!(matches!(
                client
                    .messages()
                    .reconcile_channel_reactions(envelope, channel_id),
                Err(MessageError::InvalidContent(_))
            ));
        }
        foreign = client
            .messages()
            .get_message_local(&foreign.id)
            .unwrap()
            .unwrap();
        assert_eq!(
            foreign.content,
            MessageContent::Text("foreign history".into())
        );
        assert_eq!(foreign.reaction_counts[0].count, 1);
    }
    // Valid replay still succeeds after invalid rows were rejected.
    inbound.ingest_resynced(&replay, channel_id).await.unwrap();
    assert_eq!(edits(), 0);
    // A resync of a message never seen stores it, edit included.
    let unseen = crate::domain::ids::MessageId::new();
    inbound
        .ingest_resynced(
            &chat_v1::Message {
                timestamp: sent + 1,
                ..message(&unseen, author, "edited", sent + 5)
            },
            channel_id,
        )
        .await
        .unwrap();
    let unseen = client
        .messages()
        .get_message_local(&unseen)
        .unwrap()
        .expect("stored");
    assert_eq!(Some(unseen.content), text("edited"));
}
