    /// S25, channel 5dd10bcb: three messages sent before the SKDM Bob holds
    /// ("old counter") were given up as delivery issues, yet the channel's
    /// sync failed every 60 s. Giving up cleared the gate, so the next
    /// re-fetch counted from one again and held the checkpoint, and stream
    /// resync rejected the dropped row. A given-up message must stay settled
    /// across re-fetches while later messages keep arriving.
    #[tokio::test]
    async fn given_up_old_counter_messages_stop_holding_the_sync_checkpoint() {
        use crate::domain::ids::MessageId;
        use crate::domain::message::{ConversationType, MessageContent, SystemMessage};
        use crate::services::message::channel_inbound::{ChannelInbound, Ingested};
        use crate::sync::message_sync::holds_checkpoint;
        use prost::Message as _;

        let t = two_members();
        let m = t.bob.messages();
        let now = TimestampNanos::now().as_u64();
        let mut old = Vec::new();
        for i in 0..3u64 {
            let (ge, _) = t
                .alice
                .signal()
                .encrypt_for_group(
                    &t.channel_id,
                    &crate::network::validation::protovalidate::test_plaintext("before"),
                )
                .await
                .unwrap();
            old.push(server_copy(&t, &MessageId::new(), now - 10_000 + i, &ge));
        }
        // Bob's SKDM starts past those three, as on the S25.
        let skdm = t
            .alice
            .signal()
            .create_sender_key(&t.channel_id)
            .await
            .unwrap();
        t.bob
            .signal()
            .process_sender_key(&t.alice_id, 1, &skdm)
            .await
            .unwrap();
        let inbound = ChannelInbound::new(m.clone(), t.bob.attachments().clone(), t.bob.users().clone());

        // Each sync traversal of the channel: every old message, plus one new
        // message per pass. Gate deadlines are pulled forward so each pass is
        // a gated attempt, as one per 60 s sync would be.
        let mut passes = 0;
        loop {
            passes += 1;
            assert!(
                passes <= 8,
                "the channel never stopped holding its checkpoint"
            );
            for (deadline, _) in m.channel_decrypt_failures.lock().values_mut() {
                *deadline = std::time::Instant::now();
            }
            let mut held = false;
            for copy in &old {
                held |= holds_checkpoint(inbound.ingest(copy, t.channel_id, false).await);
            }
            let plaintext = crate::proto::content_v1::Content {
                content: Some(crate::proto::content_v1::content::Content::DataMessage(
                    crate::proto::content_v1::DataMessage {
                        body: "after".into(),
                        channel_id: t.channel_id.to_string(),
                        timestamp: now + passes,
                        ..Default::default()
                    },
                )),
                timestamp: now + passes,
                ..Default::default()
            }
            .encode_to_vec();
            let (ge, _) = t
                .alice
                .signal()
                .encrypt_for_group(&t.channel_id, &plaintext)
                .await
                .unwrap();
            let fresh = server_copy(&t, &MessageId::new(), now + passes, &ge);
            assert_eq!(
                inbound.ingest(&fresh, t.channel_id, false).await,
                Ingested::Stored,
                "later messages arrive while the old ones are unreadable"
            );
            if !held {
                break;
            }
        }
        assert!(
            passes > 1,
            "positive control: the resend got its bounded chance first"
        );

        // Settled for good: later re-fetches neither retry nor hold the
        // checkpoint, and a stream resync accepts them.
        for _ in 0..3 {
            for copy in &old {
                assert_eq!(
                    inbound.ingest(copy, t.channel_id, false).await,
                    Ingested::DeliveryIssue
                );
                inbound.ingest_resynced(copy, t.channel_id).await.unwrap();
            }
        }
        let rows = m
            .message_repo
            .list_for_conversation(&t.channel_id, &ConversationType::Channel, None, 50)
            .unwrap();
        let issues = rows
            .iter()
            .filter(|msg| {
                matches!(
                    msg.content,
                    MessageContent::System(SystemMessage::DeliveryIssue)
                )
            })
            .count();
        assert_eq!(issues, 1, "one marker for the gap, not one per re-fetch");
        assert_eq!(
            rows.iter()
                .filter(|msg| msg.content == MessageContent::Text("after".into()))
                .count() as u64,
            passes
        );
    }

    /// A peer's message from a device that no longer exists leaves a visible
    /// DeliveryIssue entry, once however often it is re-fetched.
    #[tokio::test]
    async fn message_from_a_retired_peer_device_leaves_one_delivery_issue() {
        use crate::domain::ids::MessageId;
        use crate::domain::message::{ConversationType, MessageContent, SystemMessage};
        use crate::services::message::channel_inbound::{ChannelInbound, Ingested};

        let t = two_members();
        let m = t.bob.messages();
        m.retired_peer_devices
            .lock()
            .insert((t.alice_id, 1), std::time::Instant::now());
        let (ge, _) = t
            .alice
            .signal()
            .encrypt_for_group(
                &t.channel_id,
                &crate::network::validation::protovalidate::test_plaintext("gone"),
            )
            .await
            .unwrap();
        let copy = server_copy(&t, &MessageId::new(), TimestampNanos::now().as_u64(), &ge);
        let inbound = ChannelInbound::new(m.clone(), t.bob.attachments().clone(), t.bob.users().clone());

        for _ in 0..2 {
            assert_eq!(
                inbound.ingest(&copy, t.channel_id, false).await,
                Ingested::DeliveryIssue
            );
        }
        let issues: Vec<_> = m
            .message_repo
            .list_for_conversation(&t.channel_id, &ConversationType::Channel, None, 50)
            .unwrap()
            .into_iter()
            .filter(|msg| {
                matches!(
                    msg.content,
                    MessageContent::System(SystemMessage::DeliveryIssue)
                )
            })
            .collect();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].sender_id, Some(t.alice_id));
    }

    /// An identified sender is the server's claim alone, and a first identity
    /// key is trusted on first use: the identified path must never start a
    /// session, or the server could pin its own key for a peer we never met.
    #[tokio::test]
    async fn identified_message_never_starts_a_session() {
        let t = two_members();
        let m = t.bob.messages();
        let gateway = crate::services::gateway::MessageGateway::new(
            (*m.signal_service).clone(),
            m.message_repo.clone(),
            m.channel_repo.clone(),
            m.user_repo.clone(),
            m.settings_repo.clone(),
            m.solicited_resends.clone(),
        );
        let refused = |r: Result<_, crate::error::ServiceError>| {
            matches!(r, Err(crate::error::ServiceError::Crypto(crate::error::CryptoError::Protocol(ref e)))
                if e.contains("No session available for an identified sender"))
        };
        assert!(refused(
            gateway
                .decrypt_identified_message(b"any", &t.alice_id, 1)
                .await
        ));

        // Positive control: with a session the message reaches libsignal.
        let record = libsignal_protocol::SessionRecord::new_fresh()
            .serialize()
            .unwrap();
        m.signal_service
            .key_store
            .store_session(&format!("{}:1", t.alice_id), &record)
            .unwrap();
        assert!(!refused(
            gateway
                .decrypt_identified_message(b"any", &t.alice_id, 1)
                .await
        ));
    }

    #[test]
    fn own_device_list_is_refetched_only_when_it_may_be_out_of_date() {
        let s = 1_000_000_000u64;
        let fetched = 10_000 * s;
        // A message from long before the list: the list is authoritative.
        assert!(!own_device_list_stale(
            fetched,
            fetched + 10 * s,
            fetched - 3600 * s
        ));
        // A message newer than the list (or within clock slack): maybe a new device.
        assert!(own_device_list_stale(
            fetched,
            fetched + 10 * s,
            fetched + 5 * s
        ));
        assert!(own_device_list_stale(
            fetched,
            fetched + 10 * s,
            fetched - 10 * s
        ));
        // A list over a minute old is refetched regardless.
        assert!(own_device_list_stale(
            fetched,
            fetched + 61 * s,
            fetched - 3600 * s
        ));
    }

    #[test]
    fn eviction_resends_are_capped_per_sender_per_minute() {
        let mut gate = EvictionResendGate::new();
        let (a, b) = (UserId::new(), UserId::new());
        let t0 = Instant::now();
        for _ in 0..10 {
            assert!(eviction_resend_due(&mut gate, a, t0));
        }
        assert!(!eviction_resend_due(
            &mut gate,
            a,
            t0 + Duration::from_secs(59)
        ));
        assert!(
            eviction_resend_due(&mut gate, b, t0),
            "another sender has its own budget"
        );
        assert!(
            eviction_resend_due(&mut gate, a, t0 + Duration::from_secs(60)),
            "a new minute, a new budget"
        );
    }

    /// F41: a channel message sent from ANOTHER of our devices reaches this
    /// one live and must be decrypted and stored (read — it is ours), not
    /// skipped as "already on screen", which is true only for this device's
    /// own sends.
    #[tokio::test]
    async fn live_message_from_our_other_device_is_stored_as_read() {
        use crate::domain::ids::MessageId;
        use crate::domain::message::{ConversationType, MessageContent};
        use crate::proto::content_v1;
        use crate::services::message::channel_inbound::{ChannelInbound, Ingested};
        use prost::Message as _;

        let t = two_members();
        // Bob's store is Alice's second device (7); Alice's client is device 1.
        let m = t.bob.messages();
        m.settings_repo
            .set(keys::CURRENT_USER_ID, &t.alice_id.to_string())
            .unwrap();
        m.settings_repo.set(keys::CURRENT_DEVICE_ID, &7u32).unwrap();
        let skdm = t
            .alice
            .signal()
            .create_sender_key(&t.channel_id)
            .await
            .unwrap();
        t.bob
            .signal()
            .process_sender_key(&t.alice_id, 1, &skdm)
            .await
            .unwrap();

        let body = content_v1::Content {
            content: Some(content_v1::content::Content::DataMessage(
                content_v1::DataMessage {
                    body: "from my tablet".into(),
                    channel_id: t.channel_id.to_string(),
                    timestamp: TimestampNanos::now().as_u64(),
                    ..Default::default()
                },
            )),
            ..Default::default()
        }
        .encode_to_vec();
        let (ge, _) = t
            .alice
            .signal()
            .encrypt_for_group(&t.channel_id, &body)
            .await
            .unwrap();
        let id = MessageId::new();
        let copy = server_copy(&t, &id, TimestampNanos::now().as_u64(), &ge);
        let inbound = ChannelInbound::new(m.clone(), t.bob.attachments().clone(), t.bob.users().clone());

        assert_eq!(
            inbound.ingest(&copy, t.channel_id, true).await,
            Ingested::Stored
        );
        let stored = m.get_message_local(&id).unwrap().expect("stored");
        assert!(matches!(stored.content, MessageContent::Text(ref b) if b == "from my tablet"));
        assert_eq!(
            m.message_repo
                .get_unread_count(&t.channel_id, &ConversationType::Channel)
                .unwrap(),
            0,
            "our own message is never unread"
        );

        // Positive control: the same message on the device that sent it is skipped.
        m.settings_repo.set(keys::CURRENT_DEVICE_ID, &1u32).unwrap();
        let copy = server_copy(&t, &MessageId::new(), TimestampNanos::now().as_u64(), &ge);
        assert_eq!(
            inbound.ingest(&copy, t.channel_id, true).await,
            Ingested::OwnMessage
        );
    }

    /// Our own previous install (device 1, no longer listed by the server)
    /// sent channel messages before it was retired. Nobody holds its sender
    /// key: the message is dropped, not parked, and nothing is asked of a
    /// device that does not exist.
    #[tokio::test]
    async fn message_from_our_own_retired_device_is_dropped_not_parked() {
        use crate::domain::ids::MessageId;
        use crate::services::message::channel_inbound::{ChannelInbound, Ingested};

        let t = two_members();
        // Bob's store is Alice's new install: same user, device 6.
        let m = t.bob.messages();
        m.settings_repo
            .set(keys::CURRENT_USER_ID, &t.alice_id.to_string())
            .unwrap();
        m.settings_repo.set(keys::CURRENT_DEVICE_ID, &6u32).unwrap();
        let now = TimestampNanos::now().as_u64();
        *m.own_devices.lock().await = Some((now, [6].into()));

        let (ge, _) = t
            .alice
            .signal()
            .encrypt_for_group(
                &t.channel_id,
                &crate::network::validation::protovalidate::test_plaintext("old install"),
            )
            .await
            .unwrap();
        let copy = server_copy(&t, &MessageId::new(), now - 3600 * 1_000_000_000, &ge);
        let inbound = ChannelInbound::new(m.clone(), t.bob.attachments().clone(), t.bob.users().clone());
        assert_eq!(
            inbound.ingest(&copy, t.channel_id, false).await,
            Ingested::DeliveryIssue
        );
        assert!(m.pending_group_decrypts.lock().is_empty());
        m.request_group_resend(t.channel_id, t.alice_id, 1, &copy, &ge.ciphertext)
            .await;
        assert!(
            m.group_retry_requests.lock().is_empty(),
            "no DEM to a retired device"
        );
        assert!(m.solicited_resends.is_empty());

        // Positive control: the same message from a device the server lists
        // waits for its sender key as usual.
        *m.own_devices.lock().await = Some((now, [1, 6].into()));
        assert_eq!(
            inbound.ingest(&copy, t.channel_id, false).await,
            Ingested::DeferredPendingSenderKey
        );
    }

    /// A pairwise resend may take the server id it carries only when this
    /// device asked that sender for the message at that sent_at, once.
    #[tokio::test]
    async fn resend_keeps_its_carried_id_only_when_solicited() {
        use crate::domain::ids::MessageId;
        use crate::proto::content_v1::DataMessage;

        let t = two_members();
        let m = t.bob.messages();
        let gateway = crate::services::gateway::MessageGateway::new(
            (*m.signal_service).clone(),
            m.message_repo.clone(),
            m.channel_repo.clone(),
            m.user_repo.clone(),
            m.settings_repo.clone(),
            m.solicited_resends.clone(),
        );
        let resend = |id: MessageId, ts: u64| DataMessage {
            body: "resent".into(),
            channel_id: t.channel_id.to_string(),
            timestamp: ts,
            message_id: id.to_string(),
            ..Default::default()
        };
        let stored_id = |ts: u64| {
            m.message_repo
                .get_by_conversation_sender_timestamp(&t.channel_id, &t.alice_id, ts)
                .unwrap()
                .expect("stored")
                .id
        };

        // Unsolicited: another member's id cannot be squatted.
        let foreign = MessageId::new();
        let ts1 = TimestampNanos::now().as_u64();
        gateway
            .store_data_message(resend(foreign, ts1), Some(t.alice_id))
            .await
            .unwrap();
        assert_ne!(stored_id(ts1), foreign);
        assert!(m.get_message_local(&foreign).unwrap().is_none());

        // Solicited from ANOTHER sender: still not honoured for Alice.
        let ts2 = ts1 + 7_000_000;
        m.solicited_resends
            .insert((t.channel_id, t.bob_id, ts2 / 1_000_000));
        let id2 = MessageId::new();
        gateway
            .store_data_message(resend(id2, ts2), Some(t.alice_id))
            .await
            .unwrap();
        assert_ne!(stored_id(ts2), id2);

        // Solicited from Alice at that sent_at: keeps the server id, and the
        // request is consumed.
        let ts3 = ts1 + 14_000_000;
        m.solicited_resends
            .insert((t.channel_id, t.alice_id, ts3 / 1_000_000));
        let id3 = MessageId::new();
        gateway
            .store_data_message(resend(id3, ts3), Some(t.alice_id))
            .await
            .unwrap();
        assert_eq!(stored_id(ts3), id3);
        assert!(
            !m.solicited_resends
                .contains(&(t.channel_id, t.alice_id, ts3 / 1_000_000))
        );
        // A solicited UUID naming a row held at another clock is refused;
        // the retained row keeps its clock and content.
        use crate::domain::message::{ConversationType, Message};
        let sent = ts3 + 20_000_000;
        let mut held = Message::new_text(
            t.channel_id,
            ConversationType::Channel,
            Some(t.alice_id),
            "held".into(),
            sent + 1,
        );
        held.id = MessageId::new();
        m.store_message_local(&held).unwrap();
        m.solicited_resends
            .insert((t.channel_id, t.alice_id, sent / 1_000_000));
        assert!(
            gateway
                .store_data_message(resend(held.id, sent), Some(t.alice_id))
                .await
                .is_err()
        );
        let kept = m.get_message_local(&held.id).unwrap().unwrap();
        assert_eq!(kept.sent_at, sent + 1);
        assert_eq!(kept.as_text(), Some("held"));
    }

    /// Past the parked cap the oldest message is evicted — and its sender is
    /// asked to resend it rather than it vanishing.
    #[tokio::test]
    async fn parked_message_evicted_by_the_cap_requests_a_resend() {
        use crate::domain::ids::MessageId;

        let t = two_members();
        let m = t.bob.messages();
        let (ge, _) = t
            .alice
            .signal()
            .encrypt_for_group(
                &t.channel_id,
                &crate::network::validation::protovalidate::test_plaintext("flood"),
            )
            .await
            .unwrap();
        let ts = TimestampNanos::now().as_u64();
        let copies: Vec<_> = (0..513)
            .map(|i| server_copy(&t, &MessageId::new(), ts + i, &ge))
            .collect();
        for c in &copies {
            m.defer_pending_group_decrypt(t.channel_id, t.alice_id, 1, c.clone());
        }
        assert_eq!(
            m.pending_group_decrypts
                .lock()
                .values()
                .map(Vec::len)
                .sum::<usize>(),
            512
        );
        let oldest = (copies[0].id.clone(), 1);
        for _ in 0..10 {
            if m.group_retry_requests.lock().contains(&oldest) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            m.group_retry_requests.lock().contains(&oldest),
            "the evicted message is handed to request_group_resend"
        );
    }
