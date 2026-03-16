//! Tests for user service.

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::domain::ids::UserId;
    use crate::domain::user::{FriendshipStatus, PresenceStatus, Profile, StatusMessage};
    use crate::proto::chat_v1::{friend_requests_filter, UserProfile};
    use crate::storage::repositories::settings_repo::SettingsRepository;
    use crate::storage::repositories::user_repo::UserRepository;

    use super::super::conversions::{
        domain_presence_to_proto, domain_to_proto_profile, presence_from_proto,
        proto_friend_status_to_domain, proto_to_domain_profile,
    };
    use super::super::{FriendRequestFilter, UserError, UserService};

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
            avatar_url: "https://example.com/avatar.png".to_string(),
            status_message: "Hello world".to_string(),
            presence: 2,
        };

        let profile = proto_to_domain_profile(&proto).unwrap();
        assert_eq!(profile.display_name, "Test User");
        assert_eq!(
            profile.avatar_url,
            Some("https://example.com/avatar.png".to_string())
        );
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
            avatar_url: "".to_string(),
            status_message: "".to_string(),
            presence: 1,
        };

        let profile = proto_to_domain_profile(&proto).unwrap();
        assert_eq!(profile.avatar_url, None);
        assert_eq!(profile.status_message, None);
        assert_eq!(profile.presence, PresenceStatus::Offline);
    }

    #[test]
    fn test_domain_to_proto_profile() {
        let user_id: UserId = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let profile = Profile {
            user_id,
            display_name: "Test User".to_string(),
            avatar_url: Some("https://example.com/avatar.png".to_string()),
            status_message: Some(StatusMessage::new("Hello".to_string())),
            presence: PresenceStatus::Away,
        };

        let proto = domain_to_proto_profile(&profile);
        assert_eq!(proto.display_name, "Test User");
        assert_eq!(proto.avatar_url, "https://example.com/avatar.png");
        assert_eq!(proto.status_message, "Hello");
        assert_eq!(proto.presence, 3);
    }

    #[test]
    fn test_domain_to_proto_profile_none_fields() {
        let user_id: UserId = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let profile = Profile {
            user_id,
            display_name: "Test User".to_string(),
            avatar_url: None,
            status_message: None,
            presence: PresenceStatus::DoNotDisturb,
        };

        let proto = domain_to_proto_profile(&profile);
        assert_eq!(proto.avatar_url, "");
        assert_eq!(proto.status_message, "");
        assert_eq!(proto.presence, 4);
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

    #[test]
    fn test_user_service_construction() {
        fn _verify_types(
            grpc_pool: Arc<crate::network::grpc::client_pool::GrpcClientPool>,
            settings_repo: Arc<SettingsRepository>,
            user_repo: Arc<UserRepository>,
        ) {
            let _service = UserService::new(grpc_pool, settings_repo, user_repo);
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
