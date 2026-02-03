//! Moderation domain entity.
//!
//! Represents bans, mutes, and moderation log entries.
//!
//! Note: Roles and permissions are defined in `channel.rs` (MemberRole, Permissions).
//! This module focuses on moderation actions, not role management.

use crate::domain::ids::{ChannelId, UserId};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ============================================================================
// Moderation Action
// ============================================================================

/// Type of moderation action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModerationAction {
    /// User was banned from the channel.
    Ban,
    /// User was muted (cannot send messages).
    Mute,
    /// User was kicked from the channel.
    Kick,
    /// User received a warning.
    Warn,
}

// ============================================================================
// Ban
// ============================================================================

/// Ban information for a user in a channel.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Ban {
    /// Unique identifier for this ban.
    pub id: Uuid,
    /// Channel the user is banned from.
    pub channel_id: ChannelId,
    /// The banned user.
    pub user_id: UserId,
    /// The moderator who issued the ban.
    pub banned_by: UserId,
    /// Reason for the ban.
    pub reason: Option<String>,
    /// When the ban expires (None = permanent).
    pub expires_at: Option<DateTime<Utc>>,
    /// When the ban was created.
    pub created_at: DateTime<Utc>,
}

impl Ban {
    /// Create a new permanent ban.
    #[must_use]
    pub fn new(channel_id: ChannelId, user_id: UserId, banned_by: UserId) -> Self {
        Self {
            id: Uuid::new_v4(),
            channel_id,
            user_id,
            banned_by,
            reason: None,
            expires_at: None,
            created_at: Utc::now(),
        }
    }

    /// Set an expiry duration for the ban.
    #[must_use]
    pub fn with_expiry(mut self, duration: Duration) -> Self {
        self.expires_at = Some(Utc::now() + duration);
        self
    }

    /// Set the reason for the ban.
    #[must_use]
    pub fn with_reason(mut self, reason: String) -> Self {
        self.reason = Some(reason);
        self
    }

    /// Check if the ban has expired.
    #[must_use]
    pub fn is_expired(&self) -> bool {
        self.expires_at
            .map(|expires| Utc::now() > expires)
            .unwrap_or(false)
    }

    /// Check if the ban is permanent (no expiry).
    #[must_use]
    pub fn is_permanent(&self) -> bool {
        self.expires_at.is_none()
    }

    /// Check if the ban is currently active (not expired).
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.is_expired()
    }

    /// Get the remaining duration of the ban.
    ///
    /// Returns `None` if the ban is permanent or has expired.
    #[must_use]
    pub fn time_remaining(&self) -> Option<Duration> {
        self.expires_at.and_then(|expires| {
            let remaining = expires - Utc::now();
            if remaining > Duration::zero() {
                Some(remaining)
            } else {
                None
            }
        })
    }
}

// ============================================================================
// Mute
// ============================================================================

/// Mute information for a user in a channel.
///
/// Unlike bans, mutes always have an expiry time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Mute {
    /// Unique identifier for this mute.
    pub id: Uuid,
    /// Channel where the user is muted.
    pub channel_id: ChannelId,
    /// The muted user.
    pub user_id: UserId,
    /// The moderator who issued the mute.
    pub muted_by: UserId,
    /// Reason for the mute.
    pub reason: Option<String>,
    /// When the mute expires.
    pub expires_at: DateTime<Utc>,
    /// When the mute was created.
    pub created_at: DateTime<Utc>,
}

impl Mute {
    /// Create a new mute with the specified duration.
    #[must_use]
    pub fn new(
        channel_id: ChannelId,
        user_id: UserId,
        muted_by: UserId,
        duration: Duration,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            channel_id,
            user_id,
            muted_by,
            reason: None,
            expires_at: now + duration,
            created_at: now,
        }
    }

    /// Set the reason for the mute.
    #[must_use]
    pub fn with_reason(mut self, reason: String) -> Self {
        self.reason = Some(reason);
        self
    }

    /// Check if the mute has expired.
    #[must_use]
    pub fn is_expired(&self) -> bool {
        Utc::now() > self.expires_at
    }

    /// Check if the mute is currently active (not expired).
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.is_expired()
    }

    /// Get the remaining duration of the mute.
    ///
    /// Returns `None` if the mute has expired.
    #[must_use]
    pub fn time_remaining(&self) -> Option<Duration> {
        let remaining = self.expires_at - Utc::now();
        if remaining > Duration::zero() {
            Some(remaining)
        } else {
            None
        }
    }

    /// Extend the mute by the given duration.
    pub fn extend(&mut self, duration: Duration) {
        self.expires_at += duration;
    }
}

// ============================================================================
// Moderation Log Entry
// ============================================================================

/// A log entry for a moderation action.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModerationLogEntry {
    /// Unique identifier for this log entry.
    pub id: Uuid,
    /// Channel where the action took place.
    pub channel_id: ChannelId,
    /// The user who was the target of the action.
    pub target_user_id: UserId,
    /// The moderator who performed the action.
    pub moderator_id: UserId,
    /// The type of action performed.
    pub action: ModerationAction,
    /// Reason for the action.
    pub reason: Option<String>,
    /// When the action was performed.
    pub created_at: DateTime<Utc>,
}

impl ModerationLogEntry {
    /// Create a new moderation log entry.
    #[must_use]
    pub fn new(
        channel_id: ChannelId,
        target_user_id: UserId,
        moderator_id: UserId,
        action: ModerationAction,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            channel_id,
            target_user_id,
            moderator_id,
            action,
            reason: None,
            created_at: Utc::now(),
        }
    }

    /// Set the reason for the action.
    #[must_use]
    pub fn with_reason(mut self, reason: String) -> Self {
        self.reason = Some(reason);
        self
    }

    /// Create a log entry for a ban.
    #[must_use]
    pub fn for_ban(ban: &Ban) -> Self {
        Self {
            id: Uuid::new_v4(),
            channel_id: ban.channel_id,
            target_user_id: ban.user_id,
            moderator_id: ban.banned_by,
            action: ModerationAction::Ban,
            reason: ban.reason.clone(),
            created_at: ban.created_at,
        }
    }

    /// Create a log entry for a mute.
    #[must_use]
    pub fn for_mute(mute: &Mute) -> Self {
        Self {
            id: Uuid::new_v4(),
            channel_id: mute.channel_id,
            target_user_id: mute.user_id,
            moderator_id: mute.muted_by,
            action: ModerationAction::Mute,
            reason: mute.reason.clone(),
            created_at: mute.created_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ban_lifecycle() {
        let channel_id = ChannelId::new();
        let user_id = UserId::new();
        let mod_id = UserId::new();

        // Permanent ban
        let ban = Ban::new(channel_id, user_id, mod_id);
        assert!(ban.is_permanent());
        assert!(ban.is_active());
        assert!(!ban.is_expired());

        // Temporary ban
        let temp_ban = Ban::new(channel_id, user_id, mod_id)
            .with_expiry(Duration::hours(24))
            .with_reason("Spam".to_string());
        assert!(!temp_ban.is_permanent());
        assert!(temp_ban.is_active());
        assert!(temp_ban.time_remaining().is_some());
        assert_eq!(temp_ban.reason, Some("Spam".to_string()));
    }

    #[test]
    fn test_ban_expired() {
        let channel_id = ChannelId::new();
        let user_id = UserId::new();
        let mod_id = UserId::new();

        let mut ban = Ban::new(channel_id, user_id, mod_id);
        ban.expires_at = Some(Utc::now() - Duration::hours(1));

        assert!(ban.is_expired());
        assert!(!ban.is_active());
        assert!(ban.time_remaining().is_none());
    }

    #[test]
    fn test_mute_lifecycle() {
        let channel_id = ChannelId::new();
        let user_id = UserId::new();
        let mod_id = UserId::new();

        let mute = Mute::new(channel_id, user_id, mod_id, Duration::hours(1))
            .with_reason("Being disruptive".to_string());

        assert!(mute.is_active());
        assert!(!mute.is_expired());
        assert!(mute.time_remaining().is_some());
        assert_eq!(mute.reason, Some("Being disruptive".to_string()));
    }

    #[test]
    fn test_mute_expired() {
        let channel_id = ChannelId::new();
        let user_id = UserId::new();
        let mod_id = UserId::new();

        let mut mute = Mute::new(channel_id, user_id, mod_id, Duration::hours(1));
        mute.expires_at = Utc::now() - Duration::hours(1);

        assert!(mute.is_expired());
        assert!(!mute.is_active());
        assert!(mute.time_remaining().is_none());
    }

    #[test]
    fn test_mute_extend() {
        let channel_id = ChannelId::new();
        let user_id = UserId::new();
        let mod_id = UserId::new();

        let mut mute = Mute::new(channel_id, user_id, mod_id, Duration::hours(1));
        let original_expires = mute.expires_at;

        mute.extend(Duration::hours(1));
        assert!(mute.expires_at > original_expires);
    }

    #[test]
    fn test_moderation_log_entry() {
        let channel_id = ChannelId::new();
        let user_id = UserId::new();
        let mod_id = UserId::new();

        let entry = ModerationLogEntry::new(channel_id, user_id, mod_id, ModerationAction::Warn)
            .with_reason("First warning".to_string());

        assert_eq!(entry.action, ModerationAction::Warn);
        assert_eq!(entry.reason, Some("First warning".to_string()));
    }

    #[test]
    fn test_log_entry_from_ban() {
        let channel_id = ChannelId::new();
        let user_id = UserId::new();
        let mod_id = UserId::new();

        let ban = Ban::new(channel_id, user_id, mod_id)
            .with_reason("Repeated violations".to_string());
        let entry = ModerationLogEntry::for_ban(&ban);

        assert_eq!(entry.action, ModerationAction::Ban);
        assert_eq!(entry.target_user_id, user_id);
        assert_eq!(entry.moderator_id, mod_id);
        assert_eq!(entry.reason, Some("Repeated violations".to_string()));
    }

    #[test]
    fn test_log_entry_from_mute() {
        let channel_id = ChannelId::new();
        let user_id = UserId::new();
        let mod_id = UserId::new();

        let mute = Mute::new(channel_id, user_id, mod_id, Duration::minutes(30))
            .with_reason("Cool down".to_string());
        let entry = ModerationLogEntry::for_mute(&mute);

        assert_eq!(entry.action, ModerationAction::Mute);
        assert_eq!(entry.target_user_id, user_id);
        assert_eq!(entry.moderator_id, mod_id);
        assert_eq!(entry.reason, Some("Cool down".to_string()));
    }
}
