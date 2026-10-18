use super::*;
use crate::client::ChatterClient;
use crate::domain::message::{ConversationType, MessageContent};

/// An unanswered resend used to hold the channel's sync checkpoint
/// forever ("message ingestion must be retried" on every sync). The gate
/// must report enough failures to give up within a bounded schedule.
#[test]
fn decrypt_failure_gate_reaches_give_up_on_a_bounded_schedule() {
    let mut gate = DecryptFailureGate::new();
    let key = (ChannelId::new(), UserId::new(), 1, [7u8; 32]);
    let mut now = std::time::Instant::now();
    let start = now;
    let mut attempts = 0;
    while attempts < RESEND_GIVE_UP_ATTEMPTS {
        attempts = record_decrypt_failure(&mut gate, key, now);
        // The next attempt is allowed once the gate's deadline passes.
        now = gate[&key].0;
    }
    assert_eq!(attempts, RESEND_GIVE_UP_ATTEMPTS);
    assert!(now - start <= std::time::Duration::from_secs(30 * 60));
}

#[test]
fn a_new_edit_is_not_a_duplicate_of_the_original() {
    let mut original = Message::new_text(
        ChannelId::new(),
        ConversationType::Channel,
        None,
        "original".into(),
        1_000_000,
    );
    let edit = chat_v1::Message {
        edited: true,
        edited_at: 2_000_000,
        ..Default::default()
    };
    assert!(is_newer_edit(&edit, &original));
    original.edited_at = Some(chrono::DateTime::from_timestamp_nanos(2_000_000));
    assert!(!is_newer_edit(&edit, &original));
    let older = chat_v1::Message {
        edited_at: 1_000_000,
        ..edit.clone()
    };
    assert!(!is_newer_edit(&older, &original));
    assert!(!is_newer_edit(&chat_v1::Message::default(), &original));
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
    let inbound = ChannelInbound::new(client.messages().clone(), client.attachments().clone());

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
    let inbound = ChannelInbound::new(client.messages().clone(), client.attachments().clone());

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
    let inbound = ChannelInbound::new(client.messages().clone(), client.attachments().clone());
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
    let inbound = ChannelInbound::new(client.messages().clone(), client.attachments().clone());

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

#[tokio::test]
async fn resync_clock_conflict_keeps_local_history_and_allows_later_rows() {
    keyring::set_default_store(crate::system::keyring::test_credential_store());
    let dir = tempfile::tempdir().unwrap();
    let client = ChatterClient::builder()
        .server_url("https://localhost:1")
        .data_dir(dir.path().to_str().unwrap())
        .build()
        .unwrap();
    let inbound = ChannelInbound::new(client.messages().clone(), client.attachments().clone());
    let channel = ChannelId::new();
    let id = crate::domain::ids::MessageId::new();
    let mut row = chat_v1::Message {
        id: id.to_string(),
        channel_id: channel.to_string(),
        author_id: UserId::new().to_string(),
        timestamp: 1_000,
        content: Some(chat_v1::message::Content {
            r#type: Some(chat_v1::message::content::Type::Text("retained".into())),
        }),
        ..Default::default()
    };
    assert_eq!(inbound.ingest(&row, channel, false).await, Ingested::Stored);
    let original = client.messages().get_message_local(&id).unwrap().unwrap();
    row.timestamp += 1;
    let rejected = inbound.ingest_resynced(&row, channel).await;
    assert!(
        matches!(&rejected, Err(MessageError::InvalidContent(reason))
        if reason == "resync message identity mismatch")
    );
    assert!(
        crate::services::channel_stream::resync_row_complete(
            rejected,
            &channel.to_string(),
            &row.id
        )
        .unwrap()
    );
    let retained = client.messages().get_message_local(&id).unwrap().unwrap();
    assert_eq!(retained.sent_at, original.sent_at);
    assert_eq!(retained.content, original.content);
    let next_id = crate::domain::ids::MessageId::new();
    row.id = next_id.to_string();
    assert!(
        crate::services::channel_stream::resync_row_complete(
            inbound.ingest_resynced(&row, channel).await,
            &channel.to_string(),
            &row.id
        )
        .unwrap()
    );
    assert!(
        client
            .messages()
            .get_message_local(&next_id)
            .unwrap()
            .is_some()
    );
    assert!(
        !crate::services::channel_stream::resync_row_complete(
            Err(MessageError::ResyncPending),
            &channel.to_string(),
            &row.id
        )
        .unwrap()
    );
}

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
    let inbound = ChannelInbound::new(client.messages().clone(), client.attachments().clone());
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
