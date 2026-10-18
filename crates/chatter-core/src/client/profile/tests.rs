    use super::*;
    use crate::domain::anon_dm::AnonDm;
    use crate::domain::ids::{ChannelId, UserId};
    use crate::domain::message::Message;

    fn test_client() -> (Arc<ChatterClient>, tempfile::TempDir) {
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

    fn avatar_owner_fixture(client: &ChatterClient) -> (UserId, u64) {
        use crate::storage::repositories::settings_repo::keys;
        let user = UserId::new();
        client
            .messages()
            .settings_repo
            .set_string(keys::CURRENT_USER_ID, &user.to_string())
            .unwrap();
        client
            .auth_state()
            .set_logged_in(user, 1, "Avatar fixture".into());
        (user, client.auth_state().get().session_revision)
    }

    #[tokio::test]
    async fn avatar_profile_save_refuses_stale_owner_cancelled_queue_and_wire_overflow_before_write()
     {
        use crate::storage::repositories::settings_repo::keys;
        use std::sync::atomic::{AtomicBool, Ordering};
        let (client, _dir) = test_client();
        let (user, revision) = avatar_owner_fixture(&client);
        client
            .messages()
            .settings_repo
            .set_bytes(keys::AVATAR, b"old fixture")
            .unwrap();
        let alive = Arc::new(AtomicBool::new(true));
        let save = |owner, revision, alive, bytes| {
            client.save_own_profile_in_session(
                owner,
                revision,
                alive,
                Some(bytes),
                "Fixture name".into(),
                None,
            )
        };
        assert!(
            save(UserId::new(), revision, alive.clone(), vec![1])
                .await
                .is_err()
        );
        assert!(
            save(user, revision + 1, alive.clone(), vec![1])
                .await
                .is_err()
        );
        assert!(
            save(
                user,
                revision,
                alive.clone(),
                vec![0; crate::domain::anon_dm_init::MAX_AVATAR_SIZE - 27]
            )
            .await
            .is_err()
        );
        let blocker = client.auth_lifecycle_lock.lock().await;
        let queued_client = client.clone();
        let queued_alive = alive.clone();
        let queued = tokio::spawn(async move {
            queued_client
                .save_own_profile_in_session(
                    user,
                    revision,
                    queued_alive,
                    Some(vec![1]),
                    "Queued fixture".into(),
                    None,
                )
                .await
        });
        tokio::task::yield_now().await;
        alive.store(false, Ordering::SeqCst);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), queued)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        drop(blocker);
        assert_eq!(
            client
                .messages()
                .settings_repo
                .get_bytes(keys::AVATAR)
                .unwrap()
                .unwrap(),
            b"old fixture"
        );
    }

    #[tokio::test]
    async fn avatar_profile_session_lock_and_bounded_cancellation_fence_propagation_operations() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (client, _dir) = test_client();
        let (user, revision) = avatar_owner_fixture(&client);
        let alive = Arc::new(AtomicBool::new(true));
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let worker_client = client.clone();
        let worker_alive = alive.clone();
        let operation = tokio::spawn(async move {
            worker_client
                .with_profile_owner(user, revision, &worker_alive, || async {
                    entered_tx.send(()).unwrap();
                    worker_client
                        .bounded_profile_operation::<()>(
                            user,
                            revision,
                            &worker_alive,
                            std::time::Duration::from_secs(10),
                            std::future::pending(),
                        )
                        .await
                })
                .await
        });
        entered_rx.await.unwrap();
        assert!(
            client.auth_lifecycle_lock.try_lock().is_err(),
            "owner remains locked during RPC"
        );
        alive.store(false, Ordering::SeqCst);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), operation)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert!(
            client.auth_lifecycle_lock.try_lock().is_ok(),
            "cancel releases auth serialization"
        );
        let live = AtomicBool::new(true);
        let expired = client
            .with_profile_owner(user, revision, &live, || async {
                client
                    .bounded_profile_operation::<()>(
                        user,
                        revision,
                        &live,
                        std::time::Duration::from_millis(10),
                        std::future::pending(),
                    )
                    .await
            })
            .await;
        assert!(expired.unwrap_err().to_string().contains("timed out"));
        client
            .auth_state()
            .set_logged_in(user, 1, "Same account new session".into());
        let ran = AtomicBool::new(false);
        let refused = client
            .with_profile_owner(user, revision, &live, || async {
                ran.store(true, Ordering::SeqCst);
                Ok(())
            })
            .await;
        assert!(refused.is_err());
        assert!(
            !ran.load(Ordering::SeqCst),
            "the production propagation guard must not execute under replacement credentials"
        );
    }

    #[tokio::test]
    async fn avatar_profile_propagation_does_not_mix_an_old_edit_with_new_avatar() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (client, _dir) = test_client();
        let (user, revision) = avatar_owner_fixture(&client);
        let repositories = client.messages();
        let repository = &repositories.user_repo;
        repository.ensure_user_exists(&user).unwrap();
        let mut old = crate::domain::user::Profile::new(user, "Old fixture".into());
        old.avatar_bytes = Some(b"old generated fixture".to_vec());
        repository.upsert_profile(&old).unwrap();
        let live = AtomicBool::new(true);
        let ran = AtomicBool::new(false);
        client
            .with_profile_snapshot(user, revision, &live, &old, || async {
                ran.store(true, Ordering::SeqCst);
                Ok(())
            })
            .await
            .unwrap();
        assert!(ran.swap(false, Ordering::SeqCst));
        let mut newer = old.clone();
        newer.avatar_bytes = Some(b"new generated fixture".to_vec());
        repository.upsert_profile(&newer).unwrap();
        assert!(
            client
                .with_profile_snapshot(user, revision, &live, &old, || async {
                    ran.store(true, Ordering::SeqCst);
                    Ok(())
                })
                .await
                .is_err()
        );
        assert!(!ran.load(Ordering::SeqCst));
        client
            .auth_state()
            .set_logged_in(user, 1, "Replacement fixture session".into());
        assert!(
            client
                .with_profile_snapshot(user, revision, &live, &newer, || async {
                    ran.store(true, Ordering::SeqCst);
                    Ok(())
                })
                .await
                .is_err()
        );
        assert!(!ran.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn avatar_profile_legacy_response_cannot_adopt_replacement_account_or_session() {
        let (client, _dir) = test_client();
        let (user, revision) = avatar_owner_fixture(&client);
        let profile = crate::domain::user::Profile::new(user, "Old owner result".into());
        assert!(
            client
                .profile_response_in_session(profile.clone(), user, revision, false)
                .is_ok()
        );
        assert!(
            client
                .profile_response_in_session(profile.clone(), user, revision, true)
                .is_err()
        );
        client
            .auth_state()
            .set_logged_in(UserId::new(), 1, "New fixture account".into());
        assert!(
            client
                .profile_response_in_session(profile.clone(), user, revision, false)
                .is_err()
        );
        client
            .auth_state()
            .set_logged_in(user, 1, "Same fixture account re-login".into());
        assert!(
            client
                .profile_response_in_session(profile, user, revision, false)
                .is_err()
        );
    }

    #[tokio::test]
    async fn avatar_profile_rpc_failure_restores_previous_local_selection_and_does_not_publish_success()
     {
        use crate::storage::repositories::settings_repo::keys;
        use std::sync::atomic::AtomicBool;
        let (client, _dir) = test_client();
        let (user, revision) = avatar_owner_fixture(&client);
        let repositories = client.messages();
        repositories.user_repo.ensure_user_exists(&user).unwrap();
        repositories
            .user_repo
            .upsert_profile(&crate::domain::user::Profile::new(user, "Fixture".into()))
            .unwrap();
        repositories
            .settings_repo
            .set_bytes(keys::PROFILE_KEY, &[7; 32])
            .unwrap();
        repositories
            .settings_repo
            .set_bytes(keys::AVATAR, b"previous owned bytes")
            .unwrap();
        let image = image::DynamicImage::new_rgba8(2, 2);
        let mut encoded = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let normalized =
            crate::media::ImageProcessor::process_avatar_from_memory(&encoded.into_inner())
                .unwrap();
        let result = client
            .save_own_profile_in_session(
                user,
                revision,
                Arc::new(AtomicBool::new(true)),
                Some(normalized),
                "New fixture name".into(),
                None,
            )
            .await;
        assert!(result.is_err(), "offline fixture RPC must fail");
        assert_eq!(
            repositories
                .settings_repo
                .get_bytes(keys::AVATAR)
                .unwrap()
                .unwrap(),
            b"previous owned bytes"
        );
        assert_eq!(
            client.auth_state().get().display_name.as_deref(),
            Some("Avatar fixture")
        );
        assert_eq!(
            repositories
                .user_repo
                .get_profile(&user)
                .unwrap()
                .unwrap()
                .display_name,
            "Fixture"
        );
    }

    #[tokio::test]
    async fn typing_aggregate_survives_one_sender_stopping_and_expires() {
        let (client, _dir) = test_client();
        let channel = ChannelId::new();
        let alice = UserId::from_uuid(uuid::Uuid::from_u128(1));
        let bob = UserId::from_uuid(uuid::Uuid::from_u128(2));
        assert!(client.update_typing_indicator(channel, alice, 100, true));
        assert!(client.update_typing_indicator(channel, bob, 100, true));
        assert_eq!(
            client.typing_indicator_text(channel),
            format!(
                "{} and {} are typing…",
                client.display_name(&alice),
                client.display_name(&bob)
            )
        );
        assert!(client.update_typing_indicator(channel, alice, 200, false));
        assert_eq!(
            client.typing_indicator_text(channel),
            format!("{} is typing…", client.display_name(&bob))
        );
        assert!(!client.update_typing_indicator(channel, alice, 150, true));
        assert!(client.update_typing_indicator(channel, alice, 300, true));
        assert!(client.update_typing_indicator(channel, UserId::new(), 300, true));
        assert_eq!(
            client.typing_indicator_text(channel),
            "Several people are typing…"
        );
        client.chat_state().clear_typing(&channel);
        assert_eq!(client.typing_indicator_text(channel), "");
    }

    /// Nothing to change (no name, no status, no avatar) sends nothing: the
    /// server reads a maskless update as "every field". The test server is
    /// unreachable, so any RPC would fail this.
    #[tokio::test]
    async fn profile_update_with_nothing_to_change_sends_no_unmasked_update() {
        use crate::storage::repositories::settings_repo::keys;
        let (client, _dir) = test_client();
        let me = UserId::new();
        client
            .messages()
            .settings_repo
            .set(keys::CURRENT_USER_ID, &me.to_string())
            .unwrap();
        client.messages().user_repo.ensure_user_exists(&me).unwrap();
        client
            .messages()
            .user_repo
            .upsert_profile(&crate::domain::user::Profile::new(me, "me".into()))
            .unwrap();
        let profile = client
            .users()
            .update_user_profile(None, None)
            .await
            .expect("no RPC");
        assert_eq!(profile.user_id, me);
    }

    /// A blocked contact's presence is hidden: their stored "online" shows as
    /// offline (dot, profile line, live events), and returns on unblock.
    #[tokio::test]
    async fn blocked_peer_presence_is_hidden() {
        use crate::domain::user::{Friendship, FriendshipStatus, PresenceStatus, Profile};
        use crate::storage::repositories::settings_repo::keys;
        let (client, _dir) = test_client();
        let (me, peer, friend) = (UserId::new(), UserId::new(), UserId::new());
        let repos = client.messages();
        repos
            .settings_repo
            .set_string(keys::CURRENT_USER_ID, &me.to_string())
            .unwrap();
        for u in [me, peer, friend] {
            repos.user_repo.ensure_user_exists(&u).unwrap();
            let mut p = Profile::new(u, "x".into());
            p.presence = PresenceStatus::Online;
            repos.user_repo.upsert_profile(&p).unwrap();
        }
        assert_eq!(client.peer_presence(&peer), PresenceStatus::Online);

        repos
            .user_repo
            .insert_friendship(&Friendship {
                requester_id: me,
                addressee_id: peer,
                status: FriendshipStatus::Blocked,
                created_at: chrono::Utc::now(),
            })
            .unwrap();
        assert_eq!(client.peer_presence(&peer), PresenceStatus::Offline);
        assert_eq!(
            client.peer_display_presence(&peer, PresenceStatus::Away),
            PresenceStatus::Offline
        );
        assert!(client.peer_last_seen(&peer).is_none());
        assert_eq!(client.peer_presence(&friend), PresenceStatus::Online);
        assert_eq!(
            client.peer_last_seen(&friend).unwrap().0,
            PresenceStatus::Online
        );

        repos.user_repo.delete_friendship(&me, &peer).unwrap();
        assert_eq!(client.peer_presence(&peer), PresenceStatus::Online);
    }

    /// Friends › Blocked › Delete chat on an anon peer: the conversation and
    /// its history go, the block stays, and the peer's friend requests stay
    /// hidden. Unblock afterwards lifts the peer block (no chat to go through).
    #[tokio::test]
    async fn delete_blocked_anon_chat_keeps_the_block() {
        let (client, _dir) = test_client();
        let peer = UserId::from_uuid(uuid::Uuid::from_u128(7));
        let dm = AnonDm::new_initiated(peer);
        client
            .anon_dms()
            .anon_dm_repo()
            .insert_conversation(&dm)
            .unwrap();
        let conv: ChannelId = dm.id.into();
        let msg = Message::new_text(conv, ConversationType::AnonDm, Some(peer), "hi".into(), 1);
        client.messages().message_repo.insert(&msg).unwrap();
        client.block_profile_user(&peer).await.unwrap();

        let row = |c: &ChatterClient| {
            c.blocked_users()
                .into_iter()
                .find(|b| b.id == peer)
                .unwrap()
        };
        assert!(row(&client).anon && row(&client).anon_chat);

        client.remove_blocked_contact(&peer).await.unwrap();
        assert!(client.find_anon_dm_by_peer(&peer).is_none());
        assert!(
            client
                .messages()
                .message_repo
                .get(&msg.id)
                .unwrap()
                .is_none()
        );
        assert!(client.is_profile_user_blocked(&peer));
        assert!(row(&client).anon && !row(&client).anon_chat);

        let req = |id: &UserId| crate::inbox::events::FriendRequestInfo {
            user_id: id.to_string(),
            display_name: String::new(),
            username: String::new(),
            encrypted_intro: Vec::new(),
            created_at_ns: 0,
        };
        let other = UserId::from_uuid(uuid::Uuid::from_u128(8));
        let shown = client.visible_friend_requests(vec![req(&peer), req(&other)]);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].user_id, other.to_string());

        client.unblock_profile_user(&peer).await.unwrap();
        assert!(!client.is_profile_user_blocked(&peer));
    }

    /// Friends › Blocked › Delete chat on an accepted secret chat ends it for
    /// both sides like the chat's own "Delete chat": the peer is told first,
    /// failing closed, so with the peer unreachable nothing is deleted.
    #[tokio::test]
    async fn delete_blocked_accepted_secret_chat_ends_it_for_both() {
        use crate::domain::anon_dm::AnonDmStatus;
        let (client, _dir) = test_client();
        let peer = UserId::from_uuid(uuid::Uuid::from_u128(9));
        let mut dm = AnonDm::new_initiated(peer);
        dm.status = AnonDmStatus::Active;
        dm.peer_mailbox_pk = Some([2; 32]);
        client
            .anon_dms()
            .anon_dm_repo()
            .insert_conversation(&dm)
            .unwrap();
        client.block_profile_user(&peer).await.unwrap();

        assert!(client.remove_blocked_contact(&peer).await.is_err());
        assert_eq!(client.find_anon_dm_by_peer(&peer), Some(dm.id));
        assert!(client.is_profile_user_blocked(&peer));
    }

    /// A secret DM's profile shows what the chat carried, not the global
    /// profile cached for the same user.
    #[tokio::test]
    async fn anon_dm_peer_card_uses_in_chat_profile_only() {
        let (client, _dir) = test_client();
        let peer = UserId::from_uuid(uuid::Uuid::from_u128(11));
        let mut global = crate::domain::user::Profile::new(peer, "Global Name".into());
        global.avatar_bytes = Some(vec![9]);
        client
            .messages()
            .user_repo
            .ensure_user_exists(&peer)
            .unwrap();
        client.messages().user_repo.upsert_profile(&global).unwrap();

        let mut dm = AnonDm::new_initiated(peer);
        client
            .anon_dms()
            .anon_dm_repo()
            .insert_conversation(&dm)
            .unwrap();
        let (name, avatar) = client.anon_dm_peer_card(&dm.id).unwrap();
        assert_ne!(name, "Global Name");
        assert_eq!(avatar, None);

        dm.peer_display_name = Some("In-chat".into());
        dm.peer_avatar = Some(vec![1]);
        client
            .anon_dms()
            .anon_dm_repo()
            .update_conversation(&dm)
            .unwrap();
        assert_eq!(
            client.anon_dm_peer_card(&dm.id),
            Some(("In-chat".into(), Some(vec![1])))
        );
    }

    /// An anon chat blocked without its peer row (half a block) is still
    /// listed in Friends › Blocked, and Unblock there lifts it.
    #[tokio::test]
    async fn chat_only_anon_block_is_listed_and_unblockable() {
        let (client, _dir) = test_client();
        let peer = UserId::from_uuid(uuid::Uuid::from_u128(9));
        let dm = AnonDm::new_initiated(peer);
        client
            .anon_dms()
            .anon_dm_repo()
            .insert_conversation(&dm)
            .unwrap();
        client.block_anon_dm_conversation(&dm.id).unwrap();

        let row = client
            .blocked_users()
            .into_iter()
            .find(|b| b.id == peer)
            .unwrap();
        assert!(row.anon && row.anon_chat);

        client.unblock_profile_user(&peer).await.unwrap();
        assert_eq!(client.is_anon_dm_conversation_blocked(&dm.id), Some(false));
        assert!(client.blocked_users().iter().all(|b| b.id != peer));
    }
}
