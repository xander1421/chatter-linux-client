//! Anonymous DM domain types.
//!
//! Client-side managed direct messages where the server has no knowledge of
//! the conversation. Used for public accounts where strangers can message
//! without friend requests.
//!
//! Similar to anonymous groups (NDSS'21 style) but for 1:1 conversations.

use chrono::{DateTime, Utc};

use super::UserId;
use super::anon_primitives::{AnonId, AnonMessageId};

/// Phantom marker for DM types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DmMarker;

/// Unique identifier for an anonymous DM conversation.
pub type AnonDmId = AnonId<DmMarker>;

/// Unique identifier for an anonymous DM message.
pub type AnonDmMessageId = AnonMessageId<DmMarker>;

/// Status of an anonymous DM conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(i32)]
pub enum AnonDmStatus {
    /// Message request pending (not yet accepted by recipient).
    #[default]
    Pending = 0,
    /// Conversation accepted and active.
    Active = 1,
    /// Blocked by us.
    Blocked = 2,
    /// Declined/rejected.
    Declined = 3,
}

impl AnonDmStatus {
    /// Create from integer value.
    pub fn from_i32(value: i32) -> Self {
        match value {
            1 => Self::Active,
            2 => Self::Blocked,
            3 => Self::Declined,
            _ => Self::Pending,
        }
    }

    /// Convert to integer value.
    pub fn to_i32(self) -> i32 {
        self as i32
    }

    /// Check if messages can be sent.
    pub fn can_send(&self) -> bool {
        matches!(self, Self::Active)
    }

    /// Check if this is a pending request.
    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Pending)
    }
}

impl From<i32> for AnonDmStatus {
    fn from(value: i32) -> Self {
        Self::from_i32(value)
    }
}

/// An anonymous DM conversation (local state only - server has no knowledge).
#[derive(Debug, Clone)]
pub struct AnonDm {
    /// Unique conversation identifier.
    pub id: AnonDmId,
    /// The other participant's user ID.
    pub peer_id: UserId,
    /// Whether we initiated this conversation.
    pub initiated_by_us: bool,
    /// Current status of the conversation.
    pub status: AnonDmStatus,
    /// Peer's mailbox public key for this conversation (32 bytes Ed25519).
    /// None if we initiated and haven't received acknowledgment yet.
    pub peer_mailbox_pk: Option<[u8; 32]>,
    /// Peer's profile key for decrypting their display name.
    pub peer_profile_key: Option<[u8; 32]>,
    /// Peer's display name (revealed upon acceptance or received in request).
    pub peer_display_name: Option<String>,
    /// Peer's avatar (decrypted, stored locally).
    pub peer_avatar: Option<Vec<u8>>,
    /// When the conversation was created.
    pub created_at: DateTime<Utc>,
    /// When the conversation was last updated.
    pub updated_at: DateTime<Utc>,
    /// When the last message was sent/received.
    pub last_message_at: Option<DateTime<Utc>>,
    /// Unread message count.
    pub unread_count: u32,
    /// The ONE peer device this secret chat is bound to: the device our
    /// request was sealed to, or the device whose request we received. `None`
    /// until known. Pairwise encryption targets only this device; if it
    /// disappears the chat is refused, never moved or fanned out.
    pub peer_device_id: Option<u32>,
    /// For a chat we initiated: whether our request may have left this
    /// device. `false` means it provably never did (failed before the send
    /// RPC), so starting again is safe; `true` keeps uncertain delivery as is.
    pub request_submitted: bool,
}

impl AnonDm {
    /// Create a new conversation that we initiated.
    pub fn new_initiated(peer_id: UserId) -> Self {
        let now = Utc::now();
        Self {
            id: AnonDmId::new(),
            peer_id,
            initiated_by_us: true,
            status: AnonDmStatus::Pending,
            peer_mailbox_pk: None,
            peer_profile_key: None,
            peer_display_name: None,
            peer_avatar: None,
            created_at: now,
            updated_at: now,
            last_message_at: None,
            unread_count: 0,
            peer_device_id: None,
            request_submitted: false,
        }
    }

    /// Create a conversation from an incoming message request.
    pub fn from_request(id: AnonDmId, peer_id: UserId, peer_mailbox_pk: [u8; 32]) -> Self {
        let now = Utc::now();
        Self {
            id,
            peer_id,
            initiated_by_us: false,
            status: AnonDmStatus::Pending,
            peer_mailbox_pk: Some(peer_mailbox_pk),
            peer_profile_key: None,
            peer_display_name: None,
            peer_avatar: None,
            created_at: now,
            updated_at: now,
            last_message_at: None,
            unread_count: 1,
            peer_device_id: None,
            request_submitted: false,
        }
    }

    /// Set the peer's display name.
    pub fn with_display_name(mut self, name: impl Into<String>) -> Self {
        let name = name.into();
        self.peer_display_name = if name.is_empty() { None } else { Some(name) };
        self
    }

    /// Set the peer's avatar.
    pub fn with_avatar(mut self, avatar: Vec<u8>) -> Self {
        self.peer_avatar = if avatar.is_empty() {
            None
        } else {
            Some(avatar)
        };
        self
    }

    /// Set the peer's avatar (optional version).
    pub fn with_avatar_opt(mut self, avatar: Option<Vec<u8>>) -> Self {
        self.peer_avatar = avatar;
        self
    }

    /// Check if conversation is ready for messaging.
    pub fn is_ready(&self) -> bool {
        self.status == AnonDmStatus::Active && self.peer_mailbox_pk.is_some()
    }

    /// Check if this is an incoming request we haven't responded to.
    pub fn is_incoming_request(&self) -> bool {
        !self.initiated_by_us && self.status == AnonDmStatus::Pending
    }

    /// Our request that provably never left this device (it failed before
    /// the send RPC). Safe to discard and start again.
    pub fn is_unsent_request(&self) -> bool {
        self.is_outgoing_request() && !self.request_submitted
    }

    /// Check if this is an outgoing request that hasn't been accepted.
    pub fn is_outgoing_request(&self) -> bool {
        self.initiated_by_us && self.status == AnonDmStatus::Pending
    }
}

/// Our mailbox for a specific anonymous DM conversation.
#[derive(Debug, Clone)]
pub struct AnonDmMailbox {
    /// Unique row ID.
    pub id: String,
    /// The conversation this mailbox is for.
    pub conversation_id: AnonDmId,
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

impl AnonDmMailbox {
    /// Check if this mailbox has expired.
    pub fn is_expired(&self) -> bool {
        Utc::now() > self.expires_at
    }
}

use crate::domain::message::ReactionCount;

/// A message in an anonymous DM conversation.
#[derive(Debug, Clone)]
pub struct AnonDmMessage {
    /// Unique message identifier.
    pub id: AnonDmMessageId,
    /// Conversation this message belongs to.
    pub conversation_id: AnonDmId,
    /// User who sent this message.
    pub sender_id: UserId,
    /// Content type discriminant (1=text, 2=attachment, etc.).
    pub content_type: i32,
    /// JSON-serialized message content.
    pub content_json: String,
    /// Serialized attachments.
    pub attachments_blob: Vec<u8>,
    /// Whether this message has been deleted.
    pub is_deleted: bool,
    /// Whether we've read this message.
    pub is_read: bool,
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
    /// Whether this message has been edited.
    pub is_edited: bool,
}

impl AnonDmMessage {
    /// Create a new text message.
    ///
    /// The `sent_at` parameter is the sender's timestamp (nanoseconds since epoch),
    /// used for consistent ordering across devices.
    pub fn new_text(
        conversation_id: AnonDmId,
        sender_id: UserId,
        text: &str,
        sent_at: u64,
    ) -> Self {
        Self {
            id: AnonDmMessageId::new(),
            conversation_id,
            sender_id,
            content_type: 1, // Text
            content_json: serde_json::json!({ "text": text }).to_string(),
            attachments_blob: vec![8, 0, 0, 0, 0, 0, 0, 0], // Empty vec bincode
            is_deleted: false,
            is_read: false,
            reaction_counts: Vec::new(),
            current_user_reactions: Vec::new(),
            delivery_status: String::new(),
            sent_at,
            created_at: Utc::now(),
            is_pinned: false,
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

        if self.attachments_blob.is_empty() || self.attachments_blob == vec![8, 0, 0, 0, 0, 0, 0, 0]
        {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_anon_dm_id_new() {
        let id1 = AnonDmId::new();
        let id2 = AnonDmId::new();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_anon_dm_id_from_bytes() {
        let bytes = [1u8; 16];
        let id = AnonDmId::from_bytes(bytes);
        assert_eq!(id.as_bytes(), &bytes);
    }

    #[test]
    fn test_anon_dm_id_roundtrip() {
        let id = AnonDmId::new();
        let s = id.to_string();
        let parsed: AnonDmId = s.parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn test_anon_dm_status_from_i32() {
        assert_eq!(AnonDmStatus::from_i32(0), AnonDmStatus::Pending);
        assert_eq!(AnonDmStatus::from_i32(1), AnonDmStatus::Active);
        assert_eq!(AnonDmStatus::from_i32(2), AnonDmStatus::Blocked);
        assert_eq!(AnonDmStatus::from_i32(3), AnonDmStatus::Declined);
        assert_eq!(AnonDmStatus::from_i32(99), AnonDmStatus::Pending);
    }

    #[test]
    fn test_anon_dm_status_can_send() {
        assert!(!AnonDmStatus::Pending.can_send());
        assert!(AnonDmStatus::Active.can_send());
        assert!(!AnonDmStatus::Blocked.can_send());
        assert!(!AnonDmStatus::Declined.can_send());
    }

    #[test]
    fn test_anon_dm_new_initiated() {
        let peer_id = UserId::new();
        let dm = AnonDm::new_initiated(peer_id);
        assert!(dm.initiated_by_us);
        assert_eq!(dm.status, AnonDmStatus::Pending);
        assert!(dm.peer_mailbox_pk.is_none());
        assert!(dm.is_outgoing_request());
        assert!(!dm.is_incoming_request());
        assert!(!dm.is_ready());
    }

    #[test]
    fn test_anon_dm_from_request() {
        let peer_id = UserId::new();
        let mailbox_pk = [42u8; 32];
        let dm = AnonDm::from_request(AnonDmId::new(), peer_id, mailbox_pk);
        assert!(!dm.initiated_by_us);
        assert_eq!(dm.status, AnonDmStatus::Pending);
        assert_eq!(dm.peer_mailbox_pk, Some(mailbox_pk));
        assert!(dm.is_incoming_request());
        assert!(!dm.is_outgoing_request());
        assert!(!dm.is_ready());
    }

    #[test]
    fn test_anon_dm_message_text() {
        let sent_at = 1704067200_000_000_000u64; // 2024-01-01 00:00:00 UTC
        let msg = AnonDmMessage::new_text(AnonDmId::new(), UserId::new(), "Hello", sent_at);
        assert_eq!(msg.content_type, 1);
        assert_eq!(msg.as_text(), Some("Hello".to_string()));
        assert_eq!(msg.sent_at, sent_at);
    }
}
