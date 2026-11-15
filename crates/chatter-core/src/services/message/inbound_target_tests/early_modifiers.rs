//! Edits, reactions and pins received before their message, through
//! `MessageService` (storage side: `storage::repositories::message_modifier_repo`).

use super::*;
use crate::domain::message::{Message, MessageContent};
use crate::domain::reaction::ReactionScope;

    fn dm(peer: UserId, body: &str, sent_at: u64) -> Message {
        Message::new_text(
            peer.into(),
            ConversationType::ServerDm,
            Some(peer),
            body.into(),
            sent_at,
        )
    }

    /// A `MessageService` on a fresh store, logged in as `me`.
    fn service(me: UserId) -> (tempfile::TempDir, Arc<crate::client::ChatterClient>) {
        use crate::storage::repositories::settings_repo::keys;
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
        client
            .messages()
            .settings_repo
            .set(keys::CURRENT_USER_ID, &me.to_string())
            .unwrap();
        (dir, client)
    }

    /// The receive side of an edit, through `MessageService`: ordered by the
    /// edit's own time, dated by it, and refused past Signal's 48h.
    #[tokio::test]
    async fn edits_order_by_their_timestamp_and_expire_after_48h() {
        let (_dir, client) = service(UserId::new());
        let m = client.messages();
        let peer = UserId::new();
        let sent = 1_700_000_000_000_000_000u64;
        let window = crate::services::message::MessageService::EDIT_MAX_AGE_NS;

        // Early: held, then applied when the message arrives.
        assert!(m
            .apply_dm_edit(&peer, sent, "early", sent + 10, sent + 10)
            .unwrap()
            .is_none());
        m.message_repo.insert(&dm(peer, "original", sent)).unwrap();
        let text = |m: &crate::services::message::MessageService| {
            m.message_repo
                .get_by_sender_timestamp(&peer, sent)
                .unwrap()
                .unwrap()
                .content
        };
        assert_eq!(text(m), MessageContent::Text("early".into()));

        let newer = m
            .apply_dm_edit(&peer, sent, "newer", sent + 30, sent + 31)
            .unwrap()
            .expect("applied");
        assert_eq!(
            newer.edited_at,
            Some(chrono::DateTime::from_timestamp_nanos((sent + 30) as i64))
        );
        assert!(
            m.apply_dm_edit(&peer, sent, "older", sent + 20, sent + 40)
                .unwrap()
                .is_none(),
            "older than the one shown"
        );
        assert!(
            m.apply_dm_edit(&peer, sent, "same", sent + 30, sent + 40)
                .unwrap()
                .is_none(),
            "already shown"
        );
        // Signal measures the window on the edit's SERVER timestamp: a sender
        // cannot back-date an edit into it.
        assert!(
            m.apply_dm_edit(&peer, sent, "backdated", sent + 40, sent + window + 1)
                .unwrap()
                .is_none(),
            "past 48h"
        );
        // With no server timestamp the edit's own one stands in.
        assert!(
            m.apply_dm_edit(&peer, sent, "late", sent + window + 1, 0)
                .unwrap()
                .is_none(),
            "past 48h"
        );
        assert_eq!(text(m), MessageContent::Text("newer".into()));
        assert!(m
            .apply_dm_edit(&peer, sent, "in time", sent + window, 0)
            .unwrap()
            .is_some());

        // Someone else's DM cannot name it.
        let stranger = UserId::new();
        assert!(m
            .apply_targeted_dm_delete(&stranger, Some(&stranger.to_string()), sent)
            .unwrap()
            .is_none());
        assert!(m
            .apply_targeted_dm_delete(&peer, Some(&peer.to_string()), sent)
            .unwrap()
            .is_some());
        // Deleted before it is announced, not by a later event subscriber.
        assert!(m
            .message_repo
            .get_by_sender_timestamp(&peer, sent)
            .unwrap()
            .is_none());
    }

    /// Signal keeps a reaction until its message arrives
    /// (`Reactions.preload.ts`); so do we, ordered by the reaction's own time
    /// and bound by the same rule as one whose message is here.
    #[tokio::test]
    async fn an_early_reaction_waits_for_its_message() {
        let me = UserId::new();
        let (_dir, client) = service(me);
        let m = client.messages();
        let (peer, stranger) = (UserId::new(), UserId::new());
        let dm_with_peer = ChannelId::from(peer);
        let reaction_scope = ReactionScope::new(dm_with_peer, ConversationType::ServerDm);
        let reactors = |emoji: &str| -> Vec<UserId> {
            m.message_repo
                .get_reactions(&reaction_scope, &peer, 100)
                .unwrap()
                .into_iter()
                .filter(|r| r.emoji.value == emoji)
                .map(|r| r.user_id)
                .collect()
        };

        // The peer's 👍, removed (sent at 20) before the add (sent at 10)
        // reached us: the remove is newer and wins.
        assert!(!m
            .record_inbound_reaction(&reaction_scope, &peer, &peer, 100, "a", true, 20)
            .unwrap());
        assert!(!m
            .record_inbound_reaction(&reaction_scope, &peer, &peer, 100, "a", false, 10)
            .unwrap());
        // Added (30) after a remove (25) that arrived later: the add stands.
        assert!(!m
            .record_inbound_reaction(&reaction_scope, &peer, &peer, 100, "b", false, 30)
            .unwrap());
        assert!(!m
            .record_inbound_reaction(&reaction_scope, &peer, &peer, 100, "b", true, 25)
            .unwrap());
        // Ours, from another of our devices.
        assert!(!m
            .record_inbound_reaction(&reaction_scope, &me, &peer, 100, "c", false, 5)
            .unwrap());
        // A stranger, from their own DM, naming the peer's message.
        assert!(!m
            .record_inbound_reaction(
                &ReactionScope::new(ChannelId::from(stranger), ConversationType::ServerDm),
                &stranger,
                &peer,
                100,
                "d",
                false,
                5
            )
            .unwrap());

        let stored = m
            .message_repo
            .insert_received(&dm(peer, "hi", 100))
            .unwrap()
            .expect("stored");
        assert!(reactors("a").is_empty(), "the later remove wins");
        assert_eq!(reactors("b"), vec![peer]);
        assert_eq!(reactors("c"), vec![me]);
        assert!(reactors("d").is_empty(), "not the stranger's conversation");
        let mut counts: Vec<_> = stored
            .reaction_counts
            .iter()
            .map(|r| (r.emoji.clone(), r.count))
            .collect();
        counts.sort();
        assert_eq!(
            counts,
            vec![("b".to_string(), 1), ("c".to_string(), 1)],
            "the announced row carries them"
        );
        assert_eq!(stored.current_user_reactions, vec!["c".to_string()]);

        // Once the message is here, a reaction applies directly.
        assert!(m
            .record_inbound_reaction(&reaction_scope, &peer, &peer, 100, "e", false, 40)
            .unwrap());
    }

    /// An early pin is applied on arrival — the latest action winning — and a
    /// channel pin only from a member of that channel.
    #[tokio::test]
    async fn an_early_pin_waits_for_its_message() {
        use crate::domain::channel::{
            Channel, ChannelType, MemberRole, Membership, NotificationPreference,
        };
        let me = UserId::new();
        let (_dir, client) = service(me);
        let m = client.messages();
        let (peer, member, outsider, channel) = (
            UserId::new(),
            UserId::new(),
            UserId::new(),
            ChannelId::new(),
        );
        m.channel_repo
            .insert(&Channel::new(channel, "g".into(), ChannelType::Private, me))
            .unwrap();
        for user_id in [peer, member] {
            m.channel_repo
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
        let in_channel = |sent_at| {
            Message::new_text(
                channel,
                ConversationType::Channel,
                Some(peer),
                "c".into(),
                sent_at,
            )
        };

        // DM: pinned at 10, unpinned at 20 but delivered first.
        let dm_with_peer = ChannelId::from(peer);
        assert!(m
            .resolve_inbound_pin(&dm_with_peer, &peer, &peer, 100, true, 20)
            .unwrap()
            .is_none());
        assert!(m
            .resolve_inbound_pin(&dm_with_peer, &peer, &peer, 100, false, 10)
            .unwrap()
            .is_none());
        assert!(
            !m.message_repo
                .insert_received(&dm(peer, "x", 100))
                .unwrap()
                .unwrap()
                .is_pinned
        );
        assert!(m
            .resolve_inbound_pin(&dm_with_peer, &peer, &peer, 101, false, 10)
            .unwrap()
            .is_none());
        assert!(
            m.message_repo
                .insert_received(&dm(peer, "y", 101))
                .unwrap()
                .unwrap()
                .is_pinned
        );

        // Channel: an outsider's pin is refused when the message arrives; a member's lands.
        assert!(m
            .resolve_inbound_pin(&channel, &outsider, &peer, 200, false, 10)
            .unwrap()
            .is_none());
        assert!(
            !m.message_repo
                .insert_received(&in_channel(200))
                .unwrap()
                .unwrap()
                .is_pinned
        );
        assert!(m
            .resolve_inbound_pin(&channel, &member, &peer, 201, false, 10)
            .unwrap()
            .is_none());
        assert!(
            m.message_repo
                .insert_received(&in_channel(201))
                .unwrap()
                .unwrap()
                .is_pinned
        );
        // A pin naming another conversation than the message's does not apply.
        assert!(m
            .resolve_inbound_pin(&dm_with_peer, &peer, &peer, 202, false, 10)
            .unwrap()
            .is_none());
        assert!(
            !m.message_repo
                .insert_received(&in_channel(202))
                .unwrap()
                .unwrap()
                .is_pinned
        );
    }
