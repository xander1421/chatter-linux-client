    /// Signal's sender-key retry, end to end between two real clients short of
    /// the network hop: Bob cannot read Alice's message sent before her SKDM,
    /// asks with a DecryptionErrorMessage, and Alice's pairwise resend lands in
    /// Bob's channel under the message's server id and (sender, sent_at).
    #[tokio::test]
    async fn pre_skdm_group_message_is_recovered_by_dem_and_resend() {
        use crate::domain::ids::MessageId;
        use crate::domain::message::{ConversationType, Message, MessageContent};
        use crate::proto::content_v1;
        use crate::services::message::channel_inbound::{ChannelInbound, Ingested};
        use prost::Message as _;

        let t = two_members();
        let (alice, bob) = (&t.alice, &t.bob);
        let (alice_id, bob_id, channel_id) = (t.alice_id, t.bob_id, t.channel_id);

        // Alice's client clock and the server's receive time differ by more
        // than a millisecond, and both are inside the 24h resend window.
        // The row and the GroupEncrypted envelope use her clock; the outer
        // Message.timestamp is the server's.
        let server_ts = TimestampNanos::now().as_u64();
        let client_ts = server_ts - 5_000_000_000;
        assert_ne!(client_ts / 1_000_000, server_ts / 1_000_000);

        let plaintext = content_v1::Content {
            content: Some(content_v1::content::Content::DataMessage(
                content_v1::DataMessage {
                    body: "sent before the SKDM".into(),
                    channel_id: channel_id.to_string(),
                    timestamp: client_ts,
                    ..Default::default()
                },
            )),
            timestamp: client_ts,
            ..Default::default()
        }
        .encode_to_vec();
        let (mut ge, _) = alice
            .signal()
            .encrypt_for_group(&channel_id, &plaintext)
            .await
            .unwrap();
        ge.timestamp = client_ts;
        let message_id = MessageId::new();
        alice
            .messages()
            .message_repo
            .insert_outgoing(&Message {
                id: message_id,
                ..Message::new_text(
                    channel_id,
                    ConversationType::Channel,
                    Some(alice_id),
                    "sent before the SKDM".into(),
                    client_ts,
                )
            })
            .unwrap();
        // Bob's device 1 was a recipient of this send. Without that record the
        // resend is refused: membership alone is not enough.
        crate::storage::repositories::message_receipt_repo::MessageReceiptRepository::new(
            alice.messages().database.clone(),
        )
        .snapshot_for_send(
            channel_id,
            ConversationType::Channel,
            alice_id,
            client_ts,
            &[(bob_id, crate::domain::ids::DeviceId(1))],
        )
        .unwrap();

        // Bob gets Alice's SKDM only now: it starts past the message.
        let skdm = alice.signal().create_sender_key(&channel_id).await.unwrap();
        bob.signal()
            .process_sender_key(&alice_id, 1, &skdm)
            .await
            .unwrap();
        let server_copy = server_copy(&t, &message_id, server_ts, &ge);
        let inbound = ChannelInbound::new(bob.messages().clone(), bob.attachments().clone());
        assert_eq!(
            inbound.ingest(&server_copy, channel_id, false).await,
            Ingested::RetryableFailure,
            "positive control: the pre-SKDM message is unreadable on its own"
        );

        // Bob's DEM names a sender-key message: no ratchet key, so Alice's
        // gateway resends instead of archiving their 1:1 session. It names
        // Alice's envelope clock, not the server receive time.
        let dem_bytes = crate::services::signal::SignalService::group_decryption_error(
            &ge.ciphertext,
            client_ts,
            1,
        )
        .unwrap();
        let dem =
            libsignal_protocol::DecryptionErrorMessage::try_from(dem_bytes.as_slice()).unwrap();
        assert!(dem.ratchet_key().is_none());
        assert_eq!(dem.device_id(), 1);
        let ts_ms = dem.timestamp().epoch_millis();
        assert_eq!(ts_ms, client_ts / 1_000_000);
        assert_ne!(ts_ms, server_ts / 1_000_000);

        // Alice's resend bounds: a non-member gets nothing back, and the
        // server's receive time is not a key she stored. Bob's other device
        // was not a recipient of the original send.
        assert!(
            alice
                .messages()
                .group_resend_content(&UserId::new(), 1, ts_ms)
                .unwrap()
                .is_none()
        );
        assert!(
            alice
                .messages()
                .group_resend_content(&bob_id, 1, server_ts / 1_000_000)
                .unwrap()
                .is_none(),
            "the server timestamp must not match the sender's sent_at"
        );
        assert!(
            alice
                .messages()
                .group_resend_content(&bob_id, 2, ts_ms)
                .unwrap()
                .is_none(),
            "a device that was not in the original send gets nothing"
        );
        let resent = alice
            .messages()
            .group_resend_content(&bob_id, 1, ts_ms)
            .unwrap()
            .expect("Alice finds the message she sent at that timestamp");

        // Bob ingests it through the pairwise sealed DataMessage path.
        let Some(content_v1::content::Content::DataMessage(dm)) =
            content_v1::Content::decode(resent.as_slice())
                .unwrap()
                .content
        else {
            panic!("resend is not a DataMessage");
        };
        let m = bob.messages();
        let gateway = crate::services::gateway::MessageGateway::new(
            (*m.signal_service).clone(),
            m.message_repo.clone(),
            m.channel_repo.clone(),
            m.user_repo.clone(),
            m.settings_repo.clone(),
            m.solicited_resends.clone(),
        );
        // Bob's ingest above asked Alice for this message.
        for _ in 0..10 {
            if !m.solicited_resends.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            m.solicited_resends
                .contains(&(channel_id, alice_id, client_ts / 1_000_000)),
            "the resend request is keyed by the sender's envelope timestamp"
        );
        assert!(
            !m.solicited_resends
                .contains(&(channel_id, alice_id, server_ts / 1_000_000)),
            "the resend request must not be keyed by the server receive time"
        );
        gateway
            .store_data_message(dm, Some(alice_id))
            .await
            .unwrap();
        let stored = m
            .message_repo
            .get_by_conversation_sender_timestamp(&channel_id, &alice_id, client_ts)
            .unwrap()
            .expect("the recovered message is filed under the sender's sent_at");
        assert_eq!(
            stored.content,
            MessageContent::Text("sent before the SKDM".into())
        );
        assert_eq!(
            stored.id, message_id,
            "the recovered message keeps the server id the other members hold"
        );

        // A later re-fetch of the server copy neither duplicates it nor asks
        // again (the first ingest above already recorded its one request).
        assert_eq!(
            inbound.ingest(&server_copy, channel_id, false).await,
            Ingested::Dropped
        );
        m.group_retry_requests.lock().clear();
        m.request_group_resend(channel_id, alice_id, 1, &server_copy, &ge.ciphertext)
            .await;
        assert!(m.group_retry_requests.lock().is_empty());
    }

    /// A resend is the original recipient set intersected with who is still a
    /// member. Carol joined after the send, Bob's second device was never a
    /// recipient, and Bob himself is excluded once he leaves.
    #[tokio::test]
    async fn resend_refuses_a_later_joiner_a_later_device_and_a_departed_member() {
        use crate::domain::ids::DeviceId;
        use crate::domain::message::{ConversationType, Message};
        use crate::storage::repositories::message_receipt_repo::MessageReceiptRepository;

        let t = two_members();
        let sent_at = TimestampNanos::now().as_u64();
        let alice = t.alice.messages();
        alice
            .message_repo
            .insert_outgoing(&Message::new_text(
                t.channel_id,
                ConversationType::Channel,
                Some(t.alice_id),
                "before carol joined".into(),
                sent_at,
            ))
            .unwrap();
        MessageReceiptRepository::new(alice.database.clone())
            .snapshot_for_send(
                t.channel_id,
                ConversationType::Channel,
                t.alice_id,
                sent_at,
                &[(t.bob_id, DeviceId(1))],
            )
            .unwrap();

        let carol = UserId::new();
        seed_verified(&t.alice, t.channel_id, 2, &[t.alice_id, t.bob_id, carol]);

        let ts_ms = sent_at / 1_000_000;
        assert!(
            alice
                .group_resend_content(&carol, 1, ts_ms)
                .unwrap()
                .is_none(),
            "a member who was not a recipient of the original send gets nothing"
        );
        assert!(
            alice
                .group_resend_content(&t.bob_id, 2, ts_ms)
                .unwrap()
                .is_none(),
            "a device added after the send gets nothing"
        );
        assert!(
            alice
                .group_resend_content(&t.bob_id, 1, ts_ms)
                .unwrap()
                .is_some(),
            "the original recipient device still in the channel is resent to"
        );
        // Our own devices: device 7 existed at send time, device 9 was linked later.
        let receipts = MessageReceiptRepository::new(alice.database.clone());
        receipts
            .snapshot_own_devices_for_send(
                t.channel_id,
                ConversationType::Channel,
                t.alice_id,
                sent_at,
                &[DeviceId(7)],
            )
            .unwrap();
        assert_eq!(
            receipts
                .targets_for_send(t.channel_id, ConversationType::Channel, t.alice_id, sent_at)
                .unwrap(),
            vec![(t.bob_id, DeviceId(1))],
            "own devices never join the receipt targets the aggregate counts"
        );
        assert!(
            alice
                .group_resend_content(&t.alice_id, 7, ts_ms)
                .unwrap()
                .is_some(),
            "our own device present at send time is resent to"
        );
        assert!(
            alice
                .group_resend_content(&t.alice_id, 9, ts_ms)
                .unwrap()
                .is_none(),
            "our own device linked after the send gets nothing"
        );
        assert!(
            alice
                .group_resend_content(&t.alice_id, 1, ts_ms)
                .unwrap()
                .is_none(),
            "this device never asks itself"
        );

        seed_verified(&t.alice, t.channel_id, 3, &[t.alice_id, carol]);
        assert!(
            alice
                .group_resend_content(&t.bob_id, 1, ts_ms)
                .unwrap()
                .is_none(),
            "a member who left since the send gets nothing"
        );
    }

    #[test]
    fn resend_targets_drop_departed_users_and_an_empty_original_set() {
        use crate::domain::ids::DeviceId;
        let (alice, bob, carol) = (UserId::new(), UserId::new(), UserId::new());
        let original = [(bob, DeviceId(1)), (bob, DeviceId(2)), (carol, DeviceId(1))];
        assert_eq!(
            super::resend_targets(&original, &[alice, bob]),
            vec![(bob, DeviceId(1)), (bob, DeviceId(2))]
        );
        assert!(super::resend_targets(&[], &[alice, bob]).is_empty());
    }

    /// F72: a channel member's SKDM carries their profile key to the other
    /// members — and only to them: not to a non-member who asked, not to a
    /// member they blocked — and the receiver keeps it without a friendship.
    #[tokio::test]
    async fn skdm_shares_profile_key_with_channel_members_only() {
        use crate::domain::user::{Friendship, FriendshipStatus};
        use crate::proto::content_v1;
        use prost::Message as _;

        let t = two_members();
        let (alice, bob) = (&t.alice, &t.bob);
        alice
            .messages()
            .settings_repo
            .set_bytes(keys::PROFILE_KEY, &[9; 32])
            .unwrap();
        let skdm = alice
            .signal()
            .create_sender_key(&t.channel_id)
            .await
            .unwrap();
        let carried = |to: &UserId| {
            let verified = alice
                .messages()
                .group_gate()
                .for_distribution(&t.channel_id)
                .unwrap();
            let bytes =
                alice
                    .messages()
                    .skdm_plaintext(&t.channel_id, &verified, &skdm, &to.to_string());
            match content_v1::Content::decode(bytes.as_slice())
                .unwrap()
                .content
            {
                Some(content_v1::content::Content::SenderKey(s)) => s.profile_key,
                _ => panic!("not an SKDM"),
            }
        };

        let key = carried(&t.bob_id).expect("a co-member gets Alice's profile key");
        assert_eq!(key, vec![9; 32]);
        assert!(
            carried(&UserId::new()).is_none(),
            "a non-member who asked gets none"
        );
        assert!(
            carried(&t.alice_id).is_none(),
            "our own devices already hold it"
        );

        // "My contacts": a co-member who is not Alice's friend gets none.
        use crate::services::profile_visibility::ProfileVisibility;
        ProfileVisibility::Contacts
            .store(&alice.messages().settings_repo)
            .unwrap();
        assert!(
            carried(&t.bob_id).is_none(),
            "a non-friend co-member under my contacts gets none"
        );
        ProfileVisibility::Everyone
            .store(&alice.messages().settings_repo)
            .unwrap();

        // Bob is not Alice's friend: the key still lands, once.
        let m = bob.messages();
        let gateway = crate::services::gateway::MessageGateway::new(
            (*m.signal_service).clone(),
            m.message_repo.clone(),
            m.channel_repo.clone(),
            m.user_repo.clone(),
            m.settings_repo.clone(),
            m.solicited_resends.clone(),
        );
        assert!(gateway.store_skdm_profile_key(&t.alice_id, Some(&key)));
        assert!(
            !gateway.store_skdm_profile_key(&t.alice_id, Some(&key)),
            "same key is no change"
        );
        assert_eq!(
            m.user_repo
                .get_friend_profile_key(&t.bob_id, &t.alice_id)
                .unwrap(),
            Some([9; 32])
        );

        // Once Alice blocks Bob, her SKDM to him carries no key.
        let am = alice.messages();
        am.user_repo.ensure_user_exists(&t.alice_id).unwrap();
        am.user_repo.ensure_user_exists(&t.bob_id).unwrap();
        am.user_repo
            .insert_friendship(&Friendship {
                requester_id: t.alice_id,
                addressee_id: t.bob_id,
                status: FriendshipStatus::Blocked,
                created_at: chrono::Utc::now(),
            })
            .unwrap();
        assert!(carried(&t.bob_id).is_none(), "a blocked member gets none");
    }

    /// A message parked for a sender key that never comes is not dropped
    /// silently when its wait runs out: Bob asks Alice to resend it, once.
    #[tokio::test]
    async fn parked_group_message_requests_resend_when_its_sender_key_never_arrives() {
        use crate::domain::ids::MessageId;
        use crate::services::message::channel_inbound::{ChannelInbound, Ingested};

        let t = two_members();
        let (ge, _) = t
            .alice
            .signal()
            .encrypt_for_group(
                &t.channel_id,
                &crate::network::validation::protovalidate::test_plaintext("lost key"),
            )
            .await
            .unwrap();
        let message_id = MessageId::new();
        let copy = server_copy(&t, &message_id, TimestampNanos::now().as_u64(), &ge);
        let m = t.bob.messages();
        let inbound = ChannelInbound::new(m.clone(), t.bob.attachments().clone());
        assert_eq!(
            inbound.ingest(&copy, t.channel_id, false).await,
            Ingested::DeferredPendingSenderKey
        );
        assert!(
            m.group_retry_requests.lock().is_empty(),
            "no resend request while the SKDM may still come"
        );

        m.expire_pending_group_decrypt(t.channel_id, t.alice_id, 1, &copy.id)
            .await;
        assert!(m.pending_group_decrypts.lock().is_empty());
        assert!(
            m.group_retry_requests
                .lock()
                .contains(&(copy.id.clone(), 1)),
            "the expired message is handed to request_group_resend"
        );

        // Nothing left parked: a later expiry asks nothing.
        m.group_retry_requests.lock().clear();
        m.expire_pending_group_decrypt(t.channel_id, t.alice_id, 1, &copy.id)
            .await;
        assert!(m.group_retry_requests.lock().is_empty());
    }

    /// A resync page with a message parked for its sender key goes on to the
    /// next message: one undecryptable message used to fail the whole resync,
    /// so the cursor never moved and every channel stalled behind it.
    #[tokio::test]
    async fn deferred_resync_requires_retry_while_later_messages_remain_ingestible() {
        use crate::domain::ids::MessageId;
        use crate::services::message::channel_inbound::ChannelInbound;

        let t = two_members();
        let m = t.bob.messages();
        let inbound = ChannelInbound::new(m.clone(), t.bob.attachments().clone());
        let (ge, _) = t
            .alice
            .signal()
            .encrypt_for_group(
                &t.channel_id,
                &crate::network::validation::protovalidate::test_plaintext("no key yet"),
            )
            .await
            .unwrap();
        let parked = server_copy(&t, &MessageId::new(), TimestampNanos::now().as_u64(), &ge);
        let next_id = MessageId::new();
        let next = crate::proto::chat_v1::Message {
            id: next_id.to_string(),
            channel_id: t.channel_id.to_string(),
            author_id: t.alice_id.to_string(),
            author_device_id: 1,
            timestamp: TimestampNanos::now().as_u64(),
            content: Some(crate::proto::chat_v1::message::Content {
                r#type: Some(crate::proto::chat_v1::message::content::Type::Text(
                    "after".into(),
                )),
            }),
            ..Default::default()
        };

        assert!(
            matches!(
                inbound.ingest_resynced(&parked, t.channel_id).await,
                Err(MessageError::ResyncPending)
            ),
            "a missing sender key is a pending application gap, not malformed truth"
        );
        inbound
            .ingest_resynced(&next, t.channel_id)
            .await
            .expect("the following usable message remains independently ingestible");
        assert_eq!(
            m.pending_group_decrypts
                .lock()
                .get(&(t.channel_id, t.alice_id, 1))
                .map(Vec::len),
            Some(1)
        );
        assert!(
            m.get_message_local(&next_id).unwrap().is_some(),
            "the following message is stored"
        );
    }

    /// Our own other device is a sender like any other: a message it sent that
    /// this device has no key for asks for its SKDM, and on expiry its resend.
    #[tokio::test]
    async fn message_from_our_other_device_without_a_key_requests_it() {
        use crate::domain::ids::MessageId;
        use crate::services::message::channel_inbound::{ChannelInbound, Ingested};

        let t = two_members();
        // Bob's store is Alice's second device (7); Alice's client is device 1.
        let m = t.bob.messages();
        m.settings_repo
            .set(keys::CURRENT_USER_ID, &t.alice_id.to_string())
            .unwrap();
        m.settings_repo.set(keys::CURRENT_DEVICE_ID, &7u32).unwrap();
        *m.own_devices.lock().await = Some((TimestampNanos::now().as_u64(), [1, 7].into()));
        let (ge, _) = t
            .alice
            .signal()
            .encrypt_for_group(
                &t.channel_id,
                &crate::network::validation::protovalidate::test_plaintext("from device 1"),
            )
            .await
            .unwrap();
        let copy = server_copy(&t, &MessageId::new(), TimestampNanos::now().as_u64(), &ge);
        let inbound = ChannelInbound::new(m.clone(), t.bob.attachments().clone());

        assert_eq!(
            inbound.ingest(&copy, t.channel_id, false).await,
            Ingested::DeferredPendingSenderKey
        );
        assert!(
            !m.should_request_sender_key(t.channel_id, t.alice_id, 1),
            "the ingest already sent the SenderKeyRequest, so the gate is closed"
        );
        m.expire_pending_group_decrypt(t.channel_id, t.alice_id, 1, &copy.id)
            .await;
        assert!(
            m.group_retry_requests
                .lock()
                .contains(&(copy.id.clone(), 1)),
            "an expired own-device message asks that device to resend it"
        );
    }
