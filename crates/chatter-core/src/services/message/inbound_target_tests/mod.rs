
    use super::*;
    use crate::domain::channel::{
        Channel, ChannelType, MemberRole, Membership, NotificationPreference,
    };
    use crate::storage::repositories::settings_repo::keys;

    struct Fixture {
        client: Arc<crate::client::ChatterClient>,
        _dir: tempfile::TempDir,
        me: UserId,
        /// A friend we DM, who is NOT in `channel`.
        bob: UserId,
        /// A friend we DM, who IS in `channel`.
        carol: UserId,
        channel: ChannelId,
    }

    /// Carol's message in our DM with her, and her message in `channel`.
    const DM_TS: u64 = 100;
    const CH_TS: u64 = 200;

    fn fixture() -> Fixture {
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_store(crate::system::keyring::test_credential_store());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = crate::client::ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        let (me, bob, carol, channel) = (
            UserId::new(),
            UserId::new(),
            UserId::new(),
            ChannelId::new(),
        );
        let svc = client.messages();
        svc.settings_repo
            .set(keys::CURRENT_USER_ID, &me.to_string())
            .unwrap();
        svc.settings_repo
            .set(keys::CURRENT_DEVICE_ID, &1u32)
            .unwrap();
        svc.channel_repo
            .insert(&Channel::new(channel, "g".into(), ChannelType::Private, me))
            .unwrap();
        for user_id in [me, carol] {
            svc.channel_repo
                .insert_membership(&Membership {
                    channel_id: channel,
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
        svc.message_repo
            .insert(&Message::new_text(
                ChannelId::from(carol),
                ConversationType::ServerDm,
                Some(carol),
                "dm".into(),
                DM_TS,
            ))
            .unwrap();
        svc.message_repo
            .insert(&Message::new_text(
                channel,
                ConversationType::Channel,
                Some(carol),
                "ch".into(),
                CH_TS,
            ))
            .unwrap();
        Fixture {
            client,
            _dir: dir,
            me,
            bob,
            carol,
            channel,
        }
    }

    #[tokio::test]
    async fn dm_delete_connected_processor_emits_actual_author_and_rejects_other_scope() {
        use crate::inbox::{polling::device::dispatch_control_content, state::InboxStats};
        use crate::services::inbox_processor::InboxEventProcessor;
        use parking_lot::RwLock;
        use tokio::sync::broadcast;
        let f = fixture();
        let svc = f.client.messages();
        let processor = InboxEventProcessor::from_client(&f.client);
        for (author, ts) in [(f.me, DM_TS), (f.carol, DM_TS)] {
            let target = if author == f.me {
                let msg = Message::new_text(
                    ChannelId::from(f.carol),
                    ConversationType::ServerDm,
                    Some(author),
                    "own".into(),
                    ts,
                );
                svc.message_repo.insert(&msg).unwrap();
                msg
            } else {
                svc.get_by_sender_timestamp(&author, ts).unwrap().unwrap()
            };
            let (tx, mut rx) = broadcast::channel(8);
            let stats = Arc::new(RwLock::new(InboxStats::default()));
            let mut domain = f.client.event_bus().subscribe_domain();
            let wire = crate::services::gateway::builders::build_targeted_delete_content(
                ts,
                Some(&author.to_string()),
                None,
            );
            let bytes = prost::Message::encode_to_vec(&wire);
            let decoded =
                <crate::proto::content_v1::Content as prost::Message>::decode(bytes.as_slice())
                    .unwrap();
            assert!(
                matches!(crate::services::gateway::MessageGateway::parse_content(decoded.clone()).unwrap(), crate::services::gateway::DmContent::DeleteForEveryone { target_author_aci: Some(value), .. } if value == author.to_string())
            );
            let content = decoded.content.unwrap();
            dispatch_control_content(content, &f.carol, 1, "synthetic", 2, &tx, &stats);
            processor.process(&rx.try_recv().unwrap()).await;
            match domain.try_recv().unwrap() {
                crate::inbox::handlers::DomainEvent::MessageDeleted {
                    message_id,
                    author_id,
                    channel_id,
                    target_sent_timestamp,
                } => {
                    assert_eq!(message_id, target.id);
                    assert_eq!(author_id, author);
                    assert_eq!(channel_id, Some(ChannelId::from(f.carol)));
                    assert_eq!(target_sent_timestamp, ts);
                }
                event => panic!("unexpected event: {event:?}"),
            }
            assert!(svc.message_repo.get(&target.id).unwrap().is_none());
            svc.message_repo.insert(&target).unwrap();
            assert!(
                svc.message_repo.get(&target.id).unwrap().is_none(),
                "history replay cannot resurrect live deletion"
            );
        }
        let unrelated_own = Message::new_text(
            ChannelId::from(f.bob),
            ConversationType::ServerDm,
            Some(f.me),
            "unrelated own".into(),
            303,
        );
        svc.message_repo.insert(&unrelated_own).unwrap();
        assert!(
            svc.apply_targeted_dm_delete(&f.carol, Some(&f.me.to_string()), 303)
                .unwrap()
                .is_none()
        );
        assert!(svc.message_repo.get(&unrelated_own.id).unwrap().is_some());
        let late = Message::new_text(
            ChannelId::from(f.carol),
            ConversationType::ServerDm,
            Some(f.me),
            "actual late DM".into(),
            303,
        );
        svc.message_repo.insert(&late).unwrap();
        assert!(svc.message_repo.get(&late.id).unwrap().is_none());
        // Bob cannot use Carol's timestamp to delete her DM or channel row.
        assert!(
            svc.apply_targeted_dm_delete(&f.bob, Some(&f.bob.to_string()), CH_TS)
                .unwrap()
                .is_none()
        );
        assert!(
            svc.get_by_sender_timestamp(&f.carol, CH_TS)
                .unwrap()
                .is_some()
        );
        let own_channel = Message::new_text(
            ChannelId::from(f.carol),
            ConversationType::Channel,
            Some(f.me),
            "collision".into(),
            302,
        );
        svc.message_repo.insert(&own_channel).unwrap();
        assert!(
            svc.apply_targeted_dm_delete(&f.carol, Some(&f.me.to_string()), 302)
                .unwrap()
                .is_none()
        );
        assert!(
            svc.apply_targeted_dm_delete(&f.me, Some(&f.me.to_string()), 302)
                .unwrap()
                .is_none()
        );
        assert!(svc.message_repo.get(&own_channel.id).unwrap().is_some());
        let late = Message::new_text(
            ChannelId::from(f.carol),
            ConversationType::ServerDm,
            Some(f.me),
            "actual late DM".into(),
            302,
        );
        svc.message_repo.insert(&late).unwrap();
        assert!(svc.message_repo.get(&late.id).unwrap().is_none());
        let handle = f
            .client
            .conversation(crate::state::ActiveConversation::ServerDm { peer_id: f.bob });
        assert!(handle.message_menu(&f.carol, CH_TS).is_err());
        assert!(handle.is_own_message(&f.carol, CH_TS).is_err());
        let channel_handle = f
            .client
            .conversation(crate::state::ActiveConversation::Channel(ChannelId::new()));
        assert!(channel_handle.message_menu(&f.carol, CH_TS).is_err());
    }

    #[tokio::test]
    async fn self_device_dm_delete_carries_destination_through_production_processor() {
        use crate::inbox::{polling::device::dispatch_control_content, state::InboxStats};
        use crate::services::inbox_processor::InboxEventProcessor;
        use parking_lot::RwLock;
        use tokio::sync::broadcast;
        let f = fixture();
        let svc = f.client.messages();
        let processor = InboxEventProcessor::from_client(&f.client);
        let (tx, mut rx) = broadcast::channel(16);
        let stats = Arc::new(RwLock::new(InboxStats::default()));
        for (ts, author) in [(910, f.me), (911, f.carol)] {
            let target = Message::new_text(
                ChannelId::from(f.carol),
                ConversationType::ServerDm,
                Some(author),
                "self target".into(),
                ts,
            );
            let other_author = if author == f.me { f.carol } else { f.me };
            let survivor = Message::new_text(
                ChannelId::from(f.carol),
                ConversationType::ServerDm,
                Some(other_author),
                "same timestamp".into(),
                ts,
            );
            let other_chat = Message::new_text(
                ChannelId::from(f.bob),
                ConversationType::ServerDm,
                Some(author),
                "other chat".into(),
                ts,
            );
            for msg in [&target, &survivor, &other_chat] {
                svc.message_repo.insert(msg).unwrap();
            }
            let wire = crate::services::gateway::builders::build_targeted_delete_content(
                ts,
                Some(&author.to_string()),
                None,
            );
            let copy = super::dm::own_device_copy(&f.carol, &wire, 1).unwrap();
            let bytes = prost::Message::encode_to_vec(&copy);
            let decoded =
                <crate::proto::content_v1::Content as prost::Message>::decode(bytes.as_slice())
                    .unwrap();
            assert!(
                matches!(crate::services::gateway::MessageGateway::parse_content(decoded.clone()).unwrap(),
                crate::services::gateway::DmContent::DeleteForEveryone {
                    target_author_aci: Some(a), destination_user_id: Some(d), ..
                } if a == author.to_string() && d == f.carol.to_string())
            );
            let mut domain = f.client.event_bus().subscribe_domain();
            dispatch_control_content(decoded.content.unwrap(), &f.me, 2, "self", 3, &tx, &stats);
            processor.process(&rx.try_recv().unwrap()).await;
            assert!(
                matches!(domain.try_recv().unwrap(), crate::inbox::handlers::DomainEvent::MessageDeleted {
                message_id, author_id, channel_id: Some(conversation), ..
            } if message_id == target.id && author_id == author && conversation == ChannelId::from(f.carol))
            );
            assert!(svc.message_repo.get(&target.id).unwrap().is_none());
            for msg in [&survivor, &other_chat] {
                assert!(svc.message_repo.get(&msg.id).unwrap().is_some());
            }
            svc.message_repo.insert(&target).unwrap();
            assert!(svc.message_repo.get(&target.id).unwrap().is_none());
        }
        // Missing, malformed, self destinations, third-party authors, and peer
        // destination claims cannot establish a self-device conversation.
        for (index, sender, author, destination) in [
            (0, f.me, Some(f.me.to_string()), None),
            (1, f.me, None, Some(f.carol.to_string())),
            (2, f.me, Some(f.me.to_string()), Some(String::new())),
            (3, f.me, Some(f.me.to_string()), Some("invalid".into())),
            (4, f.me, Some(f.me.to_string()), Some(f.me.to_string())),
            (5, f.me, Some(f.bob.to_string()), Some(f.carol.to_string())),
            (6, f.carol, Some(f.me.to_string()), Some(f.bob.to_string())),
            (
                7,
                f.carol,
                Some(f.me.to_string()),
                Some(f.carol.to_string()),
            ),
            (8, f.carol, Some(f.me.to_string()), Some(String::new())),
            (9, f.me, None, None),
            (10, f.me, Some(String::new()), Some(f.carol.to_string())),
            (11, f.me, Some("invalid".into()), Some(f.carol.to_string())),
        ] {
            let ts = 920 + index;
            let held = Message::new_text(
                ChannelId::from(f.carol),
                ConversationType::ServerDm,
                Some(f.me),
                "held protected target".into(),
                ts,
            );
            let cross_chat = Message::new_text(
                ChannelId::from(f.bob),
                ConversationType::ServerDm,
                Some(f.me),
                "held cross-chat target".into(),
                ts,
            );
            svc.message_repo.insert(&held).unwrap();
            svc.message_repo.insert(&cross_chat).unwrap();
            let mut content = crate::services::gateway::builders::build_targeted_delete_content(
                ts,
                author.as_deref(),
                None,
            );
            if let Some(crate::proto::content_v1::content::Content::DataMessage(dm)) =
                &mut content.content
            {
                dm.delete.as_mut().unwrap().destination_user_id = destination;
            }
            let mut domain = f.client.event_bus().subscribe_domain();
            dispatch_control_content(
                content.content.unwrap(),
                &sender,
                2,
                "forged",
                3,
                &tx,
                &stats,
            );
            processor.process(&rx.try_recv().unwrap()).await;
            assert!(domain.try_recv().is_err());
            assert!(svc.message_repo.get(&held.id).unwrap().is_some());
            assert!(svc.message_repo.get(&cross_chat.id).unwrap().is_some());
            svc.message_repo.delete(&held.id).unwrap();
            svc.message_repo.delete(&cross_chat.id).unwrap();
            for peer in [f.carol, f.bob] {
                for author in [f.me, f.carol, f.bob] {
                    let late = Message::new_text(
                        ChannelId::from(peer),
                        ConversationType::ServerDm,
                        Some(author),
                        "refused control must not queue".into(),
                        ts,
                    );
                    svc.message_repo.insert(&late).unwrap();
                    assert!(svc.message_repo.get(&late.id).unwrap().is_some());
                }
            }
        }
    }

    #[tokio::test]
    async fn queued_dm_deletes_block_either_author_catchup_without_crossing_type_or_chat() {
        let f = fixture();
        let svc = f.client.messages();
        for author in [f.me, f.carol] {
            let ts = if author == f.me { 701 } else { 702 };
            assert!(
                svc.apply_targeted_dm_delete(&f.carol, Some(&author.to_string()), ts)
                    .unwrap()
                    .is_none()
            );
            let other_chat = Message::new_text(
                ChannelId::from(f.bob),
                ConversationType::ServerDm,
                Some(author),
                "other".into(),
                ts,
            );
            svc.message_repo.insert(&other_chat).unwrap();
            assert!(svc.message_repo.get(&other_chat.id).unwrap().is_some());
            svc.message_repo.delete(&other_chat.id).unwrap();
            let channel = Message::new_text(
                ChannelId::from(f.carol),
                ConversationType::Channel,
                Some(author),
                "channel collision".into(),
                ts,
            );
            svc.message_repo.insert(&channel).unwrap();
            assert!(svc.message_repo.get(&channel.id).unwrap().is_some());
            svc.message_repo.delete(&channel.id).unwrap();
            let survivor = Message::new_text(
                ChannelId::from(f.carol),
                ConversationType::ServerDm,
                Some(if author == f.me { f.carol } else { f.me }),
                "collision survivor".into(),
                ts,
            );
            svc.message_repo.insert(&survivor).unwrap();
            assert!(svc.message_repo.get(&survivor.id).unwrap().is_some());
            let dm = Message::new_text(
                ChannelId::from(f.carol),
                ConversationType::ServerDm,
                Some(author),
                "catchup".into(),
                ts,
            );
            for _ in 0..2 {
                svc.message_repo.insert(&dm).unwrap();
                assert!(
                    svc.message_repo.get(&dm.id).unwrap().is_none(),
                    "redelivery must not resurrect deleted DM"
                );
            }
        }
    }

    #[tokio::test]
    async fn channel_delete_dispatch_is_exact_and_retains_typed_late_arrival_tombstone() {
        use crate::inbox::{polling::device::dispatch_control_content, state::InboxStats};
        use crate::services::inbox_processor::InboxEventProcessor;
        use parking_lot::RwLock;
        use tokio::sync::broadcast;

        let f = fixture();
        let svc = f.client.messages();
        let processor = InboxEventProcessor::from_client(&f.client);
        let (tx, mut rx) = broadcast::channel(16);
        let stats = Arc::new(RwLock::new(InboxStats::default()));
        let other_channel = ChannelId::new();
        // Insert other types first to expose the old global LIMIT 1 lookup.
        for (ts, held) in [(930, true), (931, false)] {
            let mut survivors = Vec::new();
            for (conversation, kind, author) in [
                (f.channel, ConversationType::ServerDm, f.carol),
                (f.channel, ConversationType::AnonDm, f.carol),
                (f.channel, ConversationType::AnonGroup, f.carol),
                (other_channel, ConversationType::Channel, f.carol),
                (f.channel, ConversationType::Channel, f.me),
            ] {
                let row =
                    Message::new_text(conversation, kind, Some(author), "survivor".into(), ts);
                svc.message_repo.insert(&row).unwrap();
                survivors.push(row);
            }
            let target = Message::new_text(
                f.channel,
                ConversationType::Channel,
                Some(f.carol),
                "target".into(),
                ts,
            );
            if held {
                svc.message_repo.insert(&target).unwrap();
            }
            let mut domain = f.client.event_bus().subscribe_domain();
            let author = f.carol.to_string();
            let channel = f.channel.to_string();
            let content = crate::services::gateway::builders::build_targeted_delete_content(
                ts,
                Some(author.as_str()),
                Some(&channel),
            );
            dispatch_control_content(
                content.content.unwrap(),
                &f.carol,
                1,
                "channel delete",
                2,
                &tx,
                &stats,
            );
            processor.process(&rx.try_recv().unwrap()).await;
            if held {
                match domain.try_recv().unwrap() {
                    crate::inbox::handlers::DomainEvent::MessageDeleted {
                        message_id,
                        author_id,
                        channel_id,
                        target_sent_timestamp,
                    } => {
                        assert_eq!(message_id, target.id);
                        assert_eq!(author_id, f.carol);
                        assert_eq!(channel_id, Some(f.channel));
                        assert_eq!(target_sent_timestamp, ts);
                    }
                    event => panic!("unexpected event: {event:?}"),
                }
            } else {
                assert!(domain.try_recv().is_err());
            }
            assert!(svc.message_repo.get(&target.id).unwrap().is_none());
            for _ in 0..2 {
                svc.message_repo.insert(&target).unwrap();
                assert!(svc.message_repo.get(&target.id).unwrap().is_none());
            }
            for row in survivors {
                assert!(svc.message_repo.get(&row.id).unwrap().is_some());
                svc.message_repo.delete(&row.id).unwrap();
                svc.message_repo.insert(&row).unwrap();
                assert!(svc.message_repo.get(&row.id).unwrap().is_some());
            }
        }
    }

    #[tokio::test]
    async fn channel_delete_dispatch_rejects_forged_author_and_destination_without_tombstone() {
        use crate::inbox::{polling::device::dispatch_control_content, state::InboxStats};
        use crate::services::inbox_processor::InboxEventProcessor;
        use parking_lot::RwLock;
        use tokio::sync::broadcast;

        let f = fixture();
        let svc = f.client.messages();
        let processor = InboxEventProcessor::from_client(&f.client);
        let (tx, mut rx) = broadcast::channel(16);
        let stats = Arc::new(RwLock::new(InboxStats::default()));
        for (index, author, destination) in [
            (0, Some(f.me.to_string()), None),
            (1, Some(String::new()), None),
            (2, Some("invalid".into()), None),
            (3, Some(f.carol.to_string()), Some(f.me.to_string())),
            (4, None, Some(String::new())),
            (5, None, None),
        ] {
            let ts = 940 + index;
            let targets: Vec<_> = [f.me, f.carol]
                .into_iter()
                .map(|sender| {
                    Message::new_text(
                        f.channel,
                        ConversationType::Channel,
                        Some(sender),
                        "protected".into(),
                        ts,
                    )
                })
                .collect();
            for row in &targets {
                svc.message_repo.insert(row).unwrap();
            }
            let mut domain = f.client.event_bus().subscribe_domain();
            let mut content = crate::services::gateway::builders::build_targeted_delete_content(
                ts,
                author.as_deref(),
                Some(&f.channel.to_string()),
            );
            if let Some(crate::proto::content_v1::content::Content::DataMessage(dm)) =
                &mut content.content
            {
                dm.delete.as_mut().unwrap().destination_user_id = destination;
            }
            dispatch_control_content(
                content.content.unwrap(),
                &f.carol,
                1,
                "forged channel delete",
                2,
                &tx,
                &stats,
            );
            if let Ok(event) = rx.try_recv() {
                processor.process(&event).await;
                panic!("forged channel delete reached the processor");
            }
            assert!(domain.try_recv().is_err());
            for row in targets {
                assert!(svc.message_repo.get(&row.id).unwrap().is_some());
                svc.message_repo.delete(&row.id).unwrap();
                svc.message_repo.insert(&row).unwrap();
                assert!(svc.message_repo.get(&row.id).unwrap().is_some());
            }
        }
    }

    #[test]
    fn channel_delete_rejects_forged_target_author_before_event() {
        use crate::inbox::{polling::device::dispatch_control_content, state::InboxStats};
        use parking_lot::RwLock;
        use tokio::sync::broadcast;
        let sender = UserId::new();
        let victim = UserId::new();
        let channel = ChannelId::new().to_string();
        let (tx, mut rx) = broadcast::channel(8);
        let stats = Arc::new(RwLock::new(InboxStats::default()));
        let content = crate::services::gateway::builders::build_targeted_delete_content(
            1,
            Some(&victim.to_string()),
            Some(&channel),
        );
        dispatch_control_content(
            content.content.unwrap(),
            &sender,
            1,
            "fixture",
            1,
            &tx,
            &stats,
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn exact_dm_delete_authorless_and_forged_author_admission() {
        let f = fixture();
        let svc = f.client.messages();
        let scope = ChannelId::from(f.carol);
        // A delete that names no author is refused and holds nothing back.
        assert!(
            svc.apply_targeted_dm_delete(&f.carol, None, 910)
                .unwrap()
                .is_none()
        );
        for author in [f.me, f.carol] {
            let message = Message::new_text(
                scope,
                ConversationType::ServerDm,
                Some(author),
                "kept".into(),
                910,
            );
            svc.message_repo.insert(&message).unwrap();
            assert!(svc.message_repo.get(&message.id).unwrap().is_some());
        }
        // Explicit missing own target queues only own, preserving the peer collision.
        assert!(
            svc.apply_targeted_dm_delete(&f.carol, Some(&f.me.to_string()), 911)
                .unwrap()
                .is_none()
        );
        let peer = Message::new_text(
            scope,
            ConversationType::ServerDm,
            Some(f.carol),
            "peer".into(),
            911,
        );
        svc.message_repo.insert(&peer).unwrap();
        assert!(svc.message_repo.get(&peer.id).unwrap().is_some());
        let own = Message::new_text(
            scope,
            ConversationType::ServerDm,
            Some(f.me),
            "own".into(),
            911,
        );
        svc.message_repo.insert(&own).unwrap();
        assert!(svc.message_repo.get(&own.id).unwrap().is_none());
        for author in [f.bob.to_string(), String::new(), "invalid".into()] {
            assert!(
                svc.apply_targeted_dm_delete(&f.carol, Some(&author), 912)
                    .unwrap()
                    .is_none()
            );
        }
        let foreign = Message::new_text(
            scope,
            ConversationType::ServerDm,
            Some(f.bob),
            "forged".into(),
            912,
        );
        svc.message_repo.insert(&foreign).unwrap();
        assert!(svc.message_repo.get(&foreign.id).unwrap().is_some());
        let peer_message = svc
            .get_by_sender_timestamp(&f.carol, DM_TS)
            .unwrap()
            .unwrap();
        assert_eq!(
            svc.apply_targeted_dm_delete(&f.carol, Some(&f.carol.to_string()), DM_TS)
                .unwrap()
                .unwrap()
                .id,
            peer_message.id
        );
    }

    #[tokio::test]
    async fn latest_preview_pages_past_typed_timer_notices_and_preserves_other_system_events() {
        use crate::domain::message::{MessageContent, SystemMessage};
        let f = fixture();
        let svc = f.client.messages();
        for ts in 1000..1040 {
            let mut notice = Message::new_text(
                ChannelId::from(f.carol),
                ConversationType::ServerDm,
                Some(f.carol),
                String::new(),
                ts,
            );
            notice.content =
                MessageContent::System(SystemMessage::DisappearingTimerChanged { seconds: 60 });
            svc.message_repo.insert(&notice).unwrap();
        }
        assert_eq!(
            f.client
                .latest_message(&ChannelId::from(f.carol), &ConversationType::ServerDm)
                .unwrap()
                .sent_at,
            DM_TS
        );
        let mut system = Message::new_text(
            ChannelId::from(f.carol),
            ConversationType::ServerDm,
            Some(f.carol),
            String::new(),
            1041,
        );
        system.content = MessageContent::System(SystemMessage::ChannelCreated);
        svc.message_repo.insert(&system).unwrap();
        assert_eq!(
            f.client
                .latest_message(&ChannelId::from(f.carol), &ConversationType::ServerDm)
                .unwrap()
                .id,
            system.id
        );
        let empty_chat = ChannelId::new();
        for ts in 2000..2300 {
            let mut notice = Message::new_text(
                empty_chat,
                ConversationType::ServerDm,
                Some(f.carol),
                String::new(),
                ts,
            );
            notice.content =
                MessageContent::System(SystemMessage::DisappearingTimerChanged { seconds: 0 });
            svc.message_repo.insert(&notice).unwrap();
        }
        assert!(
            f.client
                .latest_message(&empty_chat, &ConversationType::ServerDm)
                .is_none()
        );
    }

    #[tokio::test]
    async fn anonymous_dm_receiver_deletes_both_authors_and_queues_without_group_privilege() {
        use crate::domain::anon_dm::{AnonDm, AnonDmStatus};
        let f = fixture();
        let svc = f.client.messages();
        let anon = f.client.anon_dms();
        let mut dm = AnonDm::new_initiated(f.carol);
        dm.status = AnonDmStatus::Active;
        dm.peer_mailbox_pk = Some([7; 32]);
        anon.anon_dm_repo.insert_conversation(&dm).unwrap();
        for author in [f.me, f.carol] {
            let row = Message::new_text(
                dm.id.into(),
                ConversationType::AnonDm,
                Some(author),
                "same timestamp".into(),
                801,
            );
            svc.message_repo.insert(&row).unwrap();
        }
        for (author, ts) in [(f.me, 801), (f.carol, 801)] {
            let target = anon
                .get_message_by_sender_timestamp(&dm.id, &author, ts)
                .unwrap()
                .unwrap();
            let wire = crate::services::gateway::builders::build_targeted_delete_content(
                ts,
                Some(&author.to_string()),
                None,
            );
            let bytes = prost::Message::encode_to_vec(&wire);
            let content =
                <crate::proto::content_v1::Content as prost::Message>::decode(bytes.as_slice())
                    .unwrap();
            assert!(
                anon.process_incoming_content(&dm.id, &f.bob, &content)
                    .unwrap()
                    .is_empty()
            );
            assert!(svc.message_repo.get(&target.id).unwrap().is_some());
            let events = anon
                .process_incoming_content(&dm.id, &f.carol, &content)
                .unwrap();
            assert_eq!(events.len(), 1);
            if author == f.me {
                assert!(
                    anon.get_message_by_sender_timestamp(&dm.id, &f.carol, ts)
                        .unwrap()
                        .is_some()
                );
            }
            match &events[0] {
                crate::inbox::handlers::DomainEvent::MessageDeleted {
                    message_id,
                    author_id,
                    ..
                } => {
                    assert_eq!(*message_id, target.id);
                    assert_eq!(*author_id, author);
                }
                _ => panic!("expected deletion event"),
            }
            assert!(svc.message_repo.get(&target.id).unwrap().is_none());
            svc.message_repo.insert(&target).unwrap();
            assert!(svc.message_repo.get(&target.id).unwrap().is_none());
        }
        for (author, ts) in [(f.me, 803), (f.carol, 804)] {
            let content = crate::services::gateway::builders::build_targeted_delete_content(
                ts,
                Some(&author.to_string()),
                None,
            );
            assert!(
                anon.process_incoming_content(&dm.id, &f.carol, &content)
                    .unwrap()
                    .is_empty()
            );
            let wrong_type = Message::new_text(
                dm.id.into(),
                ConversationType::AnonGroup,
                Some(author),
                "group".into(),
                ts,
            );
            svc.message_repo.insert(&wrong_type).unwrap();
            assert!(svc.message_repo.get(&wrong_type.id).unwrap().is_some());
            svc.message_repo.delete(&wrong_type.id).unwrap();
            let target = Message::new_text(
                dm.id.into(),
                ConversationType::AnonDm,
                Some(author),
                "late".into(),
                ts,
            );
            for _ in 0..2 {
                svc.message_repo.insert(&target).unwrap();
                assert!(svc.message_repo.get(&target.id).unwrap().is_none());
            }
        }
        // An explicit missing delete leaves the other author's collision alive.
        for (timestamp, explicit) in [(820, true)] {
            let content = crate::services::gateway::builders::build_targeted_delete_content(
                timestamp,
                explicit.then(|| f.me.to_string()).as_deref(),
                None,
            );
            assert!(
                anon.process_incoming_content(&dm.id, &f.carol, &content)
                    .unwrap()
                    .is_empty()
            );
            let survivor_author = if explicit { f.carol } else { f.me };
            let survivor = Message::new_text(
                dm.id.into(),
                ConversationType::AnonDm,
                Some(survivor_author),
                "survivor".into(),
                timestamp,
            );
            svc.message_repo.insert(&survivor).unwrap();
            assert!(svc.message_repo.get(&survivor.id).unwrap().is_some());
        }
        let peer_message = Message::new_text(
            dm.id.into(),
            ConversationType::AnonDm,
            Some(f.carol),
            "peer author".into(),
            822,
        );
        svc.message_repo.insert(&peer_message).unwrap();
        let content = crate::services::gateway::builders::build_targeted_delete_content(
            822,
            Some(&f.carol.to_string()),
            None,
        );
        assert_eq!(
            anon.process_incoming_content(&dm.id, &f.carol, &content)
                .unwrap()
                .len(),
            1
        );
        for (author, channel) in [(f.bob, None), (f.me, Some("forged-channel"))] {
            let target = Message::new_text(
                dm.id.into(),
                ConversationType::AnonDm,
                Some(author),
                "refused".into(),
                823,
            );
            svc.message_repo.insert(&target).unwrap();
            let content = crate::services::gateway::builders::build_targeted_delete_content(
                823,
                Some(&author.to_string()),
                channel,
            );
            assert!(
                anon.process_incoming_content(&dm.id, &f.carol, &content)
                    .unwrap()
                    .is_empty()
            );
            assert!(svc.message_repo.get(&target.id).unwrap().is_some());
        }
        for (timestamp, conversation, kind) in [
            (824, ChannelId::new(), ConversationType::AnonDm),
            (825, dm.id.into(), ConversationType::AnonGroup),
        ] {
            let target = Message::new_text(
                conversation,
                kind,
                Some(f.me),
                "other scope".into(),
                timestamp,
            );
            svc.message_repo.insert(&target).unwrap();
            let content = crate::services::gateway::builders::build_targeted_delete_content(
                timestamp,
                Some(&f.me.to_string()),
                None,
            );
            assert!(
                anon.process_incoming_content(&dm.id, &f.carol, &content)
                    .unwrap()
                    .is_empty()
            );
            assert!(svc.message_repo.get(&target.id).unwrap().is_some());
            let late = Message::new_text(
                dm.id.into(),
                ConversationType::AnonDm,
                Some(f.me),
                "actual late anon DM".into(),
                timestamp,
            );
            svc.message_repo.insert(&late).unwrap();
            assert!(svc.message_repo.get(&late.id).unwrap().is_none());
        }
        let destination_claim_target = Message::new_text(
            dm.id.into(),
            ConversationType::AnonDm,
            Some(f.me),
            "destination forgery".into(),
            826,
        );
        svc.message_repo.insert(&destination_claim_target).unwrap();
        let mut destination_claim =
            crate::services::gateway::builders::build_targeted_delete_content(
                826,
                Some(&f.me.to_string()),
                None,
            );
        if let Some(crate::proto::content_v1::content::Content::DataMessage(data)) =
            &mut destination_claim.content
        {
            data.delete.as_mut().unwrap().destination_user_id = Some(f.bob.to_string());
        }
        assert!(
            anon.process_incoming_content(&dm.id, &f.carol, &destination_claim)
                .unwrap()
                .is_empty()
        );
        assert!(
            svc.message_repo
                .get(&destination_claim_target.id)
                .unwrap()
                .is_some()
        );
        let catchup = crate::services::gateway::build_text_content("late peer", 804);
        let events = anon
            .process_incoming_content(&dm.id, &f.carol, &catchup)
            .unwrap();
        assert!(
            !events.iter().any(|event| matches!(
                event,
                crate::inbox::handlers::DomainEvent::MessageReceived { .. }
            )),
            "deleted catchup cannot emit a live bubble"
        );
        // The common group backend keeps the author-only lookup.
        let group_id = crate::domain::anon_group::AnonGroupId::new();
        let own = Message::new_text(
            group_id.into(),
            ConversationType::AnonGroup,
            Some(f.me),
            "own group".into(),
            805,
        );
        svc.message_repo.insert(&own).unwrap();
        assert!(
            !f.client
                .anon_groups()
                .delete_message_by_timestamp(&group_id, &f.carol, 805)
                .unwrap()
        );
        assert!(svc.message_repo.get(&own.id).unwrap().is_some());
        let group_target = Message::new_text(
            group_id.into(),
            ConversationType::AnonGroup,
            Some(f.carol),
            "group target".into(),
            807,
        );
        let type_collision = Message::new_text(
            group_id.into(),
            ConversationType::AnonDm,
            Some(f.carol),
            "type collision".into(),
            807,
        );
        svc.message_repo.insert(&group_target).unwrap();
        svc.message_repo.insert(&type_collision).unwrap();
        let control = crate::services::gateway::builders::build_targeted_delete_content(
            807,
            Some(&f.carol.to_string()),
            None,
        );
        let group_events = crate::inbox::handlers::base_anon::dispatch_common_content(
            &group_id,
            &f.carol,
            &control,
            f.client.anon_groups().as_ref(),
        );
        assert!(
            matches!(group_events, crate::inbox::handlers::base_anon::CommonDispatchOutcome::Handled(events) if events.len() == 1)
        );
        assert!(
            svc.message_repo
                .get(&group_target.id)
                .unwrap()
                .unwrap()
                .is_deleted
        );
        assert!(
            !svc.message_repo
                .get(&type_collision.id)
                .unwrap()
                .unwrap()
                .is_deleted
        );
        let only_collision = Message::new_text(
            group_id.into(),
            ConversationType::AnonDm,
            Some(f.carol),
            "no group target".into(),
            808,
        );
        svc.message_repo.insert(&only_collision).unwrap();
        assert!(
            !f.client
                .anon_groups()
                .delete_message_by_timestamp(&group_id, &f.carol, 808)
                .unwrap()
        );
        assert!(
            !svc.message_repo
                .get(&only_collision.id)
                .unwrap()
                .unwrap()
                .is_deleted
        );
        // Likewise the channel receiver cannot borrow the DM peer permission.
        let channel = Message::new_text(
            f.channel,
            ConversationType::Channel,
            Some(f.me),
            "own channel".into(),
            806,
        );
        svc.message_repo.insert(&channel).unwrap();
        assert!(
            svc.apply_channel_delete(&f.channel, &f.carol, 806)
                .unwrap()
                .is_none()
        );
        assert!(svc.message_repo.get(&channel.id).unwrap().is_some());
    }


#[path = "reactions.rs"]
mod reactions;

#[cfg(feature = "media")]
mod stable_call_delete_tests {
    include!("../stable_call_delete_tests.rs");
}
