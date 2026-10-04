//! Moderation domain entity.
//!
//! Represents bans, mutes, and moderation log entries.
//!
//! Note: Roles and permissions are defined in `channel.rs` (MemberRole, Permissions).
//! This module focuses on moderation actions, not role management.

use crate::domain::ids::{ChannelId, UserId};
use chrono::{DateTime, Utc};
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

}

