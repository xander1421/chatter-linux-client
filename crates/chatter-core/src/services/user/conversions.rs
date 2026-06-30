//! Proto <-> Domain conversion helpers for user types.

use crate::domain::ids::UserId;
use crate::domain::user::{FriendshipStatus, PresenceStatus, Profile, StatusMessage};
use crate::proto::chat_v1::UserProfile;

use super::UserError;

/// Convert a proto UserProfile to a domain Profile.
pub fn proto_to_domain_profile(proto: &UserProfile) -> Result<Profile, UserError> {
    // Parse user_id
    let user_id: UserId = proto
        .user_id
        .parse()
        .map_err(|_| UserError::InvalidInput("invalid user ID in profile".to_string()))?;

    // Map proto presence (i32) to domain PresenceStatus
    let presence = presence_from_proto(proto.presence);

    // Map status_message (empty string -> None)
    let status_message = if proto.status_message.is_empty() {
        None
    } else {
        Some(StatusMessage::new(proto.status_message.clone()))
    };

    Ok(Profile {
        user_id,
        display_name: proto.display_name.clone(),
        status_message,
        presence,
    })
}

/// Convert proto UserPresence (i32) to domain PresenceStatus.
pub fn presence_from_proto(value: i32) -> PresenceStatus {
    match value {
        1 => PresenceStatus::Offline,
        2 => PresenceStatus::Online,
        3 => PresenceStatus::Away,
        4 => PresenceStatus::DoNotDisturb,
        _ => PresenceStatus::Offline,
    }
}

/// Convert domain PresenceStatus to proto UserPresence (i32).
pub fn domain_presence_to_proto(status: PresenceStatus) -> i32 {
    match status {
        PresenceStatus::Offline => 1,
        PresenceStatus::Online => 2,
        PresenceStatus::Away => 3,
        PresenceStatus::DoNotDisturb => 4,
    }
}

/// Convert proto FriendStatus (i32) to domain FriendshipStatus.
pub fn proto_friend_status_to_domain(status: i32) -> FriendshipStatus {
    match status {
        3 => FriendshipStatus::Accepted,
        5 => FriendshipStatus::Blocked,
        _ => FriendshipStatus::Pending,
    }
}
