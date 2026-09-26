//! Channel domain entity.
//!
//! Represents chat channels, membership, and permissions.

use crate::domain::ids::{ChannelId, UserId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Channel type (matches proto ChannelType).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ChannelType {
    #[default]
    Public,
    Private,
    DirectMessage,
    SavedMessages,
}

/// Message persistence policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MessagePersistence {
    #[default]
    Permanent,
    Ephemeral,
    Hybrid,
}

/// Channel configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChannelConfig {
    pub persistence: MessagePersistence,
    pub max_members: Option<u32>,
    pub default_ttl_seconds: Option<u32>,
    pub allow_reactions: bool,
    pub allow_threads: bool,
    pub allow_attachments: bool,
    pub require_encryption: bool,
}

impl Default for ChannelConfig {
    fn default() -> Self {
        Self {
            persistence: MessagePersistence::Permanent,
            max_members: None,
            default_ttl_seconds: None,
            allow_reactions: true,
            allow_threads: true,
            allow_attachments: true,
            require_encryption: true,
        }
    }
}

/// Represents a chat channel.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Channel {
    pub id: ChannelId,
    pub channel_type: ChannelType,
    pub name: String,
    pub description: Option<String>,
    /// Channel icon as raw image bytes (PNG/JPEG/WebP, ≤64KB).
    /// Server-managed channels store this plaintext alongside the
    /// channel record — same trust level as the channel name. Encrypted
    /// channels go through the anon-group icon path instead.
    pub icon: Option<Vec<u8>>,
    pub owner_id: UserId,
    pub config: ChannelConfig,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub archived: bool,
    /// Unread message count (local state)
    pub unread_count: u32,
    /// Last message timestamp for sorting
    pub last_message_at: Option<DateTime<Utc>>,
    /// Number of members in this channel
    pub member_count: u32,
}

impl Channel {
    /// Create a new channel with defaults.
    #[must_use]
    pub fn new(id: ChannelId, name: String, channel_type: ChannelType, owner_id: UserId) -> Self {
        let now = Utc::now();
        Self {
            id,
            channel_type,
            name,
            description: None,
            icon: None,
            owner_id,
            config: ChannelConfig::default(),
            created_at: now,
            updated_at: now,
            archived: false,
            unread_count: 0,
            last_message_at: None,
            member_count: 0,
        }
    }

    /// Check if this is a direct message channel.
    #[must_use]
    pub fn is_dm(&self) -> bool {
        matches!(self.channel_type, ChannelType::DirectMessage)
    }

    /// Check if this channel is private (Private or DirectMessage).
    #[must_use]
    pub fn is_private(&self) -> bool {
        matches!(
            self.channel_type,
            ChannelType::Private | ChannelType::DirectMessage
        )
    }

    /// Get the correct ConversationType for this channel.
    ///
    /// Every row in the channels table is a server channel, encrypted or not:
    /// `require_encryption` picks sender-key E2E on the wire, not a different
    /// conversation. Anon groups live in their own `anon_groups` table and are
    /// never a `Channel`, so `AnonGroup` is not reachable from here.
    #[must_use]
    pub fn conversation_type(&self) -> crate::domain::message::ConversationType {
        use crate::domain::message::ConversationType;
        match self.channel_type {
            ChannelType::DirectMessage => ConversationType::ServerDm,
            ChannelType::SavedMessages => ConversationType::SavedMessages,
            _ => ConversationType::Channel,
        }
    }

    /// Whether `user_id` owns this channel.
    #[must_use]
    pub fn is_owner(&self, user_id: &UserId) -> bool {
        &self.owner_id == user_id
    }

    /// Whether `user_id` may edit this channel's metadata (name, description, icon).
    ///
    /// Owner-only, and DMs are not editable (no name/description to set).
    #[must_use]
    pub fn can_user_edit(&self, user_id: &UserId) -> bool {
        self.is_owner(user_id) && !self.is_dm()
    }

    /// Whether `user_id` may add/remove members or change roles.
    ///
    /// Owner-only, and DMs have a fixed two-party membership.
    #[must_use]
    pub fn can_user_manage_members(&self, user_id: &UserId) -> bool {
        self.is_owner(user_id) && !self.is_dm()
    }

    /// Whether `user_id` may archive/unarchive this channel.
    ///
    /// Owner-only, and DMs cannot be archived.
    #[must_use]
    pub fn can_user_archive(&self, user_id: &UserId) -> bool {
        self.is_owner(user_id) && !self.is_dm()
    }

    /// Whether `user_id` may delete this channel.
    ///
    /// Owner-only, and DMs are deleted via the DM-delete flow, not channel delete.
    #[must_use]
    pub fn can_user_delete(&self, user_id: &UserId) -> bool {
        self.is_owner(user_id) && !self.is_dm()
    }

    /// Whether `user_id` may leave this channel. Every member but the owner:
    /// the server refuses an owner's leave while others remain
    /// (`channels.go` Leave, "cannot leave without transferring ownership"),
    /// so the owner transfers ownership first (then leaves as an admin) or
    /// deletes the channel.
    /// `still_member` is false once we left or were removed.
    #[must_use]
    pub fn can_user_leave(&self, user_id: &UserId, still_member: bool) -> bool {
        still_member
            && !self.is_owner(user_id)
            && !matches!(self.channel_type, ChannelType::DirectMessage | ChannelType::SavedMessages)
    }
}

/// Channel member role.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MemberRole {
    #[default]
    Member,
    Moderator,
    Admin,
    Owner,
}

impl MemberRole {
    /// UI/string token for this role. Single source of truth so the UI never
    /// re-maps the enum.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Member => "member",
            Self::Moderator => "moderator",
            Self::Admin => "admin",
            Self::Owner => "owner",
        }
    }

    /// Position in the server's role order (`ChannelRole` in common.proto).
    fn rank(self) -> u8 {
        match self {
            Self::Member => 1,
            Self::Moderator => 2,
            Self::Admin => 3,
            Self::Owner => 4,
        }
    }

    /// Whether this role may ban, unban and see the ban list (owner+admin,
    /// `ban_user`/`unban_user` in schema.zed).
    #[must_use]
    pub fn can_unban(self) -> bool {
        matches!(self, Self::Owner | Self::Admin)
    }

    /// Inverse of [`Self::as_str`].
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        match token {
            "member" => Some(Self::Member),
            "moderator" => Some(Self::Moderator),
            "admin" => Some(Self::Admin),
            "owner" => Some(Self::Owner),
            _ => None,
        }
    }
}

/// Channel membership information.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Membership {
    pub channel_id: ChannelId,
    pub user_id: UserId,
    pub role: MemberRole,
    pub joined_at: DateTime<Utc>,
    /// Last read message ID for tracking unread
    pub last_read_message_id: Option<crate::domain::ids::MessageId>,
    pub is_muted: bool,
    pub muted_until: Option<DateTime<Utc>>,
    pub notification_preference: NotificationPreference,
}

/// Channel member with profile information.
///
/// Combines membership data with user profile for display.
#[derive(Clone, Debug)]
pub struct MemberWithProfile {
    pub membership: Membership,
    pub display_name: String,
    /// The member's handle, empty when this device does not know it.
    pub username: String,
    /// Decrypted avatar bytes from the member's server profile, if known.
    pub avatar_bytes: Option<Vec<u8>>,
    /// What the viewing user may do to this member.
    pub moderation: MemberModeration,
}

/// What the viewing user may do to one channel member.
///
/// Mirrors the server's checks so the UI never offers an action the server
/// refuses: `ban_user` is owner+admin and `mute_user` owner+admin+moderator
/// (`internal/utils/authz/schema.zed`), nobody acts on themselves or the
/// owner, and a non-owner must strictly outrank the target
/// (`internal/services/channel/service/moderation.go`). Only the owner may
/// hand the channel to another member (`TransferOwnership`, `members.go`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemberModeration {
    pub can_ban: bool,
    pub can_mute: bool,
    pub can_make_owner: bool,
}

impl MemberModeration {
    /// `actor` is the viewer's role in the channel (`None` = not a member).
    #[must_use]
    pub fn for_target(actor: Option<MemberRole>, actor_is_target: bool, target: MemberRole) -> Self {
        let Some(actor) = actor else { return Self::default() };
        if actor_is_target || target == MemberRole::Owner {
            return Self::default();
        }
        let outranks = actor == MemberRole::Owner || actor.rank() > target.rank();
        Self {
            can_ban: outranks && actor.can_unban(),
            can_mute: outranks && actor.rank() >= MemberRole::Moderator.rank(),
            can_make_owner: actor == MemberRole::Owner,
        }
    }
}

/// Why the viewer may not post in a channel. The composer is replaced by
/// [`Self::notice`], as Signal replaces it for a member who can't send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendBlock {
    /// A moderator muted us; the server refuses our sends until it lapses
    /// (`until`) or, when `None`, until a moderator lifts it.
    Muted { until: Option<DateTime<Utc>> },
    /// We left, or were kicked or banned (our membership is gone).
    Removed,
    /// The channel was deleted.
    Deleted,
}

impl SendBlock {
    /// `own` is our stored membership; `roster_known` whether any membership
    /// is stored for the channel at all, so an unsynced roster never reads as
    /// "removed"; `former` the channel's former-channel state (`Some(deleted)`
    /// once we are out of it, see `ChannelRepository::mark_former`).
    #[must_use]
    pub fn for_viewer(own: Option<&Membership>, roster_known: bool, former: Option<bool>) -> Option<Self> {
        match own {
            Some(m) if m.is_muted => Some(Self::Muted { until: m.muted_until }),
            Some(_) => None,
            None if former == Some(true) => Some(Self::Deleted),
            None if roster_known || former.is_some() => Some(Self::Removed),
            None => None,
        }
    }

    #[must_use]
    pub fn notice(self) -> String {
        self.notice_at(chrono::Local::now())
    }

    /// [`Self::notice`] with the mute's end in `now`'s time zone: "until
    /// 15:30" today, "until tomorrow 09:00", else "until Oct 03, 09:00".
    #[must_use]
    pub fn notice_at<Tz: chrono::TimeZone>(self, now: DateTime<Tz>) -> String
    where
        Tz::Offset: std::fmt::Display,
    {
        const MUTED: &str = "You can't send messages in this channel while muted";
        match self {
            Self::Muted { until: None } => MUTED.to_string(),
            Self::Muted { until: Some(until) } => {
                let until = until.with_timezone(&now.timezone());
                let (day, today) = (until.date_naive(), now.date_naive());
                let when = if day == today {
                    until.format("%H:%M").to_string()
                } else if today.succ_opt() == Some(day) {
                    until.format("tomorrow %H:%M").to_string()
                } else {
                    until.format("%b %d, %H:%M").to_string()
                };
                format!("{MUTED} until {when}")
            }
            // Signal's wording, true whether we left or were removed.
            Self::Removed => "You're no longer a member of this channel".to_string(),
            Self::Deleted => "This channel was deleted".to_string(),
        }
    }
}

/// One active ban on a channel, shaped for display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BannedMember {
    pub user_id: UserId,
    pub banned_at: DateTime<Utc>,
    pub display_name: String,
    /// Decrypted avatar bytes from the user's stored profile, if known.
    pub avatar_bytes: Option<Vec<u8>>,
}

/// Notification preference for a channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum NotificationPreference {
    #[default]
    All,
    Mentions,
    None,
}

/// Channel permissions (what actions a role can perform).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Permissions {
    pub can_send_messages: bool,
    pub can_edit_own_messages: bool,
    pub can_delete_own_messages: bool,
    pub can_delete_any_message: bool,
    pub can_pin_messages: bool,
    pub can_manage_members: bool,
    pub can_manage_roles: bool,
    pub can_edit_channel: bool,
    pub can_delete_channel: bool,
}

impl Permissions {
    /// Default permissions for a member.
    #[must_use]
    pub fn member() -> Self {
        Self {
            can_send_messages: true,
            can_edit_own_messages: true,
            can_delete_own_messages: true,
            can_delete_any_message: false,
            can_pin_messages: false,
            can_manage_members: false,
            can_manage_roles: false,
            can_edit_channel: false,
            can_delete_channel: false,
        }
    }

    /// Permissions for a moderator.
    #[must_use]
    pub fn moderator() -> Self {
        Self {
            can_send_messages: true,
            can_edit_own_messages: true,
            can_delete_own_messages: true,
            can_delete_any_message: true,
            can_pin_messages: true,
            can_manage_members: true,
            can_manage_roles: false,
            can_edit_channel: false,
            can_delete_channel: false,
        }
    }

    /// Full permissions for owner/admin.
    #[must_use]
    pub fn admin() -> Self {
        Self {
            can_send_messages: true,
            can_edit_own_messages: true,
            can_delete_own_messages: true,
            can_delete_any_message: true,
            can_pin_messages: true,
            can_manage_members: true,
            can_manage_roles: true,
            can_edit_channel: true,
            can_delete_channel: true,
        }
    }

    /// Permissions for DM participants — both users can delete any message.
    #[must_use]
    pub fn dm() -> Self {
        Self {
            can_send_messages: true,
            can_edit_own_messages: true,
            can_delete_own_messages: true,
            can_delete_any_message: false,
            can_pin_messages: true,
            can_manage_members: false,
            can_manage_roles: false,
            can_edit_channel: false,
            can_delete_channel: false,
        }
    }

    /// Get permissions for a given role.
    #[must_use]
    pub fn for_role(role: MemberRole) -> Self {
        match role {
            MemberRole::Member => Self::member(),
            MemberRole::Moderator => Self::moderator(),
            MemberRole::Admin | MemberRole::Owner => Self::admin(),
        }
    }
}

/// Channel template for pre-configured channels.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Template {
    pub id: String,
    pub name: String,
    pub description: String,
    pub channel_type: ChannelType,
    pub config: ChannelConfig,
    pub icon: Option<String>,
}

#[cfg(test)]
mod send_block_tests {
    use super::*;

    #[test]
    fn send_block_follows_own_membership() {
        let mut me = Membership {
            channel_id: ChannelId::new(),
            user_id: UserId::new(),
            role: MemberRole::Member,
            joined_at: Utc::now(),
            last_read_message_id: None,
            is_muted: false,
            muted_until: None,
            notification_preference: NotificationPreference::All,
        };
        assert_eq!(SendBlock::for_viewer(Some(&me), true, None), None);
        me.is_muted = true;
        assert_eq!(SendBlock::for_viewer(Some(&me), true, None), Some(SendBlock::Muted { until: None }));
        assert_eq!(SendBlock::for_viewer(None, true, None), Some(SendBlock::Removed));
        // Roster never synced: not evidence of removal.
        assert_eq!(SendBlock::for_viewer(None, false, None), None);
        // A former channel is read-only even with no roster stored; a rejoin
        // (our membership back) lifts it.
        assert_eq!(SendBlock::for_viewer(None, false, Some(false)), Some(SendBlock::Removed));
        assert_eq!(SendBlock::for_viewer(None, true, Some(true)), Some(SendBlock::Deleted));
        me.is_muted = false;
        assert_eq!(SendBlock::for_viewer(Some(&me), true, Some(true)), None);
    }

    #[test]
    fn only_non_owner_members_may_leave() {
        let (owner, member) = (UserId::new(), UserId::new());
        let ch = Channel::new(ChannelId::new(), "invtest".into(), ChannelType::Private, owner);
        assert!(ch.can_user_leave(&member, true));
        assert!(!ch.can_user_leave(&owner, true));
        assert!(!ch.can_user_leave(&member, false));
        let dm = Channel::new(ChannelId::new(), String::new(), ChannelType::DirectMessage, owner);
        assert!(!dm.can_user_leave(&member, true));
    }

    #[test]
    fn mute_notice_names_the_end_time() {
        use chrono::TimeZone;
        let now = Utc.with_ymd_and_hms(2026, 9, 25, 12, 0, 0).unwrap();
        let at = |d, h, m| SendBlock::Muted { until: Some(Utc.with_ymd_and_hms(2026, 9, d, h, m, 0).unwrap()) }.notice_at(now);
        let base = "You can't send messages in this channel while muted";
        assert_eq!(SendBlock::Muted { until: None }.notice_at(now), base);
        assert_eq!(at(25, 15, 30), format!("{base} until 15:30"));
        assert_eq!(at(26, 9, 0), format!("{base} until tomorrow 09:00"));
        assert_eq!(at(28, 9, 0), format!("{base} until Sep 28, 09:00"));
    }
}

#[cfg(test)]
mod moderation_tests {
    use super::{MemberModeration, MemberRole::*};

    #[test]
    fn moderation_mirrors_server_role_rules() {
        let m = |actor, is_self, target| MemberModeration::for_target(actor, is_self, target);
        // Owner acts on anyone but themselves; nobody acts on the owner.
        assert_eq!(m(Some(Owner), false, Admin), MemberModeration { can_ban: true, can_mute: true, can_make_owner: true });
        assert_eq!(m(Some(Owner), false, Member), MemberModeration { can_ban: true, can_mute: true, can_make_owner: true });
        assert_eq!(m(Some(Owner), true, Owner), MemberModeration::default());
        assert_eq!(m(Some(Admin), false, Owner), MemberModeration::default());
        // Admin outranks moderator/member; peers do not act on each other.
        // Only the owner hands over ownership: an admin who may ban may not.
        assert_eq!(m(Some(Admin), false, Moderator), MemberModeration { can_ban: true, can_mute: true, can_make_owner: false });
        assert_eq!(m(Some(Admin), false, Admin), MemberModeration::default());
        // Moderator may mute a member but never ban.
        assert_eq!(m(Some(Moderator), false, Member), MemberModeration { can_ban: false, can_mute: true, can_make_owner: false });
        assert_eq!(m(Some(Moderator), false, Moderator), MemberModeration::default());
        // Members and non-members get nothing.
        assert_eq!(m(Some(Member), false, Member), MemberModeration::default());
        assert_eq!(m(None, false, Member), MemberModeration::default());
    }
}
