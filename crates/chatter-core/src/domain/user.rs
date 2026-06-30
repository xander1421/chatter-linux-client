//! User domain entity.
//!
//! Represents users, profiles, and presence status.

use crate::domain::ids::{DeviceId, UserId};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

// ============================================================================
// Presence Status
// ============================================================================

/// User presence status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PresenceStatus {
    #[default]
    Offline,
    Online,
    Away,
    DoNotDisturb,
}

impl PresenceStatus {
    /// Check if the user is available (online or away).
    #[must_use]
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Online | Self::Away)
    }

    /// Check if the user can be disturbed (not DND).
    #[must_use]
    pub fn can_be_disturbed(&self) -> bool {
        !matches!(self, Self::DoNotDisturb)
    }
}

// ============================================================================
// User
// ============================================================================

/// Represents a user in the system.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub id: UserId,
    pub username: String,
    /// SVR (Secure Value Recovery) public key for PIN-based key recovery.
    pub svr_public_key: Vec<u8>,
    pub last_active: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

impl User {
    /// Create a new user with the given ID, username, and SVR public key.
    /// Sets created_at and last_active to the current time.
    #[must_use]
    pub fn new(id: UserId, username: String, svr_public_key: Vec<u8>) -> Self {
        let now = Utc::now();
        Self {
            id,
            username,
            svr_public_key,
            last_active: now,
            created_at: now,
        }
    }

    /// Check if the user was active within the last 5 minutes.
    #[must_use]
    pub fn is_active(&self) -> bool {
        let five_minutes = Duration::minutes(5);
        Utc::now() - self.last_active < five_minutes
    }

    /// Update the last_active timestamp to now.
    pub fn touch(&mut self) {
        self.last_active = Utc::now();
    }
}

// ============================================================================
// Status Message
// ============================================================================

/// Custom status message (mood text).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatusMessage {
    pub text: String,
    pub emoji: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

impl StatusMessage {
    /// Create a new status message with text only.
    #[must_use]
    pub fn new(text: String) -> Self {
        Self {
            text,
            emoji: None,
            expires_at: None,
        }
    }

    /// Create a new status message with text and emoji.
    #[must_use]
    pub fn with_emoji(text: String, emoji: String) -> Self {
        Self {
            text,
            emoji: Some(emoji),
            expires_at: None,
        }
    }

    /// Check if the status message has expired.
    #[must_use]
    pub fn is_expired(&self) -> bool {
        self.expires_at
            .map(|expires| Utc::now() > expires)
            .unwrap_or(false)
    }

    /// Set an expiration time for this status.
    #[must_use]
    pub fn expiring_in(mut self, duration: Duration) -> Self {
        self.expires_at = Some(Utc::now() + duration);
        self
    }
}

// ============================================================================
// Profile
// ============================================================================

/// User profile information (display data).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub user_id: UserId,
    pub display_name: String,
    /// Decrypted avatar image bytes (normalized WebP). `None` when we don't
    /// have the owner's profile key or they have no avatar set. Sourced from
    /// the server profile's E2E `encrypted_avatar` blob, decrypted locally.
    pub avatar_bytes: Option<Vec<u8>>,
    pub status_message: Option<StatusMessage>,
    pub presence: PresenceStatus,
}

impl Profile {
    /// Create a new profile with minimal information.
    #[must_use]
    pub fn new(user_id: UserId, display_name: String) -> Self {
        Self {
            user_id,
            display_name,
            avatar_bytes: None,
            status_message: None,
            presence: PresenceStatus::default(),
        }
    }

    /// Get the display name, or fall back to the user's username if empty.
    #[must_use]
    pub fn display_name_or_username<'a>(&'a self, user: &'a User) -> &'a str {
        if self.display_name.is_empty() {
            &user.username
        } else {
            &self.display_name
        }
    }

    /// Get the active status message (if not expired).
    #[must_use]
    pub fn active_status_message(&self) -> Option<&StatusMessage> {
        self.status_message
            .as_ref()
            .filter(|msg| !msg.is_expired())
    }
}

// ============================================================================
// Device
// ============================================================================

/// User's device for multi-device support.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Device {
    pub id: DeviceId,
    pub user_id: UserId,
    pub name: String,
    /// Signal protocol registration ID for this device.
    pub registration_id: u32,
    pub created_at: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
}

impl Device {
    /// Create a new device.
    #[must_use]
    pub fn new(id: DeviceId, user_id: UserId, name: String, registration_id: u32) -> Self {
        let now = Utc::now();
        Self {
            id,
            user_id,
            name,
            registration_id,
            created_at: now,
            last_seen: now,
        }
    }

    /// Check if this is the primary device.
    #[must_use]
    pub fn is_primary(&self) -> bool {
        self.id.is_primary()
    }

    /// Update the last_seen timestamp to now.
    pub fn touch(&mut self) {
        self.last_seen = Utc::now();
    }
}

// ============================================================================
// Friendship
// ============================================================================

/// Friend request status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FriendshipStatus {
    Pending,
    Accepted,
    Blocked,
}

/// Friend relationship between users.
///
/// `requester_id` is the user who sent the friend request; `addressee_id`
/// is the user who received it. The naming is asymmetric on purpose so
/// callers don't have to guess which side is which (the previous
/// `user_id` / `friend_id` pair was ambiguous).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Friendship {
    pub requester_id: UserId,
    pub addressee_id: UserId,
    pub status: FriendshipStatus,
    pub created_at: DateTime<Utc>,
}

impl Friendship {
    /// Create a new pending friendship request.
    #[must_use]
    pub fn new_request(requester_id: UserId, addressee_id: UserId) -> Self {
        Self {
            requester_id,
            addressee_id,
            status: FriendshipStatus::Pending,
            created_at: Utc::now(),
        }
    }

    /// The other party in the friendship from `viewer`'s perspective.
    /// Returns `addressee_id` if viewer is the requester, otherwise
    /// `requester_id`. None if viewer isn't part of the friendship.
    #[must_use]
    pub fn other_party(&self, viewer: &UserId) -> Option<&UserId> {
        if &self.requester_id == viewer {
            Some(&self.addressee_id)
        } else if &self.addressee_id == viewer {
            Some(&self.requester_id)
        } else {
            None
        }
    }

    /// Check if the friendship is active (accepted).
    #[must_use]
    pub fn is_active(&self) -> bool {
        matches!(self.status, FriendshipStatus::Accepted)
    }

    /// Check if the user is blocked.
    #[must_use]
    pub fn is_blocked(&self) -> bool {
        matches!(self.status, FriendshipStatus::Blocked)
    }

    /// Accept the friendship request.
    pub fn accept(&mut self) {
        self.status = FriendshipStatus::Accepted;
    }

    /// Block the user.
    pub fn block(&mut self) {
        self.status = FriendshipStatus::Blocked;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_is_active() {
        let mut user = User::new(
            UserId::new(),
            "testuser".to_string(),
            vec![0u8; 32],
        );
        assert!(user.is_active());

        // Simulate old activity by setting last_active to 10 minutes ago
        user.last_active = Utc::now() - Duration::minutes(10);
        assert!(!user.is_active());
    }

    #[test]
    fn test_status_message_expiry() {
        let msg = StatusMessage::new("Working".to_string())
            .expiring_in(Duration::hours(1));
        assert!(!msg.is_expired());

        let mut expired_msg = StatusMessage::new("Gone".to_string());
        expired_msg.expires_at = Some(Utc::now() - Duration::hours(1));
        assert!(expired_msg.is_expired());
    }

    #[test]
    fn test_profile_display_name_fallback() {
        let user = User::new(
            UserId::new(),
            "username".to_string(),
            vec![],
        );

        let profile_with_name = Profile::new(user.id, "Display Name".to_string());
        assert_eq!(profile_with_name.display_name_or_username(&user), "Display Name");

        let profile_empty = Profile::new(user.id, "".to_string());
        assert_eq!(profile_empty.display_name_or_username(&user), "username");
    }

    #[test]
    fn test_friendship_lifecycle() {
        let user1 = UserId::new();
        let user2 = UserId::new();

        let mut friendship = Friendship::new_request(user1, user2);
        assert!(!friendship.is_active());
        assert!(!friendship.is_blocked());

        friendship.accept();
        assert!(friendship.is_active());

        friendship.block();
        assert!(!friendship.is_active());
        assert!(friendship.is_blocked());
    }
}
