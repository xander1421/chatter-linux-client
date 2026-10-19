    #[test]
    fn skdm_distribution_preserves_retryable_causes_and_validation_errors() {
        use crate::error::NetworkError;
        for cause in [
            NetworkError::Unavailable,
            NetworkError::Timeout,
            NetworkError::RateLimited,
            NetworkError::ConnectionFailed("offline".into()),
        ] {
            let error = sender_key_distribution_result(0, 2, Some(MessageError::Network(cause)))
                .unwrap_err();
            assert!(matches!(error, MessageError::Network(_)));
            assert_eq!(error.permanent_send_failure_reason(), None);
        }
        let error = sender_key_distribution_result(
            0,
            2,
            Some(MessageError::Network(NetworkError::InvalidRequest(
                "invalid recipient".into(),
            ))),
        )
        .unwrap_err();
        assert_eq!(
            error.permanent_send_failure_reason(),
            Some("Message rejected")
        );
        let mut failure = Some(error);
        super::super::server_dm::record_deposit_error(
            &mut failure,
            MessageError::Network(NetworkError::Timeout),
        );
        assert_eq!(
            sender_key_distribution_result(0, 2, failure)
                .unwrap_err()
                .permanent_send_failure_reason(),
            None
        );
        assert_eq!(
            sender_key_distribution_result(0, 2, None)
                .unwrap_err()
                .permanent_send_failure_reason(),
            None
        );
        assert!(sender_key_distribution_result(1, 2, None).is_ok());
        assert!(sender_key_distribution_result(0, 0, None).is_ok());
    }

    /// Own devices are the ones with a bundle; a response without any is none.
    #[test]
    fn own_devices_are_the_bundle_holders() {
        use crate::proto::chat_v1::{
            GetPreKeyBundleResponse, SignalPreKeyBundle, get_pre_key_bundle_response::Result,
        };
        let b = |device_id| SignalPreKeyBundle {
            device_id,
            ..Default::default()
        };
        let many = GetPreKeyBundleResponse {
            result: Some(Result::Devices(
                crate::proto::chat_v1::PreKeyBundleDeviceList {
                    bundles: vec![b(2), b(6), b(7), b(8)],
                    ..Default::default()
                },
            )),
            ..Default::default()
        };
        assert_eq!(super::bundle_device_ids(many), [2, 6, 7, 8].into());
        let one = GetPreKeyBundleResponse {
            result: Some(Result::Bundle(b(6))),
            ..Default::default()
        };
        assert_eq!(super::bundle_device_ids(one), [6].into());
        assert!(super::bundle_device_ids(GetPreKeyBundleResponse::default()).is_empty());
    }

    use std::time::{Duration, Instant};

    #[test]
    fn sender_key_request_is_sent_once_then_backs_off() {
        let mut gate = SenderKeyRequestGate::new();
        let key = (ChannelId::new(), UserId::new(), 6);
        let t0 = Instant::now();

        // A burst of misses from one history sync: one request.
        assert!(sender_key_request_due(&mut gate, key, t0));
        for _ in 0..20 {
            assert!(!sender_key_request_due(&mut gate, key, t0));
        }
        assert!(!sender_key_request_due(
            &mut gate,
            key,
            t0 + Duration::from_secs(29)
        ));
        // Unanswered: ask again after 30s, then the wait doubles.
        assert!(sender_key_request_due(
            &mut gate,
            key,
            t0 + Duration::from_secs(30)
        ));
        assert!(!sender_key_request_due(
            &mut gate,
            key,
            t0 + Duration::from_secs(89)
        ));
        assert!(sender_key_request_due(
            &mut gate,
            key,
            t0 + Duration::from_secs(90)
        ));

        // Another device of the same sender is gated on its own.
        assert!(sender_key_request_due(&mut gate, (key.0, key.1, 1), t0));

        // SKDM arrived (entry removed): the next miss asks immediately.
        gate.remove(&key);
        assert!(sender_key_request_due(
            &mut gate,
            key,
            t0 + Duration::from_secs(91)
        ));
    }

    /// Alice and Bob, each a client with its own store, both members of one
    /// channel. The TempDirs must outlive the clients.
    struct TwoMembers {
        alice: std::sync::Arc<crate::client::ChatterClient>,
        bob: std::sync::Arc<crate::client::ChatterClient>,
        alice_id: UserId,
        bob_id: UserId,
        channel_id: ChannelId,
        _dirs: (tempfile::TempDir, tempfile::TempDir),
    }

    fn two_members() -> TwoMembers {
        use crate::client::ChatterClient;
        use crate::domain::channel::{
            Channel, ChannelType, MemberRole, Membership, NotificationPreference,
        };

        let client = |dir: &tempfile::TempDir, user: UserId| {
            // Keyless, with the passphrase in a file beside the database, so
            // a test can reopen the same store (a mock keyring forgets it).
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
            let c = ChatterClient::builder()
                .server_url("https://localhost:1")
                .data_dir(dir.path().to_str().unwrap())
                .build()
                .expect("build client");
            let s = &c.messages().settings_repo;
            s.set(keys::CURRENT_USER_ID, &user.to_string()).unwrap();
            s.set(keys::CURRENT_DEVICE_ID, &1u32).unwrap();
            c
        };
        let (alice_id, bob_id) = (UserId::new(), UserId::new());
        let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (alice, bob) = (client(&da, alice_id), client(&db, bob_id));
        let channel_id = ChannelId::new();
        for c in [&alice, &bob] {
            let repo = &c.messages().channel_repo;
            repo.insert(&Channel::new(
                channel_id,
                "g".into(),
                ChannelType::Private,
                alice_id,
            ))
            .unwrap();
            for user_id in [alice_id, bob_id] {
                repo.insert_membership(&Membership {
                    channel_id,
                    user_id,
                    role: MemberRole::Member,
                    joined_at: chrono::Utc::now(),
                    last_read_message_id: None,
                    is_muted: false,
                    muted_until: None,
                    notification_preference: NotificationPreference::default(),
                })
                .unwrap();
            }
        }

        // Both hold the same verified state: Alice and Bob, revision 1.
        for c in [&alice, &bob] {
            seed_verified(c, channel_id, 1, &[alice_id, bob_id]);
        }

        TwoMembers {
            alice,
            bob,
            alice_id,
            bob_id,
            channel_id,
            _dirs: (da, db),
        }
    }

    /// Commit `members` as the channel's verified state at `revision` (the
    /// verifier itself is W2's and tested there; this only seeds storage).
    /// Returns the state hash.
    fn seed_verified(
        c: &crate::client::ChatterClient,
        channel_id: ChannelId,
        revision: u64,
        members: &[UserId],
    ) -> crate::services::channel::verified_state::StateHash {
        use crate::proto::chat_v1 as pb;
        use crate::services::channel::verified_state::{
            ChannelRole, Member, MemberStatus, MembershipState, PendingChange, Policy,
            VerifiedChange,
        };
        let state = MembershipState {
            channel_id: channel_id.to_string(),
            revision,
            members: members
                .iter()
                .map(|u| {
                    (
                        u.to_string(),
                        Member {
                            role: ChannelRole::Member,
                            status: MemberStatus::Active,
                            identity_key: [5; 33],
                        },
                    )
                })
                .collect(),
            policy: Policy::defaults(false),
            invites: Default::default(),
            disappearing_seconds: 0,
        };
        let hash = state.hash();
        let change = pb::ChannelChange {
            actions: vec![revision as u8],
            signature: vec![],
            invite_proof: vec![],
        };
        VerifiedMembershipRepository::new(c.messages().database.clone())
            .commit(
                &channel_id,
                &change,
                &VerifiedChange(PendingChange {
                    author_user_id: members[0].to_string(),
                    author_identity_key: [5; 33],
                    timestamp: 0,
                    new_state: state,
                    new_state_hash: hash,
                }),
            )
            .unwrap();
        hash
    }

    /// Signal hands sender keys to current group members only: a request from
    /// a non-member is dropped before any key is created; a member's is
    /// answered (its delivery fails here only because there is no server).
    #[tokio::test]
    async fn sender_key_request_is_answered_for_members_only() {
        let t = two_members();
        let alice = t.alice.messages();
        let outsider = UserId::new();

        assert!(matches!(
            alice
                .answer_sender_key_request(&t.channel_id, &outsider, &[])
                .await,
            Ok(false)
        ));
        assert!(
            !t.alice
                .signal()
                .has_sender_key(&t.channel_id)
                .await
                .unwrap(),
            "a non-member's request must not even create our key"
        );
        let skdm = t
            .alice
            .signal()
            .create_sender_key(&t.channel_id)
            .await
            .unwrap();
        assert!(
            alice
                .send_skdm_to_member(&t.channel_id, &outsider, &skdm)
                .await
                .is_err()
        );

        let answered = alice
            .answer_sender_key_request(&t.channel_id, &t.bob_id, &[])
            .await;
        assert!(
            matches!(answered, Err(MessageError::EncryptionFailed(ref e)) if !e.contains("not a current member")),
            "a member's request goes to delivery: {answered:?}"
        );

        // Removed by a verified change: no longer answered.
        seed_verified(&t.alice, t.channel_id, 2, &[t.alice_id]);
        assert!(matches!(
            alice
                .answer_sender_key_request(&t.channel_id, &t.bob_id, &[])
                .await,
            Ok(false)
        ));
    }

    /// Connected production send conversion -> delayed server envelope ->
    /// missing SKDM -> authenticated retry -> controls in both directions.
    #[tokio::test]
    async fn delayed_channel_receive_preserves_authenticated_reaction_identity() {
        use crate::domain::{
            MessageId,
            message::{ConversationType, MessageContent},
            reaction::ReactionScope,
        };
        use crate::inbox::{polling::device::dispatch_control_content, state::InboxStats};
        use crate::proto::content_v1;
        use crate::services::inbox_processor::InboxEventProcessor;
        use crate::services::message::channel_inbound::{ChannelInbound, Ingested};
        use prost::Message as _;
        let t = two_members();
        let sent = TimestampNanos::now().as_u64();
        let transport = sent + 445_413_938;
        let id = MessageId::new();
        let skdm = t
            .alice
            .signal()
            .create_sender_key(&t.channel_id)
            .await
            .unwrap();
        let dm = content_v1::DataMessage {
            body: "bold target".into(),
            message_id: String::new(),
            channel_id: t.channel_id.to_string(),
            timestamp: sent,
            ..Default::default()
        };
        let plaintext = content_v1::Content {
            timestamp: sent,
            content: Some(content_v1::content::Content::DataMessage(dm)),
            ..Default::default()
        }
        .encode_to_vec();
        let (encrypted, _) = t
            .alice
            .messages()
            .prepare_encrypted_content(
                &t.channel_id,
                &plaintext,
                crate::proto::chat_v1::message::Content {
                    r#type: Some(crate::proto::chat_v1::message::content::Type::Text(
                        "bold target".into(),
                    )),
                },
                true,
                sent,
            )
            .await
            .unwrap();
        let Some(crate::proto::chat_v1::message::content::Type::Encrypted(ciphertext)) =
            encrypted.r#type
        else {
            panic!("production send must encrypt");
        };
        let ge = content_v1::GroupEncrypted::decode(ciphertext.ciphertext.as_slice()).unwrap();
        let envelope = server_copy(&t, &id, transport, &ge);
        let mut outgoing = super::super::conversion::proto_to_outgoing_channel_message(
            &envelope,
            t.alice_id,
            ConversationType::Channel,
            sent,
            true,
        )
        .unwrap();
        outgoing.content = MessageContent::Text("bold target".into());
        t.alice
            .messages()
            .message_repo
            .insert_outgoing(&outgoing)
            .unwrap();
        // Plain public messages already use the server's accepted row clock.
        let plain = super::super::conversion::proto_to_outgoing_channel_message(
            &crate::proto::chat_v1::Message {
                content: Some(crate::proto::chat_v1::message::Content {
                    r#type: Some(crate::proto::chat_v1::message::content::Type::Text(
                        "public".into(),
                    )),
                }),
                ..envelope.clone()
            },
            t.alice_id,
            ConversationType::Channel,
            sent,
            false,
        )
        .unwrap();
        assert_eq!(plain.sent_at, transport);
        let inbound = ChannelInbound::new(t.bob.messages().clone(), t.bob.attachments().clone(), t.bob.users().clone());
        assert_eq!(
            inbound.ingest(&envelope, t.channel_id, false).await,
            Ingested::DeferredPendingSenderKey
        );
        let scope = ReactionScope::new(t.channel_id, ConversationType::Channel);
        // Target-missing is retained, never guessed from a nearby clock.
        assert!(
            !t.bob
                .messages()
                .record_inbound_reaction(&scope, &t.alice_id, &t.alice_id, sent, "😂", false, 2)
                .unwrap()
        );
        assert!(
            t.bob
                .messages()
                .message_repo
                .get_by_reaction_scope(&scope, &t.alice_id, sent)
                .unwrap()
                .is_none()
        );
        t.bob
            .signal()
            .process_sender_key(&t.alice_id, 1, &skdm)
            .await
            .unwrap();
        t.bob
            .messages()
            .retry_pending_group_decrypts(t.channel_id, t.alice_id, 1)
            .await;
        let received = t.bob.messages().get_message_local(&id).unwrap().unwrap();
        assert_eq!(received.sent_at, sent);
        assert_eq!(received.as_text(), Some("bold target"));
        assert_eq!(
            t.bob
                .messages()
                .message_repo
                .get_reaction_counts(&scope, &t.alice_id, sent)
                .unwrap()[0]
                .count,
            1
        );
        // Existing local ledger ownership survives; each peer's new control
        // addresses the canonical authenticated clock through real dispatch.
        t.alice
            .messages()
            .record_reaction_at(&scope, &t.alice_id, sent, &t.alice_id, "😂", false, 2)
            .unwrap();
        for (receiver, actor) in [(&t.alice, t.bob_id), (&t.bob, t.bob_id)] {
            let content = crate::services::build_reaction_content_with_channel(
                "😂",
                false,
                &t.alice_id.to_string(),
                sent,
                Some(&t.channel_id.to_string()),
            );
            let (tx, mut rx) = tokio::sync::broadcast::channel(8);
            let stats = Arc::new(parking_lot::RwLock::new(InboxStats::default()));
            dispatch_control_content(
                content.content.unwrap(),
                &actor,
                1,
                "reaction",
                transport + 100,
                &tx,
                &stats,
            );
            InboxEventProcessor::from_client(receiver)
                .process(&rx.try_recv().unwrap())
                .await;
        }
        for client in [&t.alice, &t.bob] {
            let snapshot = crate::services::conversation::ConversationHandle::new(
                crate::state::ActiveConversation::Channel(t.channel_id),
                client.clone(),
            )
            .reaction_snapshot(&t.alice_id, sent)
            .unwrap()
            .unwrap();
            assert_eq!(snapshot.sent_at, sent);
            assert_eq!(snapshot.counts[0].count, 2);
            assert!(snapshot.own_emojis.contains(&"😂".to_string()));
        }
        // Cached ciphertext does not authenticate its outer timestamp. Replay
        // preserves the exact mapping, cached row, and aggregate.
        inbound
            .ingest_resynced(&envelope, t.channel_id)
            .await
            .unwrap();
        assert_eq!(
            t.bob
                .messages()
                .get_message_local(&id)
                .unwrap()
                .unwrap()
                .sent_at,
            sent
        );
        let reopened = crate::client::ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(t._dirs.1.path().to_str().unwrap())
            .build()
            .unwrap();
        let repo =
            crate::storage::repositories::unified_message_repo::UnifiedMessageRepository::new(
                reopened.messages().database.clone(),
            );
        assert_eq!(
            repo.get_reaction_counts(&scope, &t.alice_id, sent).unwrap()[0].count,
            2
        );
        assert!(
            repo.get_by_reaction_scope(&scope, &t.alice_id, sent + 1)
                .unwrap()
                .is_none()
        );
        let foreign = ReactionScope::new(ChannelId::new(), ConversationType::Channel);
        assert!(
            repo.get_by_reaction_scope(&foreign, &t.alice_id, sent)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            repo.get_reaction_counts(&scope, &t.alice_id, sent).unwrap()[0].count,
            2
        );
    }

    /// Alice's group message as the server hands it to Bob.
    fn server_copy(
        t: &TwoMembers,
        id: &crate::domain::ids::MessageId,
        ts: u64,
        ge: &crate::proto::content_v1::GroupEncrypted,
    ) -> crate::proto::chat_v1::Message {
        use crate::proto::chat_v1;
        use prost::Message as _;
        chat_v1::Message {
            id: id.to_string(),
            channel_id: t.channel_id.to_string(),
            author_id: t.alice_id.to_string(),
            author_device_id: 1,
            timestamp: ts,
            content: Some(chat_v1::message::Content {
                r#type: Some(chat_v1::message::content::Type::Encrypted(
                    chat_v1::EncryptedData {
                        ciphertext: ge.encode_to_vec(),
                        ..Default::default()
                    },
                )),
            }),
            ..Default::default()
        }
    }
