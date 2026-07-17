//! Proto/domain type conversions for channels.

use chrono::Utc;

use crate::util::time::TimestampNanos;

use crate::domain::channel::{
    Channel, ChannelConfig, ChannelType, MemberRole, Membership, MessagePersistence,
    NotificationPreference,
};
use crate::domain::ids::{ChannelId, UserId};
use crate::proto::chat_v1::{self, ChannelRole};

use super::error::ChannelError;

/// Convert a proto Channel to a domain Channel.
pub fn proto_to_domain_channel(proto: &chat_v1::Channel) -> Result<Channel, ChannelError> {
    // Parse channel ID
    let id: ChannelId = proto
        .id
        .parse()
        .map_err(|_| ChannelError::InvalidInput("invalid channel ID".to_string()))?;

    // Parse owner ID
    let owner_id: UserId = proto
        .owner_id
        .parse()
        .map_err(|_| ChannelError::InvalidInput("invalid owner ID".to_string()))?;

    // Convert proto ChannelType to domain ChannelType
    let channel_type = proto_channel_type_to_domain(proto.r#type);

    // Convert proto Config to domain ChannelConfig
    let config = proto_config_to_domain(proto.config.as_ref());

    // Convert timestamp (proto uses nanos)
    let created_at = TimestampNanos::from_nanos(proto.created_at).to_datetime().unwrap_or_else(Utc::now);

    Ok(Channel {
        id,
        channel_type,
        name: proto.name.clone(),
        description: if proto.description.is_empty() {
            None
        } else {
            Some(proto.description.clone())
        },
        icon: if proto.icon.is_empty() {
            None
        } else {
            Some(proto.icon.clone())
        },
        owner_id,
        config,
        created_at,
        updated_at: created_at, // Proto doesn't have updated_at, use created_at
        archived: proto.archived,
        unread_count: 0,       // Local state, not from server
        last_message_at: None, // Local state, not from server
        member_count: proto.members.len() as u32,
    })
}

/// Convert proto ChannelType (i32) to domain ChannelType.
///
/// The server no longer has a DIRECT_MESSAGE channel type (DMs are
/// channel-free, Signal-style). Value `3` is unused/reserved on the wire.
pub fn proto_channel_type_to_domain(proto_type: i32) -> ChannelType {
    match proto_type {
        1 => ChannelType::Public,
        2 => ChannelType::Private,
        4 => ChannelType::SavedMessages,
        _ => ChannelType::Public, // Default for unspecified
    }
}

/// Convert domain ChannelType to proto ChannelType.
///
/// `ChannelType::DirectMessage` has no server-side channel type anymore
/// (DMs are channel-free); this arm is unreachable in practice since no
/// client code path creates/updates a DM channel. Mapped to `ChannelPrivate`
/// defensively rather than panicking.
pub fn domain_channel_type_to_proto(channel_type: ChannelType) -> chat_v1::ChannelType {
    match channel_type {
        ChannelType::Public => chat_v1::ChannelType::ChannelPublic,
        ChannelType::Private | ChannelType::DirectMessage => chat_v1::ChannelType::ChannelPrivate,
        ChannelType::SavedMessages => chat_v1::ChannelType::SavedMessages,
    }
}

/// Convert proto Config to domain ChannelConfig.
pub fn proto_config_to_domain(proto_config: Option<&chat_v1::channel::Config>) -> ChannelConfig {
    match proto_config {
        Some(config) => ChannelConfig {
            persistence: proto_persistence_to_domain(config.persistence),
            max_members: if config.max_members > 0 {
                Some(config.max_members)
            } else {
                None
            },
            default_ttl_seconds: config.ttl.as_ref().map(|d| d.seconds as u32),
            allow_reactions: true,   // Default, not in proto
            allow_threads: true,     // Default, not in proto
            allow_attachments: true, // Default, not in proto
            require_encryption: config.encrypted,
        },
        None => ChannelConfig::default(),
    }
}

/// Convert domain ChannelConfig to proto Config.
pub fn domain_config_to_proto(config: &ChannelConfig) -> chat_v1::channel::Config {
    chat_v1::channel::Config {
        persistence: domain_persistence_to_proto(config.persistence) as i32,
        // A 0-second TTL means "no expiry" (persistent channel). The proto rule
        // requires `ttl` to be strictly `> 0s` WHEN PRESENT, so emit `None` rather
        // than `Duration{0}` — otherwise every UpdateChannel on a non-ephemeral
        // channel (which round-trips default_ttl_seconds=Some(0)) is rejected by
        // protovalidate before it reaches the handler.
        ttl: config
            .default_ttl_seconds
            .filter(|&s| s > 0)
            .map(|s| prost_types::Duration {
                seconds: s as i64,
                nanos: 0,
            }),
        // Proto requires 0 < max_members <= 10000; None means "no user-set limit" → cap at max.
        max_members: config.max_members.unwrap_or(10000),
        encrypted: config.require_encryption,
        password_hash: String::new(),
        friends_only: false,
    }
}

/// Convert proto Persistence to domain MessagePersistence.
pub fn proto_persistence_to_domain(proto_persistence: i32) -> MessagePersistence {
    match proto_persistence {
        1 => MessagePersistence::Ephemeral,
        2 => MessagePersistence::Permanent,
        _ => MessagePersistence::Permanent, // Default
    }
}

/// Convert domain MessagePersistence to proto Persistence.
pub fn domain_persistence_to_proto(
    persistence: MessagePersistence,
) -> chat_v1::channel::config::Persistence {
    match persistence {
        MessagePersistence::Permanent => chat_v1::channel::config::Persistence::Persistent,
        MessagePersistence::Ephemeral => chat_v1::channel::config::Persistence::Ephemeral,
        MessagePersistence::Hybrid => chat_v1::channel::config::Persistence::Persistent,
    }
}

/// Convert a proto ChannelMember to domain Membership.
pub fn proto_to_domain_membership(
    channel_id: &ChannelId,
    proto: &chat_v1::channel::ChannelMember,
) -> Result<Membership, ChannelError> {
    let user_id: UserId = proto
        .user_id
        .parse()
        .map_err(|_| ChannelError::InvalidInput("invalid user ID in member".to_string()))?;

    let role = proto_role_to_domain(proto.role);
    let joined_at = TimestampNanos::from_nanos(proto.joined_at).to_datetime().unwrap_or_else(Utc::now);

    Ok(Membership {
        channel_id: *channel_id,
        user_id,
        role,
        joined_at,
        last_read_message_id: None, // Not in proto
        is_muted: proto.muted,
        muted_until: None,                              // Not in proto (ban_expires_at is for bans, not mutes)
        notification_preference: NotificationPreference::All, // Not in proto
    })
}

/// Convert proto ChannelRole (i32) to domain MemberRole.
pub fn proto_role_to_domain(proto_role: i32) -> MemberRole {
    match proto_role {
        1 => MemberRole::Member,
        2 => MemberRole::Moderator,
        3 => MemberRole::Admin,
        4 => MemberRole::Owner,
        _ => MemberRole::Member, // Default for unspecified
    }
}

/// Convert domain MemberRole to proto ChannelRole.
pub fn domain_role_to_proto(role: MemberRole) -> ChannelRole {
    match role {
        MemberRole::Member => ChannelRole::Member,
        MemberRole::Moderator => ChannelRole::Moderator,
        MemberRole::Admin => ChannelRole::Admin,
        MemberRole::Owner => ChannelRole::Owner,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proto_channel_type_conversion() {
        assert_eq!(proto_channel_type_to_domain(1), ChannelType::Public);
        assert_eq!(proto_channel_type_to_domain(2), ChannelType::Private);
        assert_eq!(proto_channel_type_to_domain(4), ChannelType::SavedMessages);
        assert_eq!(proto_channel_type_to_domain(0), ChannelType::Public); // Unspecified
        assert_eq!(proto_channel_type_to_domain(3), ChannelType::Public); // Unused/reserved
    }

    #[test]
    fn test_domain_channel_type_conversion() {
        assert_eq!(
            domain_channel_type_to_proto(ChannelType::Public),
            chat_v1::ChannelType::ChannelPublic
        );
        assert_eq!(
            domain_channel_type_to_proto(ChannelType::Private),
            chat_v1::ChannelType::ChannelPrivate
        );
        assert_eq!(
            domain_channel_type_to_proto(ChannelType::DirectMessage),
            chat_v1::ChannelType::ChannelPrivate
        );
    }

    #[test]
    fn test_nanos_to_datetime() {
        let nanos = 1706140800_000_000_000u64; // 2024-01-25 00:00:00 UTC in nanoseconds
        let dt = TimestampNanos::from_nanos(nanos).to_datetime().unwrap();
        assert_eq!(dt.timestamp_nanos_opt().unwrap(), nanos as i64);
    }

    #[test]
    fn test_proto_role_to_domain() {
        assert_eq!(proto_role_to_domain(0), MemberRole::Member); // Unspecified
        assert_eq!(proto_role_to_domain(1), MemberRole::Member);
        assert_eq!(proto_role_to_domain(2), MemberRole::Moderator);
        assert_eq!(proto_role_to_domain(3), MemberRole::Admin);
        assert_eq!(proto_role_to_domain(4), MemberRole::Owner);
    }

    #[test]
    fn test_domain_role_to_proto() {
        assert_eq!(domain_role_to_proto(MemberRole::Member), ChannelRole::Member);
        assert_eq!(
            domain_role_to_proto(MemberRole::Moderator),
            ChannelRole::Moderator
        );
        assert_eq!(domain_role_to_proto(MemberRole::Admin), ChannelRole::Admin);
        assert_eq!(domain_role_to_proto(MemberRole::Owner), ChannelRole::Owner);
    }

    #[test]
    fn test_role_conversion_roundtrip() {
        for role in [
            MemberRole::Member,
            MemberRole::Moderator,
            MemberRole::Admin,
            MemberRole::Owner,
        ] {
            let proto = domain_role_to_proto(role);
            let back = proto_role_to_domain(proto as i32);
            assert_eq!(back, role);
        }
    }

    #[test]
    fn test_channel_type_roundtrip() {
        // DirectMessage is intentionally excluded: it has no server-side
        // proto counterpart anymore (DMs are channel-free), so it does not
        // round-trip.
        for ct in [ChannelType::Public, ChannelType::Private] {
            let proto = domain_channel_type_to_proto(ct);
            let back = proto_channel_type_to_domain(proto as i32);
            assert_eq!(back, ct);
        }
    }

    #[test]
    fn test_proto_persistence_conversion() {
        assert_eq!(
            proto_persistence_to_domain(0),
            MessagePersistence::Permanent
        ); // Unspecified
        assert_eq!(
            proto_persistence_to_domain(1),
            MessagePersistence::Ephemeral
        );
        assert_eq!(
            proto_persistence_to_domain(2),
            MessagePersistence::Permanent
        );
    }

    #[test]
    fn test_domain_persistence_conversion() {
        assert_eq!(
            domain_persistence_to_proto(MessagePersistence::Permanent),
            chat_v1::channel::config::Persistence::Persistent
        );
        assert_eq!(
            domain_persistence_to_proto(MessagePersistence::Ephemeral),
            chat_v1::channel::config::Persistence::Ephemeral
        );
        assert_eq!(
            domain_persistence_to_proto(MessagePersistence::Hybrid),
            chat_v1::channel::config::Persistence::Persistent
        );
    }

    #[test]
    fn test_proto_config_to_domain_none() {
        let config = proto_config_to_domain(None);
        assert_eq!(config, ChannelConfig::default());
    }

    #[test]
    fn test_proto_config_to_domain_full() {
        let proto_config = chat_v1::channel::Config {
            persistence: 1, // Ephemeral
            ttl: Some(prost_types::Duration {
                seconds: 3600,
                nanos: 0,
            }),
            max_members: 100,
            encrypted: true,
            password_hash: String::new(),
            friends_only: false,
        };

        let config = proto_config_to_domain(Some(&proto_config));
        assert_eq!(config.persistence, MessagePersistence::Ephemeral);
        assert_eq!(config.max_members, Some(100));
        assert_eq!(config.default_ttl_seconds, Some(3600));
        assert!(config.require_encryption);
    }

    #[test]
    fn test_domain_config_to_proto() {
        let config = ChannelConfig {
            persistence: MessagePersistence::Ephemeral,
            max_members: Some(50),
            default_ttl_seconds: Some(1800),
            allow_reactions: true,
            allow_threads: true,
            allow_attachments: true,
            require_encryption: true,
        };

        let proto = domain_config_to_proto(&config);
        assert_eq!(
            proto.persistence,
            chat_v1::channel::config::Persistence::Ephemeral as i32
        );
        assert_eq!(proto.max_members, 50);
        assert!(proto.encrypted);
        assert!(proto.ttl.is_some());
        assert_eq!(proto.ttl.unwrap().seconds, 1800);
    }

    #[test]
    fn test_proto_to_domain_channel() {
        let proto = chat_v1::Channel {
            id: "00000000-0000-0000-0000-000000000001".to_string(),
            r#type: 1, // Public
            name: "test-channel".to_string(),
            config: Some(chat_v1::channel::Config {
                persistence: 2,
                ttl: None,
                max_members: 100,
                encrypted: false,
                password_hash: String::new(),
                friends_only: false,
            }),
            owner_id: "00000000-0000-0000-0000-000000000002".to_string(),
            members: vec![],
            created_at: 1706140800000,
            description: "A test channel".to_string(),
            icon: vec![0x89, b'P', b'N', b'G'],
            archived: false,
        };

        let channel = proto_to_domain_channel(&proto).unwrap();
        assert_eq!(channel.name, "test-channel");
        assert_eq!(channel.channel_type, ChannelType::Public);
        assert_eq!(channel.description, Some("A test channel".to_string()));
        assert_eq!(channel.icon, Some(vec![0x89, b'P', b'N', b'G']));
        assert!(!channel.archived);
    }

    #[test]
    fn test_proto_to_domain_channel_empty_strings() {
        let proto = chat_v1::Channel {
            id: "00000000-0000-0000-0000-000000000001".to_string(),
            r#type: 2, // Private
            name: "private-channel".to_string(),
            config: None,
            owner_id: "00000000-0000-0000-0000-000000000002".to_string(),
            members: vec![],
            created_at: 1706140800000,
            description: "".to_string(), // Empty
            icon: Vec::new(),            // Empty
            archived: true,
        };

        let channel = proto_to_domain_channel(&proto).unwrap();
        assert_eq!(channel.channel_type, ChannelType::Private);
        assert_eq!(channel.description, None);
        assert_eq!(channel.icon, None);
        assert!(channel.archived);
    }
}
