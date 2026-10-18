use super::*;

/// A mailbox copy sealed for a sibling device is recognised as not ours;
/// one sealed for this device, or one naming no recipient, is not — those
/// are genuine failures and keep their marker and report.
#[test]
fn a_copy_for_a_sibling_device_is_not_a_failure_of_ours() {
    use libsignal_protocol::{
        CiphertextMessageType, DeviceId, KeyPair, ProtocolAddress, SignalMessage,
    };
    use rand::TryRngCore;

    let mut rng = rand::rngs::OsRng.unwrap_err();
    let us: UserId = "14142135-6237-3095-0488-016887242096".parse().unwrap();
    let address = |uuid: &str, device: u8| {
        ProtocolAddress::new(uuid.to_owned(), DeviceId::new(device).unwrap())
    };
    let sender = address("16180339-8874-9894-8482-045868343656", 2);
    let mut sealed_for = |recipient: Option<&ProtocolAddress>| {
        let identity = KeyPair::generate(&mut rng).public_key.into();
        SignalMessage::new(
            4,
            &[7u8; 32],
            recipient.map(|r| (&sender, r)),
            KeyPair::generate(&mut rng).public_key,
            11,
            0,
            &[0u8; 20],
            &identity,
            &identity,
            b"",
        )
        .unwrap()
        .serialized()
        .to_vec()
    };
    let desync = |ciphertext: Vec<u8>| {
        crate::error::ServiceError::Crypto(crate::error::CryptoError::SessionDesync {
            sender_uuid: sender.name().to_owned(),
            sender_device_id: 2,
            msg_type: CiphertextMessageType::Whisper as u8,
            ciphertext,
            reason: "Bad Mac".into(),
            group_id: None,
        })
    };
    let local = local_recipient_address(&us, 6).unwrap();
    let local = Some(local.as_slice());

    let sibling = sealed_for(Some(&address(&us.to_string(), 7)));
    assert!(addressed_to_another_device(&desync(sibling), local));

    let ours = sealed_for(Some(&address(&us.to_string(), 6)));
    assert!(!addressed_to_another_device(&desync(ours), local));

    let unaddressed = sealed_for(None);
    assert!(!addressed_to_another_device(&desync(unaddressed), local));
}

/// A friend's DM that arrived over the Shield mailbox is the same
/// conversation as one that arrived over sealed sender: both come back
/// from the key the chat list, badge and history read. It used to be
/// filed under a mailbox-only key nothing read, and vanished on reload.
#[test]
fn mailbox_dm_joins_the_friends_one_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let db =
        Arc::new(crate::storage::Database::open(&dir.path().join("t.db"), "testpass").unwrap());
    db.run_migrations().unwrap();
    let message_repo = Arc::new(MessageRepository::new(db.clone()));
    let (me, friend) = (UserId::new(), UserId::new());
    let conversation = ChannelId::from(friend);

    let streamed = Message::new_text(
        conversation,
        ConversationType::ServerDm,
        Some(friend),
        "streamed".into(),
        1,
    );
    message_repo.insert(&streamed).unwrap();

    let (event_tx, _rx) = broadcast::channel(16);
    let text = |body: &str, timestamp| DmContent::Text {
        body: body.into(),
        timestamp,
        attachments: vec![],
        profile_key: None,
        quote: None,
        thread_id: None,
        expire_timer: 0,
        stated_timer: None,
        timer_update: false,
    };
    for (content, id) in [(text("via mailbox", 2), "m-1"), (text("", 3), "m-2")] {
        dispatch_dm_content(
            content,
            &friend,
            1,
            friend,
            id,
            0,
            &Some(me),
            &event_tx,
            &message_repo,
            &Arc::new(AttachmentRepository::new(db.clone())),
            &Arc::new(UserRepository::new(db.clone())),
            &DmConversationManager::new(db.clone()),
            None,
        );
    }

    let mut thread: Vec<String> = message_repo
        .list_for_channel(&conversation, &ConversationType::ServerDm, None, 10)
        .unwrap()
        .iter()
        .filter_map(|m| m.as_text().map(str::to_string))
        .collect();
    thread.sort();
    // The empty body is a profile-key handshake: no row.
    assert_eq!(thread, ["streamed", "via mailbox"]);
}
