use super::*;

    fn reactors(f: &Fixture, ts: u64) -> Vec<UserId> {
        f.client
            .messages()
            .message_repo
            .get_reactions(
                &crate::domain::reaction::ReactionScope::new(
                    if ts == DM_TS {
                        ChannelId::from(f.carol)
                    } else {
                        f.channel
                    },
                    if ts == DM_TS {
                        ConversationType::ServerDm
                    } else {
                        ConversationType::Channel
                    },
                ),
                &f.carol,
                ts,
            )
            .unwrap()
            .into_iter()
            .map(|r| r.user_id)
            .collect()
    }

    #[tokio::test]
    async fn inbox_reaction_delta_uses_action_time_and_skips_replay() {
        use crate::inbox::events::InboxEvent;
        use crate::services::inbox_processor::InboxEventProcessor;

        let f = fixture();
        let processor = InboxEventProcessor::from_client(&f.client);
        let mut domain = f.client.event_bus().subscribe_domain();
        let action_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap() as u64 - 60_000_000_000;
        let target_ns = action_ns - 600_000_000_000;
        for (scope, event) in [
            (
                crate::domain::reaction::ReactionScope::new(
                    ChannelId::from(f.carol), ConversationType::ServerDm,
                ),
                InboxEvent::DmReaction {
                    conversation_peer_id: Some(f.carol), peer_id: f.carol,
                    emoji: "😂".into(), remove: false,
                    target_author_aci: crate::domain::reaction::encode_dm_reaction_target(
                        &f.me, &f.me,
                    ),
                    target_sent_timestamp: target_ns, timestamp: action_ns,
                },
            ),
            (
                crate::domain::reaction::ReactionScope::new(
                    f.channel, ConversationType::Channel,
                ),
                InboxEvent::ChannelReaction {
                    channel_id: f.channel, sender_id: f.carol,
                    emoji: "😂".into(), remove: false,
                    target_author_aci: f.me.to_string(),
                    target_sent_timestamp: target_ns, timestamp: action_ns,
                },
            ),
        ] {
            f.client.messages().message_repo.insert(&Message::new_text(
                scope.id, scope.kind.clone(), Some(f.me), "own".into(), target_ns,
            )).unwrap();
            processor.process(&event).await;
            match domain.try_recv().unwrap() {
                crate::inbox::handlers::DomainEvent::ReactionAdded {
                    scope: emitted_scope, sender_id, sent_at_ns, reactor_id,
                    emoji, timestamp,
                } => {
                    assert_eq!(emitted_scope, scope);
                    assert_eq!(sender_id, f.me);
                    assert_eq!(sent_at_ns, target_ns);
                    assert_eq!(reactor_id, f.carol);
                    assert_eq!(emoji, "😂");
                    assert_eq!(timestamp, action_ns / 1_000_000);
                }
                other => panic!("unexpected event: {other:?}"),
            }
            processor.process(&event).await;
            assert!(domain.try_recv().is_err(), "replayed reaction emitted a new delta");
        }
    }

    /// Connected control dispatch -> processor -> scoped ledger reproduction.
    /// Missing display membership refuses even a locally held target; hydration
    /// changes admission only for a new delivery, without replaying the refusal.
    #[tokio::test]
    async fn channel_reaction_connected_membership_hydration_diagnosis() {
        use crate::inbox::{
            events::InboxEvent, polling::device::dispatch_control_content, state::InboxStats,
        };
        use crate::services::inbox_processor::InboxEventProcessor;
        use parking_lot::RwLock;
        use tokio::sync::broadcast;

        let f = fixture();
        let svc = f.client.messages();
        let scope =
            crate::domain::reaction::ReactionScope::new(f.channel, ConversationType::Channel);
        let stored_reactors = || {
            svc.message_repo
                .get_reactions(&scope, &f.carol, CH_TS)
                .unwrap()
                .into_iter()
                .map(|r| r.user_id)
                .collect::<Vec<_>>()
        };
        assert!(
            svc.record_reaction_at(&scope, &f.carol, CH_TS, &f.me, "👍", false, 1)
                .unwrap()
        );
        let processor = InboxEventProcessor::from_client(&f.client);
        let (tx, mut rx) = broadcast::channel(8);
        let mut domain = f.client.event_bus().subscribe_domain();
        let stats = Arc::new(RwLock::new(InboxStats::default()));
        let reaction = crate::services::build_reaction_content_with_channel(
            "👍",
            false,
            &f.carol.to_string(),
            CH_TS,
            Some(&f.channel.to_string()),
        )
        .content
        .unwrap();
        dispatch_control_content(reaction.clone(), &f.bob, 1, "synthetic", 2, &tx, &stats);
        let event = rx.try_recv().unwrap();
        assert!(matches!(event, InboxEvent::ChannelReaction { .. }));
        processor.process(&event).await;
        assert_eq!(stored_reactors(), vec![f.me]);
        assert!(
            domain.try_recv().is_err(),
            "refusal emits no reaction domain event"
        );

        // Same repository operation used by get_all_channel_members on open.
        let mut roster = svc.channel_repo.list_members(&f.channel).unwrap();
        let mut member = roster[0].clone();
        member.user_id = f.bob;
        roster.push(member);
        svc.channel_repo
            .replace_memberships(&f.channel, &roster)
            .unwrap();
        assert_eq!(
            stored_reactors(),
            vec![f.me],
            "hydration cannot replay a refused reaction"
        );
        dispatch_control_content(
            reaction.clone(),
            &f.bob,
            1,
            "synthetic-replay",
            2,
            &tx,
            &stats,
        );
        processor.process(&rx.try_recv().unwrap()).await;
        let stored = stored_reactors();
        assert_eq!(stored.len(), 2);
        assert!(stored.contains(&f.me) && stored.contains(&f.bob));
        assert!(matches!(
            domain.try_recv().unwrap(),
            crate::inbox::handlers::DomainEvent::ReactionAdded { .. }
        ));
        dispatch_control_content(reaction, &f.bob, 1, "synthetic-duplicate", 2, &tx, &stats);
        processor.process(&rx.try_recv().unwrap()).await;
        assert_eq!(stored_reactors().len(), 2);
        assert!(
            domain.try_recv().is_err(),
            "duplicate does not emit a second delta"
        );
    }

    /// Signal binds a reaction to the conversation it arrived in: a peer who
    /// knows (author, sent_at) of a message elsewhere cannot touch it.
    #[tokio::test]
    async fn reactions_bind_to_the_arrival_conversation() {
        let f = fixture();
        let svc = f.client.messages();

        // Bob, from his own DM, targets Carol's DM message and her channel message.
        assert!(
            !svc.record_inbound_reaction(
                &crate::domain::reaction::ReactionScope::new(
                    ChannelId::from(f.bob),
                    ConversationType::ServerDm
                ),
                &f.bob,
                &f.carol,
                DM_TS,
                "x",
                false,
                1
            )
            .unwrap()
        );
        assert!(
            !svc.record_inbound_reaction(
                &crate::domain::reaction::ReactionScope::new(
                    ChannelId::from(f.bob),
                    ConversationType::ServerDm
                ),
                &f.bob,
                &f.carol,
                CH_TS,
                "x",
                false,
                1
            )
            .unwrap()
        );
        // Bob claims the channel he is not a member of.
        assert!(
            !svc.record_inbound_reaction(
                &crate::domain::reaction::ReactionScope::new(f.channel, ConversationType::Channel),
                &f.bob,
                &f.carol,
                CH_TS,
                "x",
                false,
                1
            )
            .unwrap()
        );
        // Carol, a channel member, aims a channel reaction at her DM message.
        assert!(
            !svc.record_inbound_reaction(
                &crate::domain::reaction::ReactionScope::new(f.channel, ConversationType::Channel),
                &f.carol,
                &f.carol,
                DM_TS,
                "x",
                false,
                1
            )
            .unwrap()
        );
        assert!(reactors(&f, DM_TS).is_empty() && reactors(&f, CH_TS).is_empty());

        // The legitimate ones land.
        assert!(
            svc.record_inbound_reaction(
                &crate::domain::reaction::ReactionScope::new(
                    ChannelId::from(f.carol),
                    ConversationType::ServerDm
                ),
                &f.carol,
                &f.carol,
                DM_TS,
                "x",
                false,
                1
            )
            .unwrap()
        );
        assert!(
            svc.record_inbound_reaction(
                &crate::domain::reaction::ReactionScope::new(f.channel, ConversationType::Channel),
                &f.carol,
                &f.carol,
                CH_TS,
                "x",
                false,
                1
            )
            .unwrap()
        );
        assert_eq!(reactors(&f, DM_TS), vec![f.carol]);
        assert_eq!(reactors(&f, CH_TS), vec![f.carol]);

        // Removal is bound the same way.
        svc.record_reaction(
            &crate::domain::reaction::ReactionScope::new(
                ChannelId::from(f.carol),
                ConversationType::ServerDm,
            ),
            &f.carol,
            DM_TS,
            &f.bob,
            "y",
            false,
        )
        .unwrap();
        assert!(
            !svc.record_inbound_reaction(
                &crate::domain::reaction::ReactionScope::new(
                    ChannelId::from(f.bob),
                    ConversationType::ServerDm
                ),
                &f.bob,
                &f.carol,
                DM_TS,
                "y",
                true,
                1
            )
            .unwrap()
        );
        assert!(reactors(&f, DM_TS).contains(&f.bob));
        assert!(
            svc.record_inbound_reaction(
                &crate::domain::reaction::ReactionScope::new(
                    ChannelId::from(f.carol),
                    ConversationType::ServerDm
                ),
                &f.carol,
                &f.carol,
                DM_TS,
                "x",
                true,
                2
            )
            .unwrap()
        );
    }

    #[tokio::test]
    async fn scoped_inbound_and_linked_device_reactions_never_cross_same_author_clock() {
        let f = fixture();
        let svc = f.client.messages();
        let dm_scope = crate::domain::reaction::ReactionScope::new(
            ChannelId::from(f.carol),
            ConversationType::ServerDm,
        );
        let channel_scope =
            crate::domain::reaction::ReactionScope::new(f.channel, ConversationType::Channel);
        svc.message_repo
            .insert(&Message::new_text(
                f.channel,
                ConversationType::Channel,
                Some(f.carol),
                "same author and clock".into(),
                DM_TS,
            ))
            .unwrap();
        let encoded = crate::domain::reaction::encode_dm_reaction_target(&f.carol, &f.carol);
        let (admitted, author) = svc.dm_reaction_admission(None, f.me, &encoded).unwrap();
        assert_eq!(admitted, dm_scope);
        assert_eq!(author, f.carol);
        assert!(
            svc.dm_reaction_admission(None, f.me, &f.carol.to_string())
                .is_err(),
            "own-device ambiguity is never guessed"
        );
        assert!(
            svc.dm_reaction_admission(Some(f.carol), f.carol, &f.carol.to_string())
                .is_err(),
            "a peer reaction must name its destination"
        );
        assert!(
            svc.dm_reaction_admission(Some(f.bob), f.bob, &encoded)
                .is_err(),
            "remote actor cannot claim a destination other than this recipient"
        );
        let remote = crate::domain::reaction::encode_dm_reaction_target(&f.me, &f.carol);
        assert_eq!(
            svc.dm_reaction_admission(Some(f.carol), f.carol, &remote)
                .unwrap()
                .0,
            dm_scope
        );
        assert!(
            svc.record_inbound_reaction(&admitted, &f.me, &author, DM_TS, "👍", false, 1)
                .unwrap()
        );
        assert!(
            !svc.record_inbound_reaction(&admitted, &f.me, &author, DM_TS, "👍", false, 1)
                .unwrap()
        );
        assert!(
            svc.message_repo
                .get_reactions(&channel_scope, &author, DM_TS)
                .unwrap()
                .is_empty()
        );
        assert!(
            svc.record_inbound_reaction(&channel_scope, &f.carol, &author, DM_TS, "👍", false, 1)
                .unwrap()
        );
        assert!(
            svc.record_inbound_reaction(&dm_scope, &f.me, &author, DM_TS, "👍", true, 2)
                .unwrap()
        );
        assert!(
            !svc.record_inbound_reaction(&dm_scope, &f.me, &author, DM_TS, "👍", true, 2)
                .unwrap()
        );
        assert!(
            svc.message_repo
                .get_by_reaction_scope(&dm_scope, &author, DM_TS)
                .unwrap()
                .unwrap()
                .reaction_counts
                .is_empty()
        );
        assert_eq!(
            svc.message_repo
                .get_by_reaction_scope(&channel_scope, &author, DM_TS)
                .unwrap()
                .unwrap()
                .reaction_counts[0]
                .count,
            1
        );
        assert!(
            svc.record_inbound_reaction(&channel_scope, &f.carol, &author, DM_TS, "👍", true, 2)
                .unwrap()
        );
        assert!(
            svc.message_repo
                .get_by_reaction_scope(&channel_scope, &author, DM_TS)
                .unwrap()
                .unwrap()
                .reaction_counts
                .is_empty()
        );
    }

    /// Pins resolve their target through the same binding.
    #[tokio::test]
    async fn pin_targets_bind_to_the_arrival_conversation() {
        let f = fixture();
        let svc = f.client.messages();
        let resolve = |conv: &ChannelId, actor: &UserId, ts| {
            svc.resolve_inbound_target(conv, actor, &f.carol, ts)
                .unwrap()
        };

        assert!(resolve(&ChannelId::from(f.bob), &f.bob, DM_TS).is_none());
        assert!(resolve(&f.channel, &f.bob, CH_TS).is_none());
        assert!(resolve(&f.channel, &f.carol, DM_TS).is_none());
        assert!(resolve(&ChannelId::from(f.carol), &f.carol, CH_TS).is_none());
        assert!(
            resolve(&ChannelId::new(), &f.carol, 999).is_none(),
            "unknown target"
        );

        assert_eq!(
            resolve(&ChannelId::from(f.carol), &f.carol, DM_TS)
                .unwrap()
                .conversation_id,
            ChannelId::from(f.carol)
        );
        assert_eq!(
            resolve(&f.channel, &f.carol, CH_TS)
                .unwrap()
                .conversation_id,
            f.channel
        );
        // Our own devices act on any conversation we hold (Signal: isMe).
        assert!(resolve(&ChannelId::from(f.me), &f.me, DM_TS).is_some());
    }
