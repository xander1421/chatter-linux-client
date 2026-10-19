mod shield_mode_tests {
    //! Shield (anon) mode is enforced in core: identity-revealing calls refuse
    //! or take the anon path themselves, whatever the UI offers.
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use crate::client::ChatterClient;
    use crate::domain::ids::UserId;
    use crate::domain::message::ConversationType;
    use crate::services::user::UserError;
    use crate::state::ActiveConversation;

    fn build_test_client() -> (tempfile::TempDir, Arc<ChatterClient>) {
        // A real client stores its DB passphrase in the OS keyring; point
        // keyring at the in-memory mock (see `transport_switch_tests`).
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
        (dir, client)
    }

    #[tokio::test]
    async fn shield_mode_turns_the_gif_picker_off() {
        let (_dir, client) = build_test_client();
        assert!(client.gif_picker_available_for(&ConversationType::ServerDm));
        assert!(client.gif_picker_available());
        client.anon_mode.store(true, Ordering::Release);
        // GifService takes account auth: Shield must not reach it, from any
        // conversation, nor search or fetch through it.
        assert!(!client.gif_picker_available_for(&ConversationType::ServerDm));
        assert!(!client.gif_picker_available_for(&ConversationType::Channel));
        assert!(!client.gif_picker_available());
        assert!(client.search_gifs("cat", 1).await.is_err());
        assert!(
            client
                .fetch_gif_media("https://localhost:1/x.gif")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn shield_mode_refuses_identified_calls() {
        let (_dir, client) = build_test_client();
        client.anon_mode.store(true, Ordering::Release);
        let peer = UserId::new();

        assert!(matches!(
            client.send_friend_request(&peer, None).await,
            Err(UserError::ForbiddenInAnonMode)
        ));
        assert!(matches!(
            client
                .connect_via_friend_code("chatter://connect/x", None)
                .await,
            Err(UserError::ForbiddenInAnonMode)
        ));
        assert!(matches!(
            client.lookup_by_username("alice.4711").await,
            Err(UserError::HandleLookupForbiddenInAnonMode)
        ));
        // Not a friend and no anon DM with this peer: shield mode has nothing
        // to open, and never falls back to the identified DM.
        assert_eq!(client.resolve_dm_conversation(&peer.to_string()), None);
        // A friend's DM opens in shield mode too — the same conversation,
        // whose transport is the mailbox there.
        let (me, friend) = (UserId::new(), UserId::new());
        let users = &client.messages().user_repo;
        client
            .messages()
            .settings_repo
            .set_string(
                crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
                &me.to_string(),
            )
            .unwrap();
        users.ensure_user_exists(&me).unwrap();
        users.ensure_user_exists(&friend).unwrap();
        let mut friendship = crate::domain::user::Friendship::new_request(me, friend);
        friendship.accept();
        users.insert_friendship(&friendship).unwrap();
        assert_eq!(
            client.resolve_dm_conversation(&friend.to_string()),
            Some(ActiveConversation::ServerDm { peer_id: friend })
        );
        // The Friends tab makes no identified call and lists no friend.
        assert!(client.friends_tab_entries().await.is_empty());
        // Search and jump-to hide channels; a friend DM shows in both modes.
        assert!(client.visible_in_mode(&ConversationType::ServerDm));
        assert!(!client.visible_in_mode(&ConversationType::SavedMessages));
        assert!(client.visible_in_mode(&ConversationType::AnonDm));
        assert!(
            client
                .quick_switcher_entries("")
                .iter()
                .all(|e| e.conv_type != "saved_messages")
        );

        client.anon_mode.store(false, Ordering::Release);
        assert_eq!(
            client.resolve_dm_conversation(&peer.to_string()),
            Some(ActiveConversation::ServerDm { peer_id: peer })
        );
        // Secret chats (anon DMs) show outside Shield too.
        assert!(client.visible_in_mode(&ConversationType::AnonDm));
    }

    /// With Shield off, a peer known only from an anon group never gets an
    /// identified DM: opening one would link their anonymous membership to
    /// their account. Anyone else still does.
    #[tokio::test]
    async fn anon_group_comember_never_gets_an_identified_dm() {
        use crate::domain::anon_group::{AnonGroup, AnonGroupMember, AnonGroupRole};

        let (_dir, client) = build_test_client();
        let (me, member, stranger) = (UserId::new(), UserId::new(), UserId::new());
        let group = AnonGroup::new_as_owner("g".into(), me);
        let repo = &client.anon_groups().anon_group_repo;
        repo.insert_group(&group).unwrap();
        repo.add_member(&AnonGroupMember::new(
            group.id,
            member,
            AnonGroupRole::Member,
        ))
        .unwrap();

        assert!(!client.anon_mode_enabled());
        // No anon DM with the member either: nothing to open.
        assert_eq!(client.resolve_dm_conversation(&member.to_string()), None);
        assert_eq!(
            client.resolve_dm_conversation(&stranger.to_string()),
            Some(ActiveConversation::ServerDm { peer_id: stranger })
        );
    }

    /// Holding the existing readers forces the pool to open another connection
    /// by path, which requires the fixture's directory to remain alive.
    #[tokio::test]
    async fn fixture_keeps_database_available_to_new_readers() {
        let (dir, client) = build_test_client();
        let db = client.messages().database();
        let readers: Vec<_> = (0..3).map(|_| db.read_connection().unwrap()).collect();
        assert!(dir.path().exists());
        let reader = db
            .read_connection()
            .expect("new reader can open the live DB path");
        assert_eq!(
            reader
                .query_row("SELECT count(*) FROM anon_groups", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            0
        );
        drop(reader);
        drop(readers);
    }

    /// Existing pooled descriptors may survive unlinking; a fresh path-based
    /// reader must fail. Membership SQL-error coverage below checks routing.
    #[tokio::test]
    async fn deleted_fixture_directory_prevents_fresh_reader() {
        let (dir, client) = build_test_client();
        let db = client.messages().database();
        assert!(db.open_reader_connection().is_ok());
        dir.close().expect("remove fixture directory");
        assert!(!db.path().exists());
        assert!(db.open_reader_connection().is_err());
    }

    /// Break actual SQL reads at each repository boundary, rather than mocking
    /// a membership boolean: an unreadable table cannot authorize a new link.
    #[tokio::test]
    async fn anon_membership_read_errors_do_not_open_identified_dms() {
        use crate::domain::anon_group::AnonGroup;
        use crate::domain::message::Message;
        use crate::domain::user::Friendship;
        use crate::storage::repositories::settings_repo::keys;

        for break_groups in [true, false] {
            let (_dir, client) = build_test_client();
            let (me, stranger, known, friend) =
                (UserId::new(), UserId::new(), UserId::new(), UserId::new());
            let group = AnonGroup::new_as_owner("g".into(), me);
            client
                .anon_groups()
                .anon_group_repo
                .insert_group(&group)
                .unwrap();
            client
                .messages()
                .settings_repo
                .set_string(keys::CURRENT_USER_ID, &me.to_string())
                .unwrap();
            for user in [me, known, friend] {
                client
                    .messages()
                    .user_repo
                    .ensure_user_exists(&user)
                    .unwrap();
            }
            let mut friendship = Friendship::new_request(me, friend);
            friendship.accept();
            client
                .messages()
                .user_repo
                .insert_friendship(&friendship)
                .unwrap();
            let message = Message::new_text(
                known.into(),
                ConversationType::ServerDm,
                Some(known),
                "hello".into(),
                1,
            );
            client.messages().message_repo.insert(&message).unwrap();
            let db = client.messages().database();
            db.connection()
                .unwrap()
                .execute_batch(if break_groups {
                    "ALTER TABLE anon_groups RENAME COLUMN owner_id TO unreadable_owner_id"
                } else {
                    "ALTER TABLE anon_group_members RENAME COLUMN user_id TO unreadable_user_id"
                })
                .unwrap();
            if break_groups {
                assert!(client.anon_groups().list_groups().is_err());
            } else {
                assert!(client.anon_groups().list_groups().is_ok());
                assert!(client.anon_groups().get_members(&group.id).is_err());
            }
            assert_eq!(client.resolve_dm_conversation(&stranger.to_string()), None);
            for peer_id in [known, friend] {
                assert_eq!(
                    client.resolve_dm_conversation(&peer_id.to_string()),
                    Some(ActiveConversation::ServerDm { peer_id })
                );
            }
            client.anon_mode.store(true, Ordering::Release);
            assert_eq!(client.resolve_dm_conversation(&known.to_string()), None);
            assert_eq!(
                client.resolve_dm_conversation(&friend.to_string()),
                Some(ActiveConversation::ServerDm { peer_id: friend })
            );
        }
    }
}

#[cfg(test)]
mod local_friends_tests {
    //! Contacts › Friends must render from the friendships table when the
    //! server cannot be reached. The builder points at a closed port.
    use std::sync::Arc;

    use crate::client::ChatterClient;
    use crate::domain::ids::UserId;
    use crate::domain::user::{FriendshipStatus, PresenceStatus, Profile};

    fn build_test_client() -> Arc<ChatterClient> {
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_store(crate::system::keyring::test_credential_store());
        });
        let dir = tempfile::tempdir().unwrap();
        ChatterClient::builder()
            .server_url("https://127.0.0.1:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client")
    }

    #[tokio::test]
    async fn local_friends_list_without_network() {
        let client = build_test_client();
        let (me, friend, pending, blocked) =
            (UserId::new(), UserId::new(), UserId::new(), UserId::new());
        client.auth_state().set_logged_in(me, 1, "Me".into());
        client
            .messages()
            .settings_repo
            .set_string(
                crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
                &me.to_string(),
            )
            .unwrap();
        let users = &client.messages().user_repo;
        for id in [me, friend, pending, blocked] {
            users.ensure_user_exists(&id).unwrap();
        }
        let mut accepted = crate::domain::user::Friendship::new_request(me, friend);
        accepted.accept();
        users.insert_friendship(&accepted).unwrap();
        users
            .insert_friendship(&crate::domain::user::Friendship::new_request(me, pending))
            .unwrap();
        let mut blocked_row = crate::domain::user::Friendship::new_request(me, blocked);
        blocked_row.status = FriendshipStatus::Blocked;
        users.insert_friendship(&blocked_row).unwrap();

        // Stored presence is Online from the last session. Offline render
        // must not repeat that.
        let mut profile = Profile::new(friend, "Ada Lovelace".into());
        profile.avatar_bytes = Some(vec![1, 2, 3, 4]);
        profile.presence = PresenceStatus::Online;
        users.upsert_profile(&profile).unwrap();

        let local = client.local_friend_tab_entries();
        assert_eq!(local.len(), 1, "pending and blocked rows are not friends");
        assert_eq!(local[0].user_id, friend.to_string());
        assert_eq!(local[0].display_name, "Ada Lovelace");
        assert_eq!(local[0].avatar_bytes.as_deref(), Some(&[1, 2, 3, 4][..]));
        assert_eq!(local[0].presence_status, "offline");
        assert!(!local[0].is_anon);

        // ListFriends against a closed port fails. The tab still returns
        // the stored friend instead of an empty vec.
        let via_tab = client.friends_tab_entries().await;
        assert_eq!(via_tab.len(), 1);
        assert_eq!(via_tab[0].user_id, friend.to_string());
        assert_eq!(via_tab[0].display_name, "Ada Lovelace");
        assert_eq!(via_tab[0].avatar_bytes.as_deref(), Some(&[1, 2, 3, 4][..]));
        assert_eq!(via_tab[0].presence_status, "offline");
    }
}

#[cfg(test)]
mod search_hit_tests {
    use crate::client::ChatterClient;
    use crate::domain::channel::{
        Channel, ChannelType, MemberRole, Membership, NotificationPreference,
    };
    use crate::domain::ids::{ChannelId, UserId};
    use crate::domain::message::{ConversationType, Message};

    /// A message hit opens the conversation its row opens: a friend DM by its
    /// peer (the DM row's id), Saved Messages by its sentinel. A DM hit once
    /// left the previous chat on screen and a Saved hit opened nothing.
    #[tokio::test]
    async fn dm_and_saved_hits_open_their_own_conversation() {
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
        let peer = UserId::new();
        let store = |id: ChannelId, kind: ConversationType, text: &str, n: u64| {
            let m = Message::new_text(id, kind, Some(peer), text.to_string(), n);
            client
                .messages()
                .message_repo
                .insert(&m)
                .expect("store message");
        };
        store(peer.into(), ConversationType::ServerDm, "badge two", 1);
        store(
            ChannelId::new(),
            ConversationType::SavedMessages,
            "badge note",
            2,
        );

        let mut targets: Vec<_> = client
            .search_messages_visible("badge", 10)
            .iter()
            .map(|m| client.search_hit_target(m))
            .collect();
        targets.sort();
        assert_eq!(
            targets,
            vec![
                ("dm", peer.to_string()),
                ("saved_messages", "saved-messages".to_string())
            ]
        );
    }

    #[tokio::test]
    async fn global_search_does_not_surface_messages_from_left_channels() {
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
        client.messages().user_repo.ensure_user_exists(&me).unwrap();
        let joined = ChannelId::new();
        let left = ChannelId::new();
        for (id, name) in [(joined, "joined"), (left, "left")] {
            client
                .services
                .channel_repo
                .insert(&Channel::new(id, name.into(), ChannelType::Public, me))
                .unwrap();
            client
                .messages()
                .message_repo
                .insert(&Message::new_text(
                    id,
                    ConversationType::Channel,
                    Some(me),
                    "search privacy".into(),
                    1,
                ))
                .unwrap();
        }
        client
            .services
            .channel_repo
            .insert_membership(&Membership {
                channel_id: joined,
                user_id: me,
                role: MemberRole::Owner,
                joined_at: chrono::Utc::now(),
                last_read_message_id: None,
                is_muted: false,
                muted_until: None,
                notification_preference: NotificationPreference::All,
            })
            .unwrap();
        let hits = client.search_messages_visible("search privacy", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].conversation_id, joined);
    }
}

#[cfg(test)]
mod new_channel_row_tests {
    use crate::client::ChatterClient;
    use crate::domain::channel::{
        Channel, ChannelType, MemberRole, Membership, NotificationPreference,
    };
    use crate::domain::ids::{ChannelId, UserId};

    /// A just-created channel has no messages. It used to sort below every
    /// dated row with no time; it orders and is dated by its creation.
    #[tokio::test]
    async fn a_new_channel_with_no_messages_leads_by_creation_time() {
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
        client.messages().user_repo.ensure_user_exists(&me).unwrap();
        let repo = &client.services.channel_repo;
        let join = |name: &str, last: Option<chrono::DateTime<chrono::Utc>>| {
            let mut c = Channel::new(ChannelId::new(), name.into(), ChannelType::Public, me);
            c.last_message_at = last;
            repo.insert(&c).unwrap();
            repo.insert_membership(&Membership {
                channel_id: c.id,
                user_id: me,
                role: MemberRole::Owner,
                joined_at: chrono::Utc::now(),
                last_read_message_id: None,
                is_muted: false,
                muted_until: None,
                notification_preference: NotificationPreference::All,
            })
            .unwrap();
            c.id.to_string()
        };
        let old = join("old", Some(chrono::Utc::now() - chrono::Duration::hours(1)));
        let new = join("new", None);

        let rows = client.sidebar_rows();
        let ids: Vec<_> = rows.iter().map(|r| r.id.clone()).collect();
        assert_eq!(ids, [new, old], "the new channel leads the list");
        assert!(
            rows[0].timestamp_at.is_some(),
            "the new channel has its creation time"
        );
    }
}

#[cfg(test)]
mod older_messages_tests {
    use crate::client::ChatterClient;
    use crate::domain::ids::{ChannelId, MessageId, UserId};
    use crate::domain::message::{ConversationType, Message};

    /// 120 messages, `sent_at` 1..=120, and their ids in that order.
    fn conversation() -> (std::sync::Arc<ChatterClient>, ChannelId, Vec<MessageId>) {
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
        let (conv, peer) = (ChannelId::new(), UserId::new());
        let ids = (1..=120u64)
            .map(|ts| {
                let msg = Message::new_text(
                    conv,
                    ConversationType::ServerDm,
                    Some(peer),
                    "hi".into(),
                    ts,
                );
                client.messages().store_message_local(&msg).expect("store");
                msg.id
            })
            .collect();
        (client, conv, ids)
    }

    /// Device sweep RC25: after a cold start nothing older than the newest
    /// page ever loaded. Paging from the oldest loaded row walks the whole
    /// history, a page at a time, and ends on an empty page.
    #[tokio::test]
    async fn pages_back_to_the_beginning() {
        let (client, conv, ids) = conversation();
        let kind = ConversationType::ServerDm;
        let page = |before: &MessageId| {
            let rows = client
                .get_older_messages_local_asc(&conv, &kind, before)
                .unwrap();
            rows.iter().map(|m| m.sent_at).collect::<Vec<_>>()
        };
        // The open loaded 71..=120.
        assert_eq!(page(&ids[70]), (21..=70).collect::<Vec<u64>>());
        assert_eq!(page(&ids[20]), (1..=20).collect::<Vec<u64>>());
        assert!(page(&ids[0]).is_empty());
    }

    /// A jump to a message older than what is loaded installs the newest
    /// rows grown back past it (the limit doubles: 50, 100, ...) — and
    /// nothing for a target this list cannot hold.
    #[tokio::test]
    async fn page_reaches_the_jump_target() {
        let (client, conv, ids) = conversation();
        let kind = ConversationType::ServerDm;
        let until = |target: &MessageId| {
            let rows = client
                .get_messages_local_reaching(&conv, &kind, target)
                .unwrap();
            rows.iter().map(|m| m.sent_at).collect::<Vec<_>>()
        };
        // sent_at 30 is past the newest 50 (71..=120); 100 rows reach it.
        assert_eq!(until(&ids[29]), (21..=120).collect::<Vec<u64>>());
        // sent_at 81 is on the newest page already.
        assert_eq!(until(&ids[80]), (71..=120).collect::<Vec<u64>>());

        let elsewhere = Message::new_text(
            ChannelId::new(),
            ConversationType::ServerDm,
            Some(UserId::new()),
            "other conversation".into(),
            5,
        );
        client
            .messages()
            .store_message_local(&elsewhere)
            .expect("store");
        assert!(
            until(&elsewhere.id).is_empty(),
            "a target in another conversation"
        );
        assert!(
            until(&MessageId::new()).is_empty(),
            "a target that is not stored"
        );
    }
}

#[cfg(test)]
mod section_row_tests {
    //! Folder rows are shaped by core (`section_row`), like loose rows.
    use crate::client::ChatterClient;
    use crate::domain::ids::{ChannelId, UserId};
    use crate::domain::message::{ConversationType, Message};
    use crate::domain::section::SectionConversation;

    fn client() -> (std::sync::Arc<ChatterClient>, tempfile::TempDir) {
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
        (client, dir)
    }

    fn conv(id: String, kind: ConversationType, name: &str) -> SectionConversation {
        SectionConversation {
            conversation_id: id,
            conversation_type: kind,
            display_name: name.into(),
            unread_count: 3,
            last_message_at: None,
            is_pinned: true,
        }
    }

    #[tokio::test]
    async fn group_row_offers_leave_and_carries_badge_and_name() {
        let (client, _dir) = client();
        // A group we belong to (not its owner): Leave is offered.
        client
            .messages()
            .settings_repo
            .set(
                crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
                &UserId::new().to_string(),
            )
            .unwrap();
        let id = crate::domain::anon_group::AnonGroupId::new();
        client
            .anon_groups()
            .anon_group_repo
            .insert_group(&crate::domain::anon_group::AnonGroup::from_invite(
                id,
                "Book Club".into(),
                UserId::new(),
            ))
            .unwrap();
        let row = client.section_row(&conv(
            id.to_string(),
            ConversationType::AnonGroup,
            "Book Club",
        ));
        assert_eq!(row.display_name, "Book Club");
        assert!(row.can_leave && !row.can_delete);
        assert!(row.is_pinned);
        assert_eq!(row.unread_count, 3);
        // Groups take no preview in a folder (as before the move).
        assert!(row.last_message.is_empty());
    }

    #[tokio::test]
    async fn unknown_channel_offers_neither_leave_nor_delete() {
        let (client, _dir) = client();
        let row = client.section_row(&conv(
            ChannelId::new().to_string(),
            ConversationType::Channel,
            "general",
        ));
        assert!(!row.can_leave && !row.can_delete);
        assert!(!row.private_channel);
        assert!(row.avatar_bytes.is_none());
    }

    #[tokio::test]
    async fn dm_row_falls_back_to_the_peer_name_and_shows_its_preview() {
        let (client, _dir) = client();
        let peer = UserId::new();
        let id: ChannelId = peer.into();
        let msg = Message::new_text(
            id,
            ConversationType::ServerDm,
            Some(peer),
            "hello there".into(),
            5,
        );
        client.messages().store_message_local(&msg).unwrap();

        let row = client.section_row(&conv(peer.to_string(), ConversationType::ServerDm, ""));
        assert_eq!(row.display_name, client.display_name(&peer));
        assert!(!row.display_name.is_empty());
        assert!(
            row.last_message.contains("hello there"),
            "{}",
            row.last_message
        );
        assert_eq!(row.presence_status, "offline");
        assert!(!row.can_leave);
    }

    /// The chat-list preview follows an edit and a delete-for-everyone of the
    /// conversation's last message, on both ends: each is in storage before
    /// its `MessageEdited` / `MessageDeleted` goes out, so the row rebuild the
    /// UI runs on those events shows it. The received delete used to leave
    /// the row for a separate event subscriber to remove, racing the rebuild.
    #[tokio::test]
    async fn dm_preview_follows_edits_and_a_deleted_last_message() {
        use crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID;
        let (client, _dir) = client();
        let me = UserId::new();
        client
            .messages()
            .settings_repo
            .set(CURRENT_USER_ID, &me.to_string())
            .unwrap();
        client.auth_state().set_logged_in(me, 1, "me".into());
        let peer = UserId::new();
        let id: ChannelId = peer.into();
        let sent = 1_700_000_000_000_000_000u64;
        let store = |from: UserId, text: &str, at: u64| {
            let msg =
                Message::new_text(id, ConversationType::ServerDm, Some(from), text.into(), at);
            client.messages().store_message_local(&msg).unwrap();
            msg
        };
        let preview = || {
            client
                .section_row(&conv(peer.to_string(), ConversationType::ServerDm, ""))
                .last_message
        };

        // Receiver: the peer edits, then deletes, its last message.
        store(peer, "earlier", sent);
        store(peer, "routing check harmless", sent + 1_000);
        assert_eq!(preview(), "routing check harmless");
        let m = client.messages();
        m.apply_dm_edit(
            &peer,
            sent + 1_000,
            "edited from S9",
            sent + 2_000,
            sent + 2_000,
        )
        .unwrap()
        .expect("applied");
        assert_eq!(preview(), "edited from S9");
        m.apply_targeted_dm_delete(&peer, Some(&peer.to_string()), sent + 1_000)
            .unwrap()
            .expect("matched");
        assert_eq!(preview(), "earlier");

        // Sender: our own edit is persisted the way `send_edit` does it.
        let own = store(me, "routing check harmless", sent + 3_000);
        assert_eq!(preview(), "You: routing check harmless");
        let edited = crate::domain::message::MessageContent::Text("edited from S9".into());
        m.update_message_body(&own.id, &edited).unwrap();
        assert_eq!(preview(), "You: edited from S9");
    }

    /// The invite search: a newer keystroke during the pre-lookup pause
    /// retires a complete-handle search before it reaches the server; a
    /// partial handle never pauses or looks up.
    #[tokio::test]
    async fn invite_search_is_superseded_before_any_lookup() {
        let (client, _dir) = client();
        let channel = ChannelId::new();
        let started = std::time::Instant::now();
        assert!(
            client
                .search_channel_invite_candidates(&channel, "alice.4711", || true)
                .await
                .is_none()
        );
        assert!(started.elapsed() >= ChatterClient::INVITE_LOOKUP_PAUSE);

        // Not a complete handle: no pause, so nothing to supersede.
        let cands = client
            .search_channel_invite_candidates(&channel, "alice.", || true)
            .await;
        assert_eq!(cands.map(|c| c.len()), Some(0));
    }
}
