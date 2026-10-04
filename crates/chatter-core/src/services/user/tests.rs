//! Tests for user service.

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::domain::ids::UserId;
    use crate::domain::user::{FriendshipStatus, PresenceStatus};
    use crate::proto::chat_v1::{friend_requests_filter, UserProfile};
    use crate::storage::repositories::settings_repo::SettingsRepository;
    use crate::storage::repositories::user_repo::UserRepository;

    use super::super::conversions::{
        domain_presence_to_proto, presence_from_proto,
        proto_friend_status_to_domain, proto_to_domain_profile,
    };
    use super::super::{friend_display_name_placeholder, FriendRequestFilter, UserError, UserService};

    /// F30: a peer whose only live device is 6 (device 1 retired) must still
    /// yield the account identity key — the friend request asks for all
    /// devices and never names one.
    #[test]
    fn friend_request_identity_key_from_any_device() {
        use crate::proto::chat_v1::{
            get_pre_key_bundle_response::Result as BundleResult, GetPreKeyBundleResponse,
            PreKeyBundleDeviceList, SignalPreKeyBundle,
        };
        use super::super::friend_request_identity_key;

        let ik = vec![5u8; 33];
        let only_device_6 = GetPreKeyBundleResponse {
            result: Some(BundleResult::Devices(PreKeyBundleDeviceList {
                bundles: vec![SignalPreKeyBundle {
                    device_id: 6,
                    identity_key: ik.clone(),
                    ..Default::default()
                }],
                identity_key: ik.clone(),
            })),
        };
        assert_eq!(friend_request_identity_key(only_device_6).unwrap(), ik);

        let none = GetPreKeyBundleResponse { result: None };
        let err = friend_request_identity_key(none).unwrap_err();
        assert_eq!(err.user_message(), "This user has no registered devices");
    }

    #[test]
    fn user_message_hides_the_error_chain() {
        use crate::error::NetworkError;
        let e = UserError::Network(NetworkError::ServerError("failed to get prekey bundle".into()));
        assert_eq!(e.user_message(), "Couldn't reach the server. Try again.");
        assert!(!UserError::Network(NetworkError::PermissionDenied).user_message().contains("error"));
    }

    #[test]
    fn lookup_handle_shape() {
        use super::super::is_lookup_handle;
        assert!(is_lookup_handle("alice.4711"));
        assert!(is_lookup_handle(" alice.47 "));
        assert!(!is_lookup_handle("alice"));
        assert!(!is_lookup_handle("alice."));
        assert!(!is_lookup_handle(".4711"));
        assert!(!is_lookup_handle("alice.47x"));
        assert!(!is_lookup_handle(""));
    }

    /// Mirrors Signal-Desktop's 5-minute `throttledGetProfiles`: never fetched
    /// or fetched a full interval ago → refetch; anything newer → skip.
    #[test]
    fn profile_fetch_due_after_interval_only() {
        use super::super::{profile_fetch_due, PROFILE_REFRESH_INTERVAL};
        use std::time::{Duration, Instant};

        let now = Instant::now() + PROFILE_REFRESH_INTERVAL * 2;
        assert!(profile_fetch_due(None, now));
        assert!(!profile_fetch_due(Some(now), now));
        assert!(!profile_fetch_due(Some(now - PROFILE_REFRESH_INTERVAL + Duration::from_secs(1)), now));
        assert!(profile_fetch_due(Some(now - PROFILE_REFRESH_INTERVAL), now));
        // A clock that reads earlier than the stamp is not "stale".
        assert!(!profile_fetch_due(Some(now + Duration::from_secs(1)), now));
    }

    #[test]
    fn test_user_error_display() {
        let err = UserError::NotLoggedIn;
        assert!(err.to_string().contains("not logged in"));

        let err = UserError::InvalidInput("empty name".to_string());
        assert!(err.to_string().contains("empty name"));

        let user_id = UserId::new();
        let err = UserError::UserNotFound(user_id);
        assert!(err.to_string().contains("user not found"));

        let err = UserError::NotAuthorized;
        assert!(err.to_string().contains("not authorized"));
    }

    #[test]
    fn test_presence_from_proto() {
        assert_eq!(presence_from_proto(0), PresenceStatus::Offline);
        assert_eq!(presence_from_proto(1), PresenceStatus::Offline);
        assert_eq!(presence_from_proto(2), PresenceStatus::Online);
        assert_eq!(presence_from_proto(3), PresenceStatus::Away);
        assert_eq!(presence_from_proto(4), PresenceStatus::DoNotDisturb);
        assert_eq!(presence_from_proto(99), PresenceStatus::Offline);
    }

    #[test]
    fn test_domain_presence_to_proto() {
        assert_eq!(domain_presence_to_proto(PresenceStatus::Offline), 1);
        assert_eq!(domain_presence_to_proto(PresenceStatus::Online), 2);
        assert_eq!(domain_presence_to_proto(PresenceStatus::Away), 3);
        assert_eq!(domain_presence_to_proto(PresenceStatus::DoNotDisturb), 4);
    }

    #[test]
    fn test_presence_roundtrip() {
        for status in [
            PresenceStatus::Offline,
            PresenceStatus::Online,
            PresenceStatus::Away,
            PresenceStatus::DoNotDisturb,
        ] {
            let proto = domain_presence_to_proto(status);
            let back = presence_from_proto(proto);
            assert_eq!(back, status);
        }
    }

    #[test]
    fn test_proto_to_domain_profile() {
        let proto = UserProfile {
            user_id: "00000000-0000-0000-0000-000000000001".to_string(),
            display_name: "Test User".to_string(),
            status_message: "Hello world".to_string(),
            presence: 2,
            encrypted_name: Vec::new(),
            encrypted_about: Vec::new(),
            encrypted_avatar: Vec::new(),
        };

        let profile = proto_to_domain_profile(&proto).unwrap();
        assert_eq!(profile.display_name, "Test User");
        assert_eq!(
            profile.status_message.as_ref().map(|s| s.text.as_str()),
            Some("Hello world")
        );
        assert_eq!(profile.presence, PresenceStatus::Online);
    }

    #[test]
    fn test_proto_to_domain_profile_empty_strings() {
        let proto = UserProfile {
            user_id: "00000000-0000-0000-0000-000000000001".to_string(),
            display_name: "Test User".to_string(),
            status_message: "".to_string(),
            presence: 1,
            encrypted_name: Vec::new(),
            encrypted_about: Vec::new(),
            encrypted_avatar: Vec::new(),
        };

        let profile = proto_to_domain_profile(&proto).unwrap();
        assert_eq!(profile.status_message, None);
        assert_eq!(profile.presence, PresenceStatus::Offline);
    }

    #[test]
    fn test_friend_request_filter_values() {
        assert_eq!(
            FriendRequestFilter::All.to_proto(),
            friend_requests_filter::Filter::All
        );
        assert_eq!(
            FriendRequestFilter::Pending.to_proto(),
            friend_requests_filter::Filter::Pending
        );
        assert_eq!(
            FriendRequestFilter::Accepted.to_proto(),
            friend_requests_filter::Filter::Accepted
        );
        assert_eq!(
            FriendRequestFilter::Sent.to_proto(),
            friend_requests_filter::Filter::Sent
        );
        assert_eq!(
            FriendRequestFilter::Received.to_proto(),
            friend_requests_filter::Filter::Received
        );
    }

    #[test]
    fn test_proto_friend_status_conversion() {
        assert_eq!(
            proto_friend_status_to_domain(3),
            FriendshipStatus::Accepted
        );
        assert_eq!(
            proto_friend_status_to_domain(5),
            FriendshipStatus::Blocked
        );
        assert_eq!(
            proto_friend_status_to_domain(2),
            FriendshipStatus::Pending
        );
        assert_eq!(
            proto_friend_status_to_domain(0),
            FriendshipStatus::Pending
        );
        assert_eq!(
            proto_friend_status_to_domain(1),
            FriendshipStatus::Pending
        );
        assert_eq!(
            proto_friend_status_to_domain(4),
            FriendshipStatus::Pending
        );
        assert_eq!(
            proto_friend_status_to_domain(99),
            FriendshipStatus::Pending
        );
    }

    /// F40: a block keeps the friendship but hides the contact everywhere a
    /// friend is listed (`listed_friends`), and a DM with them drops out of
    /// folders and search (`is_blocked_conversation`). Server blocks and
    /// anon-DM blocks both count; either row direction resolves the peer.
    #[tokio::test]
    async fn blocked_friends_are_not_listed() {
        use crate::client::ChatterClient;
        use crate::domain::message::ConversationType;
        use crate::domain::user::Friendship;
        use crate::storage::repositories::settings_repo::keys;

        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");

        let (me, kept, blocked, anon_blocked) = (UserId::new(), UserId::new(), UserId::new(), UserId::new());
        let users = client.users();
        users.settings_repo.set_string(keys::CURRENT_USER_ID, &me.to_string()).unwrap();
        client.auth_state().set_logged_in(me, 1, "me".into());
        let row = |from, to, status| Friendship { requester_id: from, addressee_id: to, status, created_at: chrono::Utc::now() };
        for u in [me, kept, blocked, anon_blocked] {
            users.user_repo.ensure_user_exists(&u).unwrap();
        }
        users.user_repo.insert_friendship(&row(me, kept, FriendshipStatus::Accepted)).unwrap();
        // Friend who sent the request, then was blocked by us: the Accepted row
        // is theirs, the Blocked row ours.
        users.user_repo.insert_friendship(&row(blocked, me, FriendshipStatus::Accepted)).unwrap();
        users.user_repo.insert_friendship(&row(me, blocked, FriendshipStatus::Blocked)).unwrap();
        users.user_repo.insert_friendship(&row(anon_blocked, me, FriendshipStatus::Accepted)).unwrap();
        users.anon_dm_repo.block_user(&anon_blocked, None).unwrap();

        assert_eq!(client.listed_friends(), vec![kept]);
        assert!(client.is_blocked_conversation(&ConversationType::ServerDm, &blocked.to_string()));
        assert!(!client.is_blocked_conversation(&ConversationType::ServerDm, &kept.to_string()));
        assert!(!client.is_blocked_conversation(&ConversationType::Channel, &blocked.to_string()));
    }

    /// F82: removing a contact never deletes the conversation (Signal). A
    /// peer we wrote to keeps a row after the friendship goes; a blocked one
    /// still drops out, and a stranger's unanswered message is not a chat.
    #[tokio::test]
    async fn dm_history_outlives_the_friendship() {
        use crate::client::ChatterClient;
        use crate::domain::ids::ChannelId;
        use crate::domain::message::{ConversationType, Message};
        use crate::domain::user::Friendship;
        use crate::storage::repositories::settings_repo::keys;

        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");

        let (me, friend, former, blocked, stranger) =
            (UserId::new(), UserId::new(), UserId::new(), UserId::new(), UserId::new());
        let users = client.users();
        users.settings_repo.set_string(keys::CURRENT_USER_ID, &me.to_string()).unwrap();
        client.auth_state().set_logged_in(me, 1, "me".into());
        for u in [me, friend, former, blocked, stranger] {
            users.user_repo.ensure_user_exists(&u).unwrap();
        }
        users.user_repo.insert_friendship(&Friendship {
            requester_id: me, addressee_id: friend,
            status: FriendshipStatus::Accepted, created_at: chrono::Utc::now(),
        }).unwrap();
        users.user_repo.insert_friendship(&Friendship {
            requester_id: me, addressee_id: blocked,
            status: FriendshipStatus::Blocked, created_at: chrono::Utc::now(),
        }).unwrap();
        for (peer, sender) in [(former, me), (blocked, me), (stranger, stranger)] {
            let m = Message::new_text(ChannelId::from(peer), ConversationType::ServerDm, Some(sender), "hi".into(), 1);
            client.messages().message_repo.insert(&m).unwrap();
        }

        assert_eq!(client.dm_peers(), vec![friend, former]);
    }

    /// REVIEW-R4 P0-3: a profile row stored without the profile key holds
    /// the `User <id8>` placeholder; Contacts and request rows must fall
    /// through to a known username instead of showing it.
    #[tokio::test]
    async fn placeholder_profile_does_not_hide_a_username() {
        use crate::client::ChatterClient;
        use crate::domain::user::{Profile, User};

        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");

        let (bob, stranger) = (UserId::new(), UserId::new());
        let users = client.users();
        users.user_repo.insert(&User::new(bob, "bob".into(), Vec::new())).unwrap();
        users.user_repo.upsert_profile(&Profile::new(bob, friend_display_name_placeholder(&bob))).unwrap();

        assert_eq!(client.display_name(&bob), "bob");
        assert_eq!(client.friend_request_peer_name(&stranger, "carol"), "carol");
        assert_eq!(client.friend_request_peer_name(&stranger, ""), friend_display_name_placeholder(&stranger));
    }

    #[test]
    fn test_user_service_construction() {
        fn _verify_types(
            grpc_pool: Arc<crate::network::grpc::client_pool::GrpcClientPool>,
            settings_repo: Arc<SettingsRepository>,
            user_repo: Arc<UserRepository>,
            anon_dm_repo: Arc<crate::storage::repositories::anon_dm_repo::AnonDmRepository>,
            anon_group_repo: Arc<crate::storage::repositories::anon_group_repo::AnonGroupRepository>,
        ) {
            let _service = UserService::new(grpc_pool, settings_repo, user_repo, anon_dm_repo, anon_group_repo);
        }
    }

    #[test]
    fn test_blocked_status_mapping() {
        assert_eq!(
            proto_friend_status_to_domain(5),
            FriendshipStatus::Blocked
        );
        assert_ne!(
            proto_friend_status_to_domain(5),
            FriendshipStatus::Pending
        );
        assert_ne!(
            proto_friend_status_to_domain(5),
            FriendshipStatus::Accepted
        );
    }

    #[test]
    fn test_friend_request_filter_all_variants() {
        let filters = [
            FriendRequestFilter::All,
            FriendRequestFilter::Pending,
            FriendRequestFilter::Accepted,
            FriendRequestFilter::Sent,
            FriendRequestFilter::Received,
        ];

        let proto_values: Vec<_> = filters.iter().map(|f| f.to_proto() as i32).collect();
        for i in 0..proto_values.len() {
            for j in (i + 1)..proto_values.len() {
                assert_ne!(
                    proto_values[i], proto_values[j],
                    "Filter variants should produce distinct values"
                );
            }
        }
    }
}
