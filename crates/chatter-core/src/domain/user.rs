//! User domain entity.
//!
//! Represents users, profiles, and presence status.

use crate::domain::ids::{DeviceId, UserId};
use chrono::{DateTime, Utc};
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
    /// UI/string token for this presence (`online`/`away`/`dnd`/`offline`).
    /// Single source of truth so the UI never re-maps the enum.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Away => "away",
            Self::DoNotDisturb => "dnd",
            Self::Offline => "offline",
        }
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

impl Device {}

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

    /// Accept the friendship request.
    pub fn accept(&mut self) {
        self.status = FriendshipStatus::Accepted;
    }
}
