//! Anonymous group domain types.
//!
//! NDSS'21 style groups where the server has no knowledge of group existence,
//! membership, or messages. All operations are via pairwise sealed sender.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::UserId;
use super::anon_primitives::{AnonId, AnonMessageId};

/// Phantom marker for Group types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GroupMarker;

/// Unique identifier for an anonymous group.
pub type AnonGroupId = AnonId<GroupMarker>;

/// Unique identifier for an anonymous group message.
pub type AnonGroupMessageId = AnonMessageId<GroupMarker>;

/// Status of an anonymous group (local view).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(i32)]
pub enum AnonGroupStatus {
    /// Group is active and can send/receive messages.
    #[default]
    Active = 0,
    /// Group is muted (receive messages but no notifications).
    Muted = 1,
    /// Group is blocked (ignore all messages, don't poll mailbox).
    Blocked = 2,
}

impl AnonGroupStatus {
    /// Create from integer value.
    pub fn from_i32(value: i32) -> Self {
        match value {
            1 => Self::Muted,
            2 => Self::Blocked,
            _ => Self::Active,
        }
    }

    /// Convert to integer value.
    pub fn to_i32(self) -> i32 {
        self as i32
    }

    /// Check if messages can be sent.
    pub fn can_send(&self) -> bool {
        matches!(self, Self::Active | Self::Muted)
    }

    /// Check if messages should be received.
    pub fn can_receive(&self) -> bool {
        matches!(self, Self::Active | Self::Muted)
    }

    /// Check if notifications should be shown.
    pub fn show_notifications(&self) -> bool {
        matches!(self, Self::Active)
    }

    /// Check if this group is blocked.
    pub fn is_blocked(&self) -> bool {
        matches!(self, Self::Blocked)
    }
}

impl From<i32> for AnonGroupStatus {
    fn from(value: i32) -> Self {
        Self::from_i32(value)
    }
}

/// Role within an anonymous group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(i32)]
pub enum AnonGroupRole {
    /// Regular group member.
    #[default]
    Member = 0,
    /// Administrator - can kick members and change roles.
    Admin = 1,
    /// Group owner - full control, cannot be kicked.
    Owner = 2,
}

impl AnonGroupRole {
    /// Create from integer value.
    pub fn from_i32(value: i32) -> Self {
        match value {
            1 => Self::Admin,
            2 => Self::Owner,
            _ => Self::Member,
        }
    }

    /// Convert to integer value.
    pub fn to_i32(self) -> i32 {
        self as i32
    }

    /// UI/string token for this role. Single source of truth so the UI never
    /// re-maps the enum.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Member => "member",
            Self::Admin => "admin",
            Self::Owner => "owner",
        }
    }

    /// Check if this role can kick members.
    pub fn can_kick(&self) -> bool {
        matches!(self, Self::Admin | Self::Owner)
    }

    /// Check if this role can change member roles.
    pub fn can_change_roles(&self) -> bool {
        matches!(self, Self::Admin | Self::Owner)
    }

    /// Check if this role can rename the group.
    pub fn can_rename(&self) -> bool {
        matches!(self, Self::Admin | Self::Owner)
    }

    /// Check if this role can invite new members.
    pub fn can_invite(&self) -> bool {
        matches!(self, Self::Admin | Self::Owner)
    }

    /// Check if this role can broadcast member lists.
    ///
    /// Only admins and owners can send authoritative member list updates.
    /// This prevents regular members from spoofing other members' profiles.
    pub fn can_broadcast_member_list(&self) -> bool {
        matches!(self, Self::Admin | Self::Owner)
    }

    /// Check if this role can pin or unpin messages.
    ///
    /// Only admins and owners can pin messages for everyone in the group.
    pub fn can_pin(&self) -> bool {
        matches!(self, Self::Admin | Self::Owner)
    }
}

impl From<i32> for AnonGroupRole {
    fn from(value: i32) -> Self {
        Self::from_i32(value)
    }
}

/// An anonymous group (local state only - server has no knowledge).
#[derive(Debug, Clone)]
pub struct AnonGroup {
    /// Unique group identifier.
    pub id: AnonGroupId,
    /// Human-readable group name.
    pub name: String,
    /// The group owner's user ID. Authoritative for member list and roles.
    pub owner_id: UserId,
    /// Our role in the group.
    pub my_role: AnonGroupRole,
    /// Group status (active/muted/blocked).
    pub status: AnonGroupStatus,
    /// Whether bootstrap is complete (all initial members confirmed).
    pub bootstrap_complete: bool,
    /// When the group was created.
    pub created_at: DateTime<Utc>,
    /// When the group was last updated.
    pub updated_at: DateTime<Utc>,
}

impl AnonGroup {
    /// Create a new group as owner.
    pub fn new_as_owner(name: String, owner_id: UserId) -> Self {
        let now = Utc::now();
        Self {
            id: AnonGroupId::new(),
            name,
            owner_id,
            my_role: AnonGroupRole::Owner,
            status: AnonGroupStatus::Active,
            bootstrap_complete: false,
            created_at: now,
            updated_at: now,
        }
    }

    /// Create a group from an invite.
    pub fn from_invite(id: AnonGroupId, name: String, owner_id: UserId) -> Self {
        let now = Utc::now();
        Self {
            id,
            name,
            owner_id,
            my_role: AnonGroupRole::Member,
            status: AnonGroupStatus::Active,
            bootstrap_complete: false,
            created_at: now,
            updated_at: now,
        }
    }

    /// Check if we are the owner of this group.
    pub fn is_owner(&self) -> bool {
        self.my_role == AnonGroupRole::Owner
    }

    /// Check if we have admin privileges.
    pub fn is_admin_or_owner(&self) -> bool {
        matches!(self.my_role, AnonGroupRole::Admin | AnonGroupRole::Owner)
    }

    /// Check if the given user is the owner of this group.
    ///
    /// True when either `owner_id` matches `user_id`, or `my_role` is `Owner`
    /// (covers the case where `owner_id` is stale but the local role is current).
    pub fn is_current_user_owner(&self, user_id: &UserId) -> bool {
        self.owner_id == *user_id || self.my_role == AnonGroupRole::Owner
    }

    /// Check if the given user has the admin role in this group.
    ///
    /// Note: this checks for the `Admin` role specifically (not owner). For
    /// "admin or owner" semantics, combine with [`Self::is_current_user_owner`].
    pub fn is_current_user_admin(&self, _user_id: &UserId) -> bool {
        self.my_role == AnonGroupRole::Admin
    }

    /// Check if the group is ready for messaging.
    pub fn is_ready(&self) -> bool {
        self.status.can_send() && self.bootstrap_complete
    }

    /// Check if the group is blocked.
    pub fn is_blocked(&self) -> bool {
        self.status.is_blocked()
    }

    /// Check if the group is muted.
    pub fn is_muted(&self) -> bool {
        matches!(self.status, AnonGroupStatus::Muted)
    }
}

/// A member of an anonymous group.
#[derive(Debug, Clone)]
pub struct AnonGroupMember {
    /// Unique row ID.
    pub id: String,
    /// The group this member belongs to.
    pub group_id: AnonGroupId,
    /// The member's user ID.
    pub user_id: UserId,
    /// The member's role in the group.
    pub role: AnonGroupRole,
    /// The member's mailbox public key for this group (32 bytes Ed25519).
    /// None if the member is pending (invite sent but no JoinAck received).
    pub mailbox_pk: Option<[u8; 32]>,
    /// Member's profile key for decrypting their display name.
    pub profile_key: Option<[u8; 32]>,
    /// Member's display name (decrypted).
    pub display_name: Option<String>,
    /// Member's avatar (decrypted, stored locally).
    pub avatar: Option<Vec<u8>>,
    /// When the member joined.
    pub joined_at: DateTime<Utc>,
}

impl AnonGroupMember {
    /// Create a new member entry.
    pub fn new(group_id: AnonGroupId, user_id: UserId, role: AnonGroupRole) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            group_id,
            user_id,
            role,
            mailbox_pk: None,
            profile_key: None,
            display_name: None,
            avatar: None,
            joined_at: Utc::now(),
        }
    }

    /// Create a member entry with a known mailbox key.
    pub fn with_mailbox(
        group_id: AnonGroupId,
        user_id: UserId,
        role: AnonGroupRole,
        mailbox_pk: [u8; 32],
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            group_id,
            user_id,
            role,
            mailbox_pk: Some(mailbox_pk),
            profile_key: None,
            display_name: None,
            avatar: None,
            joined_at: Utc::now(),
        }
    }

    /// Set the member's profile information.
    pub fn with_profile(mut self, profile_key: Option<[u8; 32]>, display_name: Option<String>, avatar: Option<Vec<u8>>) -> Self {
        self.profile_key = profile_key;
        self.display_name = display_name;
        self.avatar = avatar;
        self
    }

    /// Check if this member is confirmed (has mailbox key).
    pub fn is_confirmed(&self) -> bool {
        self.mailbox_pk.is_some()
    }

    /// Check if this member is pending (no mailbox key yet).
    pub fn is_pending(&self) -> bool {
        self.mailbox_pk.is_none()
    }
}

/// Our mailbox for a specific anonymous group.
#[derive(Debug, Clone)]
pub struct AnonGroupMailbox {
    /// Unique row ID.
    pub id: String,
    /// The group this mailbox is for.
    pub group_id: AnonGroupId,
    /// Our public key for this mailbox (32 bytes Ed25519).
    pub public_key: [u8; 32],
    /// Our private key for this mailbox (64 bytes Ed25519 keypair).
    pub private_key: [u8; 64],
    /// The blind signature token ID used to register this mailbox.
    pub token_id: String,
    /// When this mailbox was created.
    pub created_at: DateTime<Utc>,
    /// When this mailbox expires.
    pub expires_at: DateTime<Utc>,
}

impl AnonGroupMailbox {
    /// Check if this mailbox has expired.
    pub fn is_expired(&self) -> bool {
        Utc::now() > self.expires_at
    }
}

/// Type of pending action for a group member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PendingActionType {
    /// Invite sent, waiting for JoinAck.
    Invite,
    /// Kick sent, waiting for confirmation.
    Kick,
}

impl PendingActionType {
    /// Convert to string for storage.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Invite => "invite",
            Self::Kick => "kick",
        }
    }

    /// Parse from string.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "invite" => Some(Self::Invite),
            "kick" => Some(Self::Kick),
            _ => None,
        }
    }
}

/// A pending action in an anonymous group.
#[derive(Debug, Clone)]
pub struct AnonGroupPending {
    /// Unique row ID.
    pub id: String,
    /// The group this action is for.
    pub group_id: AnonGroupId,
    /// The user this action targets.
    pub user_id: UserId,
    /// The type of pending action.
    pub action: PendingActionType,
    /// When this action was created.
    pub created_at: DateTime<Utc>,
    /// When this action expires.
    pub expires_at: DateTime<Utc>,
    /// Number of retry attempts.
    pub retry_count: u32,
}

impl AnonGroupPending {
    /// Create a new pending invite.
    pub fn new_invite(group_id: AnonGroupId, user_id: UserId) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4().to_string(),
            group_id,
            user_id,
            action: PendingActionType::Invite,
            created_at: now,
            expires_at: now + chrono::Duration::days(7), // 7 day expiry
            retry_count: 0,
        }
    }

    /// Create a new pending kick.
    pub fn new_kick(group_id: AnonGroupId, user_id: UserId) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4().to_string(),
            group_id,
            user_id,
            action: PendingActionType::Kick,
            created_at: now,
            expires_at: now + chrono::Duration::hours(24), // 24 hour expiry
            retry_count: 0,
        }
    }

    /// Check if this pending action has expired.
    pub fn is_expired(&self) -> bool {
        Utc::now() > self.expires_at
    }
}

// ============================================================================
// AnonGroupMessage
// ============================================================================

use crate::domain::message::ReactionCount;

/// A message in an anonymous group.
#[derive(Debug, Clone)]
pub struct AnonGroupMessage {
    /// Unique message identifier.
    pub id: AnonGroupMessageId,
    /// Group this message belongs to.
    pub group_id: AnonGroupId,
    /// User who sent this message.
    pub sender_id: UserId,
    /// Content type discriminant.
    pub content_type: i32,
    /// JSON-serialized message content.
    pub content_json: String,
    /// Serialized attachments.
    pub attachments_blob: Vec<u8>,
    /// Whether this message has been deleted.
    pub is_deleted: bool,
    /// Reaction counts by emoji.
    pub reaction_counts: Vec<ReactionCount>,
    /// Current user's reactions (for UI state).
    pub current_user_reactions: Vec<String>,
    /// Delivery status for own messages: "sent", "delivered", "read", or "" for non-own.
    pub delivery_status: String,
    /// Sender's timestamp in nanoseconds (used for ordering).
    /// This is the authoritative timestamp from Content.timestamp.
    pub sent_at: u64,
    /// When this message was stored locally.
    pub created_at: DateTime<Utc>,
    /// Whether this message is pinned.
    pub is_pinned: bool,
    /// Whether we've read this message.
    pub is_read: bool,
    /// Whether this message has been edited.
    pub is_edited: bool,
}

impl AnonGroupMessage {
    /// Create a new text message.
    ///
    /// The `sent_at` parameter is the sender's timestamp (nanoseconds since epoch),
    /// used for consistent ordering across devices.
    pub fn new_text(
        group_id: AnonGroupId,
        sender_id: UserId,
        text: &str,
        sent_at: u64,
    ) -> Self {
        Self {
            id: AnonGroupMessageId::new(),
            group_id,
            sender_id,
            content_type: 1, // Text
            content_json: serde_json::json!({ "text": text }).to_string(),
            attachments_blob: vec![8, 0, 0, 0, 0, 0, 0, 0], // Empty vec bincode
            is_deleted: false,
            reaction_counts: Vec::new(),
            current_user_reactions: Vec::new(),
            delivery_status: String::new(),
            sent_at,
            created_at: Utc::now(),
            is_pinned: false,
            is_read: false,
            is_edited: false,
        }
    }

    /// Get the text content if this is a text message.
    pub fn as_text(&self) -> Option<String> {
        if self.content_type == 1 || self.content_type == 3 {
            serde_json::from_str::<serde_json::Value>(&self.content_json)
                .ok()
                .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(String::from))
        } else {
            None
        }
    }

    /// Get attachment IDs from the serialized blob.
    ///
    /// Returns attachment IDs stored with the message, deserialized from bincode.
    pub fn attachment_ids(&self) -> Vec<crate::domain::AttachmentId> {
        use crate::storage::serialization::db_deserialize;

        if self.attachments_blob.is_empty() || self.attachments_blob == vec![8, 0, 0, 0, 0, 0, 0, 0] {
            return Vec::new();
        }

        db_deserialize(&self.attachments_blob).unwrap_or_default()
    }

    /// Check if this message has attachments.
    pub fn has_attachments(&self) -> bool {
        // content_type 2 = attachment only, 3 = mixed (text + attachments)
        self.content_type == 2 || self.content_type == 3
    }
}

// ============================================================================
// Blocked User
// ============================================================================

/// A blocked user within an anonymous group.
#[derive(Debug, Clone)]
pub struct AnonGroupBlockedUser {
    /// Unique row ID.
    pub id: String,
    /// The group this block is for.
    pub group_id: AnonGroupId,
    /// The blocked user's ID.
    pub user_id: UserId,
    /// When the user was blocked.
    pub blocked_at: DateTime<Utc>,
    /// Optional reason for the block.
    pub reason: Option<String>,
}

impl AnonGroupBlockedUser {
    /// Create a new blocked user entry.
    pub fn new(group_id: AnonGroupId, user_id: UserId, reason: Option<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            group_id,
            user_id,
            blocked_at: Utc::now(),
            reason,
        }
    }
}

// ============================================================================
// Rate Limit Entry
// ============================================================================

// AnonRateLimitEntry is now in anon_primitives.rs
pub use super::anon_primitives::AnonRateLimitEntry;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_anon_group_id_new() {
        let id1 = AnonGroupId::new();
        let id2 = AnonGroupId::new();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_anon_group_id_from_bytes() {
        let bytes = [1u8; 16];
        let id = AnonGroupId::from_bytes(bytes);
        assert_eq!(id.as_bytes(), &bytes);
    }

    #[test]
    fn test_anon_group_id_roundtrip() {
        let id = AnonGroupId::new();
        let s = id.to_string();
        let parsed: AnonGroupId = s.parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn test_anon_group_role_from_i32() {
        assert_eq!(AnonGroupRole::from_i32(0), AnonGroupRole::Member);
        assert_eq!(AnonGroupRole::from_i32(1), AnonGroupRole::Admin);
        assert_eq!(AnonGroupRole::from_i32(2), AnonGroupRole::Owner);
        assert_eq!(AnonGroupRole::from_i32(99), AnonGroupRole::Member);
    }

    #[test]
    fn test_anon_group_role_permissions() {
        assert!(!AnonGroupRole::Member.can_kick());
        assert!(AnonGroupRole::Admin.can_kick());
        assert!(AnonGroupRole::Owner.can_kick());
        // Only admins and owners can invite
        assert!(!AnonGroupRole::Member.can_invite());
        assert!(AnonGroupRole::Admin.can_invite());
        assert!(AnonGroupRole::Owner.can_invite());
    }

    #[test]
    fn test_anon_group_new_as_owner() {
        let group = AnonGroup::new_as_owner("Test Group".to_string(), UserId::new());
        assert_eq!(group.name, "Test Group");
        assert_eq!(group.my_role, AnonGroupRole::Owner);
        assert!(!group.bootstrap_complete);
        assert!(group.is_owner());
        assert!(group.is_admin_or_owner());
    }

    #[test]
    fn test_anon_group_from_invite() {
        let id = AnonGroupId::new();
        let group = AnonGroup::from_invite(id, "Invited Group".to_string(), UserId::new());
        assert_eq!(group.id, id);
        assert_eq!(group.my_role, AnonGroupRole::Member);
        assert!(!group.is_owner());
        assert!(!group.is_admin_or_owner());
    }

    #[test]
    fn test_anon_group_member_pending() {
        let member = AnonGroupMember::new(
            AnonGroupId::new(),
            UserId::new(),
            AnonGroupRole::Member,
        );
        assert!(member.is_pending());
        assert!(!member.is_confirmed());
    }

    #[test]
    fn test_anon_group_member_with_mailbox() {
        let pk = [42u8; 32];
        let member = AnonGroupMember::with_mailbox(
            AnonGroupId::new(),
            UserId::new(),
            AnonGroupRole::Member,
            pk,
        );
        assert!(!member.is_pending());
        assert!(member.is_confirmed());
        assert_eq!(member.mailbox_pk, Some(pk));
    }

    #[test]
    fn test_pending_action_type() {
        assert_eq!(PendingActionType::Invite.as_str(), "invite");
        assert_eq!(PendingActionType::Kick.as_str(), "kick");
        assert_eq!(PendingActionType::parse("invite"), Some(PendingActionType::Invite));
        assert_eq!(PendingActionType::parse("kick"), Some(PendingActionType::Kick));
        assert_eq!(PendingActionType::parse("unknown"), None);
    }

    #[test]
    fn test_anon_group_status_from_i32() {
        assert_eq!(AnonGroupStatus::from_i32(0), AnonGroupStatus::Active);
        assert_eq!(AnonGroupStatus::from_i32(1), AnonGroupStatus::Muted);
        assert_eq!(AnonGroupStatus::from_i32(2), AnonGroupStatus::Blocked);
        assert_eq!(AnonGroupStatus::from_i32(99), AnonGroupStatus::Active);
    }

    #[test]
    fn test_anon_group_status_can_send() {
        assert!(AnonGroupStatus::Active.can_send());
        assert!(AnonGroupStatus::Muted.can_send());
        assert!(!AnonGroupStatus::Blocked.can_send());
    }

    #[test]
    fn test_anon_group_status_can_receive() {
        assert!(AnonGroupStatus::Active.can_receive());
        assert!(AnonGroupStatus::Muted.can_receive());
        assert!(!AnonGroupStatus::Blocked.can_receive());
    }

    #[test]
    fn test_anon_group_status_notifications() {
        assert!(AnonGroupStatus::Active.show_notifications());
        assert!(!AnonGroupStatus::Muted.show_notifications());
        assert!(!AnonGroupStatus::Blocked.show_notifications());
    }

    #[test]
    fn test_anon_group_is_blocked() {
        let mut group = AnonGroup::new_as_owner("Test".to_string(), UserId::new());
        assert!(!group.is_blocked());
        group.status = AnonGroupStatus::Blocked;
        assert!(group.is_blocked());
    }

    #[test]
    fn test_anon_group_is_muted() {
        let mut group = AnonGroup::new_as_owner("Test".to_string(), UserId::new());
        assert!(!group.is_muted());
        group.status = AnonGroupStatus::Muted;
        assert!(group.is_muted());
    }

    #[test]
    fn test_anon_group_is_ready() {
        let mut group = AnonGroup::new_as_owner("Test".to_string(), UserId::new());
        // Not ready - bootstrap not complete
        assert!(!group.is_ready());

        group.bootstrap_complete = true;
        // Ready - active and bootstrapped
        assert!(group.is_ready());

        group.status = AnonGroupStatus::Muted;
        // Still ready - muted can still send
        assert!(group.is_ready());

        group.status = AnonGroupStatus::Blocked;
        // Not ready - blocked
        assert!(!group.is_ready());
    }

    #[test]
    fn test_anon_group_blocked_user() {
        let blocked = AnonGroupBlockedUser::new(
            AnonGroupId::new(),
            UserId::new(),
            Some("spam".to_string()),
        );
        assert!(blocked.reason.is_some());
        assert_eq!(blocked.reason.unwrap(), "spam");
    }

    #[test]
    fn test_anon_rate_limit_entry() {
        let entry = AnonRateLimitEntry::new("group", vec![1, 2, 3]);
        assert_eq!(entry.entity_type, "group");
        assert_eq!(entry.entity_id, vec![1, 2, 3]);
        assert_eq!(entry.message_count, 1);
    }
}
