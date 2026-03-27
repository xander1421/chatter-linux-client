//! Type-specific message handlers.
//!
//! Handlers for different inbox message types. Each handler processes a specific
//! content type from decrypted inbox messages and emits domain events.
//!
//! # Architecture
//!
//! - `InboxHandler` trait: Common interface for all handlers
//! - `HandlerContext`: Shared context passed to handlers (user ID, event channel)
//! - `DomainEvent`: Events emitted by handlers for UI/storage consumption
//! - `HandlerOutcome`: Result of handler processing (processed, ignored, deferred)
//!
//! # Flow
//!
//! 1. `MessageProcessor` decrypts and determines `ContentType`
//! 2. Router selects appropriate handler based on content type
//! 3. Handler parses plaintext, validates, emits `DomainEvent`
//! 4. Events flow to UI/storage via broadcast channel

pub mod anon_dm;
pub mod anon_group;
mod base_anon;
pub mod attachment;
pub mod chat;
pub mod key_exchange;
pub mod pin;
pub mod reaction;
pub mod receipt;
pub mod sender_key;
pub mod sync;
pub mod typing;

// NOTE: proactive_dm module removed - DM bootstrap is done on-demand during first message send.
// This avoids race conditions where both sides try to initiate simultaneously.

use std::fmt;

use tokio::sync::broadcast;

use crate::domain::{Attachment, ChannelId, Message, MessageContent, MessageId, UserId};
use crate::domain::anon_dm::{AnonDmId, AnonDmMessageId};
use crate::domain::anon_group::{AnonGroupId, AnonGroupMessageId};
use crate::inbox::events::FriendRequestInfo;
use crate::inbox::processor::{ContentType, ProcessedMessage};
use crate::proto::content_v1::Content;

use std::sync::Arc;

// Re-export handlers
pub use anon_dm::AnonDmHandler;
pub use anon_group::AnonGroupHandler;
pub use attachment::AttachmentHandler;
pub use chat::ChatHandler;
pub use key_exchange::KeyExchangeHandler;
pub use pin::PinHandler;
pub use reaction::ReactionHandler;
pub use receipt::ReceiptHandler;
pub use sender_key::SenderKeyHandler;
pub use sync::SyncHandler;
pub use typing::TypingHandler;

// ============================================================================
// InboxHandler Trait
// ============================================================================

/// Handler for processing specific inbox message types.
///
/// Handlers are stateless - all context is passed via `HandlerContext`.
/// Each handler processes one or more `ContentType` variants.
#[async_trait::async_trait]
pub trait InboxHandler: Send + Sync {
    /// Handle a processed message, returning domain events.
    ///
    /// # Arguments
    ///
    /// * `message` - The decrypted message with content type and plaintext
    /// * `ctx` - Handler context with user ID and event channel
    ///
    /// # Returns
    ///
    /// `HandlerOutcome` indicating success, ignore, or deferral
    async fn handle(&self, message: &ProcessedMessage, ctx: &HandlerContext) -> HandlerResult;

    /// Content types this handler processes.
    ///
    /// Used by router to select appropriate handler for a message.
    fn content_types(&self) -> &[ContentType];
}

// ============================================================================
// Handler Context
// ============================================================================

/// Shared context for handler execution.
///
/// Passed to each `handle()` call, providing access to local user info
/// and event emission. Stateless design avoids mutable aliasing.
#[derive(Clone)]
pub struct HandlerContext {
    /// Current user's ID (for sender detection, ownership checks).
    pub local_user_id: UserId,
    /// Broadcast channel for domain events (UI/storage subscribers).
    pub event_tx: broadcast::Sender<DomainEvent>,
}

impl HandlerContext {
    /// Create a new handler context.
    pub fn new(local_user_id: UserId, event_tx: broadcast::Sender<DomainEvent>) -> Self {
        Self {
            local_user_id,
            event_tx,
        }
    }

    /// Emit a domain event to all subscribers.
    ///
    /// Returns number of active receivers, or 0 if no subscribers.
    pub fn emit(&self, event: DomainEvent) -> usize {
        self.event_tx.send(event).unwrap_or(0)
    }
}

// ============================================================================
// Handler Result Types
// ============================================================================

/// Result type for handler operations.
pub type HandlerResult = Result<HandlerOutcome, HandlerError>;

/// Outcome of handler processing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandlerOutcome {
    /// Successfully processed, message should be acknowledged to server.
    Processed,
    /// Not relevant to this handler (wrong content type), skip.
    Ignored,
    /// Needs retry later (missing dependency, rate limited).
    Deferred {
        /// Reason for deferral.
        reason: String,
    },
}

impl HandlerOutcome {
    /// Create a deferred outcome with reason.
    pub fn deferred(reason: impl Into<String>) -> Self {
        Self::Deferred {
            reason: reason.into(),
        }
    }

    /// Check if the outcome indicates success (processed).
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Processed)
    }

    /// Check if the outcome indicates the message was ignored.
    pub fn is_ignored(&self) -> bool {
        matches!(self, Self::Ignored)
    }

    /// Check if the outcome indicates deferral is needed.
    pub fn is_deferred(&self) -> bool {
        matches!(self, Self::Deferred { .. })
    }
}

/// Errors that can occur during handler processing.
#[derive(Debug, Clone)]
pub enum HandlerError {
    /// Protobuf decode failed.
    ParseFailed {
        /// Error message from prost.
        error: String,
    },
    /// Content failed validation (e.g., missing required field).
    ValidationFailed {
        /// Reason for validation failure.
        reason: String,
    },
    /// Required context data missing (e.g., no sender for non-sealed-sender).
    MissingContext {
        /// Field that was expected but not present.
        field: String,
    },
}

impl HandlerError {
    /// Create a parse failed error.
    pub fn parse_failed(error: impl Into<String>) -> Self {
        Self::ParseFailed {
            error: error.into(),
        }
    }

    /// Create a validation failed error.
    pub fn validation_failed(reason: impl Into<String>) -> Self {
        Self::ValidationFailed {
            reason: reason.into(),
        }
    }

    /// Create a missing context error.
    pub fn missing_context(field: impl Into<String>) -> Self {
        Self::MissingContext {
            field: field.into(),
        }
    }

    /// Check if this error is recoverable (should be retried).
    pub fn is_recoverable(&self) -> bool {
        // Parse and validation errors are permanent, missing context might be transient
        matches!(self, Self::MissingContext { .. })
    }
}

impl fmt::Display for HandlerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HandlerError::ParseFailed { error } => write!(f, "parse failed: {}", error),
            HandlerError::ValidationFailed { reason } => write!(f, "validation failed: {}", reason),
            HandlerError::MissingContext { field } => {
                write!(f, "missing required context: {}", field)
            }
        }
    }
}

impl std::error::Error for HandlerError {}

// ============================================================================
// Domain Events
// ============================================================================

/// Domain events emitted by handlers.
///
/// These events flow to UI and storage layers via broadcast channels.
/// Handlers emit events; consumers subscribe and react.
#[derive(Debug, Clone)]
pub enum DomainEvent {
    /// New chat message received.
    MessageReceived {
        /// The complete message with content, sender, channel.
        message: Message,
    },
    /// Existing message was edited.
    MessageEdited {
        /// ID of the edited message.
        message_id: MessageId,
        /// New content after edit.
        new_content: MessageContent,
        /// Channel containing the message.
        channel_id: ChannelId,
    },
    /// Message was deleted (delete for everyone).
    MessageDeleted {
        /// ID of the deleted message (derived from author + timestamp).
        message_id: MessageId,
        /// Author who deleted the message (also the message author for "delete for everyone").
        author_id: UserId,
        /// Channel containing the message.
        /// None for DM deletes where channel is derived from author_id.
        channel_id: Option<ChannelId>,
    },
    /// Attachment metadata ready (needs download).
    AttachmentReady {
        /// Attachment metadata from pointer.
        attachment: Attachment,
        /// Message this attachment belongs to.
        message_id: MessageId,
        /// Channel context.
        channel_id: ChannelId,
    },
    /// Typing indicator received.
    TypingStarted {
        /// User who started typing.
        user_id: UserId,
        /// Channel where typing.
        channel_id: ChannelId,
    },
    /// Typing indicator stopped.
    TypingStopped {
        /// User who stopped typing.
        user_id: UserId,
        /// Channel where typing stopped.
        channel_id: ChannelId,
    },
    /// Read receipt received.
    MessagesRead {
        /// User who read the messages.
        reader_id: UserId,
        /// Message timestamps that were read (Signal uses timestamps as message IDs).
        message_timestamps: Vec<u64>,
        /// When the receipt was sent (ms since epoch).
        receipt_timestamp: u64,
        /// Optional channel context (for anon DM receipts).
        channel_id: Option<ChannelId>,
    },
    /// Delivery receipt received.
    MessagesDelivered {
        /// User whose device received the messages.
        recipient_id: UserId,
        /// Message timestamps that were delivered (Signal uses timestamps as message IDs).
        message_timestamps: Vec<u64>,
        /// When the receipt was sent (ms since epoch).
        receipt_timestamp: u64,
    },
    /// Reaction added to message.
    ReactionAdded {
        /// Target message (identified by timestamp in Signal protocol).
        message_id: MessageId,
        /// Message author's ID (for lookup with target_author_aci).
        author_id: UserId,
        /// User who added the reaction.
        reactor_id: UserId,
        /// Emoji used.
        emoji: String,
        /// When the reaction was sent (ms since epoch).
        timestamp: u64,
    },
    /// Reaction removed from message.
    ReactionRemoved {
        /// Target message (identified by timestamp in Signal protocol).
        message_id: MessageId,
        /// Message author's ID (for lookup with target_author_aci).
        author_id: UserId,
        /// User who removed the reaction.
        reactor_id: UserId,
        /// Emoji that was removed.
        emoji: String,
        /// When the reaction removal was sent (ms since epoch).
        timestamp: u64,
    },
    /// View-once message was viewed (for ephemeral/disappearing messages).
    ViewedReceipt {
        /// The viewed message (identified by timestamp).
        message_timestamp: u64,
        /// User who viewed the message.
        from_user_id: UserId,
        /// When the view occurred (ms since epoch).
        receipt_timestamp: u64,
    },
    /// Message was pinned.
    MessagePinned {
        /// Message that was pinned.
        message_id: MessageId,
        /// User who pinned the message.
        pinner_id: UserId,
        /// Channel context.
        channel_id: ChannelId,
        /// Original sent timestamp (nanoseconds) for UI matching.
        target_sent_timestamp: u64,
    },
    /// Message was unpinned.
    MessageUnpinned {
        /// Message that was unpinned.
        message_id: MessageId,
        /// User who unpinned the message.
        unpinner_id: UserId,
        /// Channel context.
        channel_id: ChannelId,
        /// Original sent timestamp (nanoseconds) for UI matching.
        target_sent_timestamp: u64,
    },
    /// Key exchange received (mailbox DM encryption setup).
    ///
    /// Key can be 32 bytes (regular key exchange) or 64 bytes (DM bootstrap).
    /// Bootstrap format: first 32 bytes = mailbox pk, next 32 = conversation ephemeral.
    KeyExchangeReceived {
        /// User who sent the key exchange.
        sender_id: UserId,
        /// Conversation/channel this key applies to.
        conversation_id: ChannelId,
        /// Sender's ephemeral public key - either 32 bytes (regular) or 64 bytes (bootstrap).
        ephemeral_public_key: Vec<u8>,
        /// When the key exchange was initiated (ms since epoch).
        timestamp: u64,
    },
    /// Sender key distribution received (group encryption setup).
    SenderKeyReceived {
        /// User who distributed the sender key.
        sender_id: UserId,
        /// Device ID of the sender.
        sender_device_id: u32,
        /// Channel this key is for.
        channel_id: ChannelId,
        /// Serialized sender key distribution message (libsignal format).
        distribution_message: Vec<u8>,
        /// When the distribution was sent (ms since epoch).
        timestamp: u64,
    },
    /// Group mailbox address received (NDSS '21 per-group ephemeral mailbox).
    ///
    /// Bundled with SKDM via `ephemeral_reply_to` field, or sent as standalone
    /// announcement when a member joins. Enables routing group messages to
    /// member-specific mailboxes for traffic analysis resistance.
    GroupMailboxReceived {
        /// User who sent the mailbox announcement.
        sender_id: UserId,
        /// Channel this mailbox is for.
        channel_id: ChannelId,
        /// Sender's 32-byte Ed25519 public key for their group mailbox.
        mailbox_public_key: [u8; 32],
        /// When the announcement was sent (ms since epoch).
        timestamp: u64,
    },
    /// Multi-device sync: message deleted locally on another device.
    ///
    /// Different from MessageDeleted (delete for everyone) - this only
    /// syncs local deletion across user's devices.
    DeleteForMeSync {
        /// Conversation containing the deleted message.
        conversation_id: ChannelId,
        /// Author of the deleted message.
        target_author_id: UserId,
        /// The deleted message (derived from timestamp).
        target_message_id: MessageId,
        /// When the sync was issued (ms since epoch).
        timestamp: u64,
    },
    /// Saved message synced from another device (Note to Self).
    ///
    /// Emitted when a SavedMessageSync is received and stored locally.
    /// Treated like MessageReceived — added to message_state for UI display.
    SavedMessageSynced {
        /// The full message (already stored in DB).
        message: crate::domain::message::Message,
    },
    /// Call hangup received from peer (remote call ended).
    ///
    /// This event signals that the peer has ended the call and we should
    /// disconnect from LiveKit and clean up call state.
    CallHangupReceived {
        /// The call ID that was hung up.
        call_id: String,
        /// User who initiated the hangup.
        peer_id: UserId,
        /// Hangup reason (e.g., "normal", "busy", "declined").
        reason: String,
    },
    /// Call key answer received (callee's ephemeral key for PFS).
    ///
    /// Completes the ephemeral key exchange for DM calls. Caller should:
    /// 1. Compute ECDH(our_ephemeral, peer_ephemeral)
    /// 2. Derive call key with identity binding
    /// 3. Update LiveKit encryption and start call
    CallKeyAnswerReceived {
        /// The call ID.
        call_id: String,
        /// Callee who sent the answer.
        peer_id: UserId,
        /// Callee's ephemeral X25519 public key (32 bytes).
        ephemeral_public_key: [u8; 32],
    },
    /// Group call key received (distributed via pairwise sealed sender).
    ///
    /// Used to update LiveKit encryption key for group calls.
    /// Key rotation occurs when members leave.
    GroupCallKeyReceived {
        /// The call ID.
        call_id: String,
        /// User who distributed the key (call initiator or rotator).
        sender_id: UserId,
        /// AES-256 encryption key (32 bytes).
        key_material: [u8; 32],
        /// Key epoch (increments on rotation).
        key_epoch: u32,
        /// Group channel ID.
        channel_id: ChannelId,
    },

    // ========================================================================
    // Anonymous Group Events (NDSS'21 Style)
    // ========================================================================

    /// Invite received to join an anonymous group.
    AnonGroupInviteReceived {
        /// The group being invited to.
        group_id: AnonGroupId,
        /// Human-readable group name.
        group_name: String,
        /// User who sent the invite.
        inviter_id: UserId,
        /// Inviter's mailbox public key for this group.
        inviter_mailbox_pk: [u8; 32],
        /// Inviter's profile key.
        profile_key: Option<[u8; 32]>,
        /// Inviter's decrypted display name.
        inviter_display_name: Option<String>,
        /// Inviter's decrypted avatar.
        inviter_avatar: Option<Vec<u8>>,
    },

    /// Successfully joined an anonymous group.
    AnonGroupJoined {
        /// The group that was joined.
        group_id: AnonGroupId,
    },

    /// A member joined an anonymous group.
    AnonGroupMemberJoined {
        /// The group.
        group_id: AnonGroupId,
        /// User who joined.
        user_id: UserId,
        /// Member's profile key.
        profile_key: Option<[u8; 32]>,
        /// Member's decrypted display name.
        display_name: Option<String>,
        /// Member's decrypted avatar.
        avatar: Option<Vec<u8>>,
    },

    /// A member left an anonymous group voluntarily.
    AnonGroupMemberLeft {
        /// The group.
        group_id: AnonGroupId,
        /// User who left.
        user_id: UserId,
    },

    /// A member was kicked from an anonymous group.
    AnonGroupMemberKicked {
        /// The group.
        group_id: AnonGroupId,
        /// User who was kicked.
        user_id: UserId,
        /// Admin who performed the kick.
        kicked_by: UserId,
    },

    /// Message received in an anonymous group.
    AnonGroupMessageReceived {
        /// The group.
        group_id: AnonGroupId,
        /// Sender of the message.
        sender_id: UserId,
        /// The stored message ID.
        message_id: AnonGroupMessageId,
        /// The inner message content.
        inner_content: Content,
        /// The sender's timestamp (nanoseconds) for ordering.
        sent_at: u64,
    },

    /// Anonymous group bootstrap complete (all members confirmed).
    AnonGroupBootstrapComplete {
        /// The group.
        group_id: AnonGroupId,
    },

    /// Member list received for an anonymous group.
    AnonGroupMemberListReceived {
        /// The group.
        group_id: AnonGroupId,
        /// List of members.
        members: Vec<crate::proto::content_v1::AnonGroupMemberInfo>,
    },

    // ========================================================================
    // Anonymous DM Events (Client-Side Managed)
    // ========================================================================

    /// Message request received from a stranger (public account mode).
    AnonDmRequestReceived {
        /// The conversation ID.
        conversation_id: AnonDmId,
        /// User who sent the request.
        sender_id: UserId,
        /// Sender's mailbox public key for this conversation.
        sender_mailbox_pk: [u8; 32],
        /// Introductory message text.
        intro_message: Option<String>,
        /// Sender's profile key (for decrypting their display name).
        profile_key: Option<[u8; 32]>,
        /// Sender's display name (decrypted from encrypted_display_name).
        sender_display_name: Option<String>,
        /// Sender's avatar (decrypted from encrypted_avatar).
        sender_avatar: Option<Vec<u8>>,
    },

    /// Our message request was accepted by the recipient.
    AnonDmRequestAccepted {
        /// The conversation ID.
        conversation_id: AnonDmId,
        /// User who accepted.
        peer_id: UserId,
        /// Peer's mailbox public key for this conversation.
        peer_mailbox_pk: [u8; 32],
        /// Peer's profile key (for decrypting their display name).
        profile_key: Option<[u8; 32]>,
        /// Peer's display name (revealed upon acceptance).
        peer_display_name: String,
        /// Peer's avatar (decrypted from encrypted_avatar).
        peer_avatar: Option<Vec<u8>>,
    },

    /// Message received in an anonymous DM conversation.
    ///
    /// The message is already stored in the DB by the time this fires.
    /// Consumers should fetch it via `anon_dms().get_message(&message_id)`.
    AnonDmMessageReceived {
        /// The conversation ID.
        conversation_id: AnonDmId,
        /// Sender of the message.
        sender_id: UserId,
        /// The stored message ID (fetch from DB for display).
        message_id: AnonDmMessageId,
        /// The sender's timestamp (nanoseconds) for ordering.
        sent_at: u64,
    },

    /// User presence changed (online/away/dnd/offline).
    UserPresenceChanged {
        /// User whose presence changed.
        user_id: UserId,
        /// New presence status.
        presence: crate::proto::UserPresence,
        /// Timestamp of the change (nanoseconds).
        timestamp: u64,
    },

    /// Anonymous DM profile updated.
    AnonDmProfileUpdated {
        /// Conversation ID.
        conversation_id: crate::domain::anon_dm::AnonDmId,
        /// User whose profile was updated.
        user_id: UserId,
        /// Updated display name (if changed).
        display_name: Option<String>,
        /// Updated avatar URL (if changed).
        avatar_url: Option<String>,
        /// Updated bio/status message (if changed).
        bio: Option<String>,
        /// Timestamp of the update (nanoseconds).
        timestamp: u64,
    },

    /// NDSS'21 ECIES init message received (private friend contact).
    ///
    /// This is the Phase 1 HPKE-encrypted initial contact message.
    /// Contains the initiator's identity and ephemeral keys for Phase 2 Double Ratchet.
    /// Sender identity is revealed after decryption per NDSS'21 Section II-A.
    AnonDmInitReceived {
        /// The conversation ID (generated by initiator).
        conversation_id: AnonDmId,
        /// Initiator's user ID (revealed after decryption per NDSS'21).
        sender_id: UserId,
        /// Initiator's display name (revealed after decryption).
        sender_display_name: String,
        /// Initiator's avatar (revealed after decryption).
        sender_avatar: Option<Vec<u8>>,
        /// Initiator's mailbox public key for this conversation.
        peer_mailbox_pk: [u8; 32],
        /// Initiator's Signal identity key (33 bytes with 0x05 prefix).
        peer_identity_key: [u8; 33],
        /// Initiator's delivery token for sealed sender.
        peer_delivery_token: [u8; 12],
        /// Optional intro message.
        intro_message: String,
        /// Server timestamp (for ordering).
        server_timestamp: u64,
    },

    /// Message deleted in an anonymous DM conversation.
    AnonDmMessageDeleted {
        /// The conversation ID.
        conversation_id: AnonDmId,
        /// The message timestamp (used as ID in Signal protocol).
        message_timestamp: u64,
    },

    /// Message edited in an anonymous DM conversation.
    AnonDmMessageEdited {
        /// The conversation ID.
        conversation_id: AnonDmId,
        /// The message timestamp (used as ID in Signal protocol).
        message_timestamp: u64,
        /// The new message text.
        new_text: String,
    },

    /// Message edited in an anonymous group.
    AnonGroupMessageEdited {
        /// The group ID.
        group_id: AnonGroupId,
        /// The message timestamp (used as ID in Signal protocol).
        message_timestamp: u64,
        /// The new message text.
        new_text: String,
    },

    /// Anonymous call offer received.
    AnonCallOfferReceived {
        /// The conversation ID.
        conversation_id: AnonDmId,
        /// Caller's user ID.
        sender_id: UserId,
        /// The call room ID.
        call_room_id: Vec<u8>,
        /// The LiveKit room name.
        room_name: String,
        /// Call type (voice = 1, video = 2).
        call_type: i32,
        /// Caller's display name.
        caller_display_name: String,
        /// E2EE encryption key (32 bytes).
        call_encryption_key: Vec<u8>,
        /// Timestamp of the offer.
        timestamp: u64,
    },

    /// Anonymous call answer received.
    AnonCallAnswerReceived {
        /// The conversation ID.
        conversation_id: AnonDmId,
        /// Callee's user ID.
        sender_id: UserId,
        /// The call room ID.
        call_room_id: Vec<u8>,
        /// Whether the call was accepted.
        accepted: bool,
        /// Callee's display name.
        callee_display_name: String,
        /// Timestamp of the answer.
        timestamp: u64,
    },

    /// Anonymous call hangup received.
    AnonCallHangupReceived {
        /// The conversation ID.
        conversation_id: AnonDmId,
        /// Sender's user ID.
        sender_id: UserId,
        /// The call room ID.
        call_room_id: Vec<u8>,
        /// Hangup reason.
        reason: i32,
        /// Timestamp of the hangup.
        timestamp: u64,
    },

    /// Anonymous group call offer received.
    AnonGroupCallOfferReceived {
        /// The group ID.
        group_id: AnonGroupId,
        /// Caller's user ID.
        sender_id: UserId,
        /// The call room ID.
        call_room_id: Vec<u8>,
        /// The LiveKit room name.
        room_name: String,
        /// Call type (voice = 1, video = 2).
        call_type: i32,
        /// Caller's display name.
        caller_display_name: String,
        /// E2EE encryption key (32 bytes).
        call_encryption_key: Vec<u8>,
        /// Timestamp of the offer.
        timestamp: u64,
    },

    /// Anonymous group call answer received (member joined/declined).
    AnonGroupCallAnswerReceived {
        /// The group ID.
        group_id: AnonGroupId,
        /// Callee's user ID.
        sender_id: UserId,
        /// The call room ID.
        call_room_id: Vec<u8>,
        /// Whether the call was accepted.
        accepted: bool,
        /// Callee's display name.
        callee_display_name: String,
        /// Timestamp of the answer.
        timestamp: u64,
    },

    /// Anonymous group call hangup received.
    AnonGroupCallHangupReceived {
        /// The group ID.
        group_id: AnonGroupId,
        /// Sender's user ID.
        sender_id: UserId,
        /// The call room ID.
        call_room_id: Vec<u8>,
        /// Hangup reason.
        reason: i32,
        /// Timestamp of the hangup.
        timestamp: u64,
    },

    // ========================================================================
    // UI-Specific Events (previously handled only in UI layer)
    // ========================================================================

    /// Call key offer received (incoming call with ephemeral key for PFS).
    ///
    /// Initiates the call key exchange. Callee should:
    /// 1. Generate ephemeral keypair
    /// 2. Show incoming call modal
    /// 3. If accepted, send CallKeyAnswer
    CallKeyOfferReceived {
        /// The call ID.
        call_id: String,
        /// Caller's user ID.
        peer_id: UserId,
        /// Caller's ephemeral X25519 public key (32 bytes).
        ephemeral_public_key: [u8; 32],
        /// Call type: "Voice Call" or "Video Call".
        call_type: String,
        /// DM channel ID for context.
        channel_id: String,
        /// Whether caller's identity is verified (known identity key).
        is_identity_verified: bool,
        /// Identity warning message (if verification failed).
        identity_warning: String,
        /// Caller's display name.
        caller_name: String,
        /// Timestamp when offer was sent.
        timestamp: u64,
    },

    /// DM bootstrap completed - shared secret established with peer.
    ///
    /// Emitted when key exchange with a DM peer completes successfully.
    /// UI should enable composer and flush pending messages.
    DmBootstrapCompleted {
        /// Peer user ID.
        peer_id: UserId,
    },

    /// Unread count updated for a conversation.
    ///
    /// Used to update sidebar badges and notification counts.
    UnreadCountUpdated {
        /// Channel ID (DM or group channel).
        channel_id: ChannelId,
        /// Peer user ID (for DMs).
        peer_id: Option<UserId>,
        /// New unread message count.
        unread_count: u32,
    },

    /// Friend requests list was updated.
    ///
    /// Emitted when received or sent friend requests change.
    FriendRequestsUpdated {
        /// Received friend requests.
        received: Vec<FriendRequestInfo>,
        /// Sent friend requests.
        sent: Vec<FriendRequestInfo>,
    },

    /// Friend request was accepted.
    ///
    /// Emitted when a friend request (ours or theirs) is accepted.
    /// A new DM channel is created for the friendship.
    FriendAccepted {
        /// Friend's user ID.
        friend_id: UserId,
        /// DM channel ID created for this friendship.
        channel_id: ChannelId,
    },

    /// Anonymous group metadata was updated.
    ///
    /// Emitted when group name, description, or roles change.
    AnonGroupUpdated {
        /// The group ID.
        group_id: AnonGroupId,
        /// The update payload.
        update: crate::proto::content_v1::AnonGroupUpdate,
        /// User who made the update.
        updated_by: UserId,
    },

    // ========================================================================
    // Local Send Events (emitted when user sends a message)
    // ========================================================================

    /// Message was sent successfully.
    MessageSent {
        /// The message ID.
        message_id: MessageId,
        /// The channel ID.
        channel_id: ChannelId,
        /// The sender ID (current user).
        sender_id: UserId,
        /// The message content.
        content: String,
        /// When the message was sent.
        sent_at: chrono::DateTime<chrono::Utc>,
    },

    /// Message send failed.
    MessageSendFailed {
        /// The channel ID.
        channel_id: ChannelId,
        /// The error message.
        error: String,
    },

    /// Channel member joined.
    ChannelMemberJoined {
        /// The channel ID.
        channel_id: ChannelId,
        /// User who joined.
        user_id: UserId,
        /// Display name of the user.
        display_name: String,
        /// System message created for this event.
        message: Message,
    },

    /// Channel member left.
    ChannelMemberLeft {
        /// The channel ID.
        channel_id: ChannelId,
        /// User who left.
        user_id: UserId,
        /// Display name of the user.
        display_name: String,
        /// System message created for this event.
        message: Message,
    },
}

impl DomainEvent {
    /// Get the channel ID associated with this event, if any.
    pub fn channel_id(&self) -> Option<ChannelId> {
        match self {
            DomainEvent::MessageReceived { message } => Some(message.channel_id),
            DomainEvent::MessageEdited { channel_id, .. } => Some(*channel_id),
            DomainEvent::MessageDeleted { channel_id, .. } => *channel_id,
            DomainEvent::AttachmentReady { channel_id, .. } => Some(*channel_id),
            DomainEvent::TypingStarted { channel_id, .. } => Some(*channel_id),
            DomainEvent::TypingStopped { channel_id, .. } => Some(*channel_id),
            DomainEvent::MessagesRead { channel_id, .. } => *channel_id,
            DomainEvent::MessagesDelivered { .. } => None, // Cross-channel
            DomainEvent::ViewedReceipt { .. } => None, // Cross-channel
            DomainEvent::ReactionAdded { .. } => None, // Reactions now use message lookup, not channel
            DomainEvent::ReactionRemoved { .. } => None, // Reactions now use message lookup, not channel
            DomainEvent::MessagePinned { channel_id, .. } => Some(*channel_id),
            DomainEvent::MessageUnpinned { channel_id, .. } => Some(*channel_id),
            DomainEvent::KeyExchangeReceived { conversation_id, .. } => Some(*conversation_id),
            DomainEvent::SenderKeyReceived { channel_id, .. } => Some(*channel_id),
            DomainEvent::GroupMailboxReceived { channel_id, .. } => Some(*channel_id),
            DomainEvent::DeleteForMeSync { conversation_id, .. } => Some(*conversation_id),
            DomainEvent::SavedMessageSynced { message } => Some(message.channel_id),
            DomainEvent::CallHangupReceived { .. } => None, // Calls don't have a channel context here
            DomainEvent::CallKeyAnswerReceived { .. } => None, // Call answer doesn't have channel context
            DomainEvent::GroupCallKeyReceived { channel_id, .. } => Some(*channel_id),
            // Anon group events don't have ChannelId (they use AnonGroupId)
            DomainEvent::AnonGroupInviteReceived { .. } => None,
            DomainEvent::AnonGroupJoined { .. } => None,
            DomainEvent::AnonGroupMemberJoined { .. } => None,
            DomainEvent::AnonGroupMemberLeft { .. } => None,
            DomainEvent::AnonGroupMemberKicked { .. } => None,
            DomainEvent::AnonGroupMessageReceived { .. } => None,
            DomainEvent::AnonGroupBootstrapComplete { .. } => None,
            DomainEvent::AnonGroupMemberListReceived { .. } => None,
            // Anon DM events don't have ChannelId (they use AnonDmId)
            DomainEvent::AnonDmRequestReceived { .. } => None,
            DomainEvent::AnonDmRequestAccepted { .. } => None,
            DomainEvent::AnonDmMessageReceived { .. } => None,
            DomainEvent::AnonDmInitReceived { .. } => None,
            DomainEvent::AnonDmMessageDeleted { .. } => None,
            DomainEvent::AnonDmMessageEdited { .. } => None,
            DomainEvent::AnonGroupMessageEdited { .. } => None,
            // Anonymous call events don't have ChannelId
            DomainEvent::AnonCallOfferReceived { .. } => None,
            DomainEvent::AnonCallAnswerReceived { .. } => None,
            DomainEvent::AnonCallHangupReceived { .. } => None,
            DomainEvent::AnonGroupCallOfferReceived { .. } => None,
            DomainEvent::AnonGroupCallAnswerReceived { .. } => None,
            DomainEvent::AnonGroupCallHangupReceived { .. } => None,
            // UI-specific events
            DomainEvent::CallKeyOfferReceived { .. } => None,
            DomainEvent::DmBootstrapCompleted { .. } => None,
            DomainEvent::UnreadCountUpdated { channel_id, .. } => Some(*channel_id),
            DomainEvent::FriendRequestsUpdated { .. } => None,
            DomainEvent::FriendAccepted { channel_id, .. } => Some(*channel_id),
            DomainEvent::AnonGroupUpdated { .. } => None,
            // Local send events
            DomainEvent::MessageSent { channel_id, .. } => Some(*channel_id),
            DomainEvent::MessageSendFailed { channel_id, .. } => Some(*channel_id),
            // Channel member events
            DomainEvent::ChannelMemberJoined { channel_id, .. } => Some(*channel_id),
            DomainEvent::ChannelMemberLeft { channel_id, .. } => Some(*channel_id),
            // User presence events
            DomainEvent::UserPresenceChanged { .. } => None,
            DomainEvent::AnonDmProfileUpdated { .. } => None,
        }
    }

    /// Check if this is a message-related event.
    pub fn is_message_event(&self) -> bool {
        matches!(
            self,
            DomainEvent::MessageReceived { .. }
                | DomainEvent::MessageEdited { .. }
                | DomainEvent::MessageDeleted { .. }
        )
    }

    /// Check if this is a transient event (not persisted).
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            DomainEvent::TypingStarted { .. } | DomainEvent::TypingStopped { .. }
        )
    }
}

// ============================================================================
// Handler Registry
// ============================================================================

/// Registry for routing messages to appropriate handlers.
///
/// The registry maintains a collection of handlers and routes incoming
/// `ProcessedMessage` objects to the handler that supports their content type.
///
/// # Example
///
/// ```ignore
/// let (event_tx, event_rx) = broadcast::channel(64);
/// let registry = HandlerRegistry::new(local_user_id, event_tx);
///
/// // Route an incoming message
/// let result = registry.route(&processed_message).await;
///
/// // Subscribe to domain events
/// let mut rx = registry.subscribe();
/// while let Ok(event) = rx.recv().await {
///     match event {
///         DomainEvent::MessageReceived { message } => { ... }
///         DomainEvent::TypingStarted { user_id, channel_id } => { ... }
///         _ => {}
///     }
/// }
/// ```
///
/// # Adding Custom Handlers
///
/// Use `register()` to add custom handlers:
///
/// ```ignore
/// registry.register(Arc::new(MyCustomHandler::new()));
/// ```
pub struct HandlerRegistry {
    handlers: Vec<Arc<dyn InboxHandler>>,
    context: HandlerContext,
}

impl HandlerRegistry {
    /// Create a new handler registry with default handlers.
    ///
    /// # Arguments
    ///
    /// * `local_user_id` - The current user's ID (for sender detection, ownership checks)
    /// * `event_tx` - Broadcast channel for domain events
    ///
    /// # Default Handlers
    ///
    /// - `ChatHandler` - DataMessage content (text, media, edits, deletes)
    /// - `AttachmentHandler` - Attachment metadata from pointers
    /// - `ReactionHandler` - Emoji reactions on messages
    /// - `ReceiptHandler` - Delivery/read/viewed receipts
    /// - `TypingHandler` - Typing indicators
    /// - `KeyExchangeHandler` - Mailbox key exchange for DM encryption
    /// - `SenderKeyHandler` - Group sender key distribution
    /// - `PinHandler` - Message pin/unpin events
    /// - `SyncHandler` - Multi-device sync messages (DeleteForMeSync)
    pub fn new(local_user_id: UserId, event_tx: broadcast::Sender<DomainEvent>) -> Self {
        let context = HandlerContext::new(local_user_id, event_tx);

        let handlers: Vec<Arc<dyn InboxHandler>> = vec![
            Arc::new(ChatHandler::new()),
            Arc::new(AttachmentHandler::new()),
            Arc::new(ReactionHandler::new()),
            Arc::new(ReceiptHandler::new()),
            Arc::new(TypingHandler::new()),
            // Phase 25 handlers
            Arc::new(KeyExchangeHandler::new()),
            Arc::new(SenderKeyHandler::new()),
            Arc::new(PinHandler::new()),
            Arc::new(SyncHandler::new()),
        ];

        Self { handlers, context }
    }

    /// Route a processed message to the appropriate handler.
    ///
    /// Finds the first handler that supports the message's content type
    /// and delegates processing to it.
    ///
    /// # Returns
    ///
    /// - `Ok(HandlerOutcome::Processed)` - Message was successfully handled
    /// - `Ok(HandlerOutcome::Ignored)` - No handler for this content type
    /// - `Ok(HandlerOutcome::Deferred { reason })` - Handler needs retry
    /// - `Err(HandlerError)` - Handler encountered an error
    pub async fn route(&self, message: &ProcessedMessage) -> HandlerResult {
        // Find handler that supports this content type
        for handler in &self.handlers {
            if handler.content_types().contains(&message.content_type) {
                return handler.handle(message, &self.context).await;
            }
        }

        // No handler found for content type
        tracing::debug!(
            content_type = %message.content_type,
            "No handler registered for content type"
        );
        Ok(HandlerOutcome::Ignored)
    }

    /// Register an additional handler.
    ///
    /// Custom handlers are appended to the handler list. The first handler
    /// that supports a content type wins, so built-in handlers take precedence.
    ///
    /// # Arguments
    ///
    /// * `handler` - The handler to register
    pub fn register(&mut self, handler: Arc<dyn InboxHandler>) {
        self.handlers.push(handler);
    }

    /// Subscribe to domain events.
    ///
    /// Returns a receiver that will receive all domain events emitted by handlers.
    /// Multiple subscribers can exist simultaneously.
    pub fn subscribe(&self) -> broadcast::Receiver<DomainEvent> {
        self.context.event_tx.subscribe()
    }

    /// Get the number of registered handlers.
    pub fn handler_count(&self) -> usize {
        self.handlers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_handler_outcome_constructors() {
        assert!(HandlerOutcome::Processed.is_success());
        assert!(HandlerOutcome::Ignored.is_ignored());

        let deferred = HandlerOutcome::deferred("rate limited");
        assert!(deferred.is_deferred());
        if let HandlerOutcome::Deferred { reason } = deferred {
            assert_eq!(reason, "rate limited");
        }
    }

    #[test]
    fn test_handler_error_display() {
        let parse_err = HandlerError::parse_failed("invalid protobuf");
        assert_eq!(parse_err.to_string(), "parse failed: invalid protobuf");

        let validation_err = HandlerError::validation_failed("missing body");
        assert_eq!(
            validation_err.to_string(),
            "validation failed: missing body"
        );

        let context_err = HandlerError::missing_context("sender_address");
        assert_eq!(
            context_err.to_string(),
            "missing required context: sender_address"
        );
    }

    #[test]
    fn test_handler_error_is_recoverable() {
        assert!(!HandlerError::parse_failed("test").is_recoverable());
        assert!(!HandlerError::validation_failed("test").is_recoverable());
        assert!(HandlerError::missing_context("test").is_recoverable());
    }

    #[test]
    fn test_domain_event_is_transient() {
        let channel_id = ChannelId::new();
        let user_id = UserId::new();

        let typing = DomainEvent::TypingStarted {
            user_id,
            channel_id,
        };
        assert!(typing.is_transient());

        let msg = DomainEvent::MessageReceived {
            message: Message::new_text(channel_id, Some(user_id), "test".to_string(), 0),
        };
        assert!(!msg.is_transient());
    }

    #[test]
    fn test_domain_event_channel_id() {
        let channel_id = ChannelId::new();
        let user_id = UserId::new();

        let msg = DomainEvent::MessageReceived {
            message: Message::new_text(channel_id, Some(user_id), "test".to_string(), 0),
        };
        assert_eq!(msg.channel_id(), Some(channel_id));

        let read = DomainEvent::MessagesRead {
            reader_id: user_id,
            message_timestamps: vec![123],
            receipt_timestamp: 456,
            channel_id: Some(channel_id),
        };
        assert_eq!(read.channel_id(), Some(channel_id)); // With channel context

        // Test DeleteForMeSync event
        let message_id = MessageId::new();
        let delete_sync = DomainEvent::DeleteForMeSync {
            conversation_id: channel_id,
            target_author_id: user_id,
            target_message_id: message_id,
            timestamp: 1704067200000,
        };
        assert_eq!(delete_sync.channel_id(), Some(channel_id));
        assert!(!delete_sync.is_transient()); // DeleteForMeSync is NOT transient
    }

    #[test]
    fn test_domain_event_delete_for_me_sync() {
        let channel_id = ChannelId::new();
        let user_id = UserId::new();
        let message_id = MessageId::new();

        let event = DomainEvent::DeleteForMeSync {
            conversation_id: channel_id,
            target_author_id: user_id,
            target_message_id: message_id,
            timestamp: 1704067200000,
        };

        // Test channel_id()
        assert_eq!(event.channel_id(), Some(channel_id));

        // Test is_transient() - DeleteForMeSync should be persisted
        assert!(!event.is_transient());

        // Test is_message_event() - this is a sync event, not a direct message event
        assert!(!event.is_message_event());
    }

    #[test]
    fn test_handler_context_emit() {
        let (tx, mut rx) = broadcast::channel(16);
        let ctx = HandlerContext::new(UserId::new(), tx);

        let channel_id = ChannelId::new();
        let user_id = UserId::new();

        ctx.emit(DomainEvent::TypingStarted {
            user_id,
            channel_id,
        });

        let event = rx.try_recv().unwrap();
        if let DomainEvent::TypingStarted {
            user_id: u,
            channel_id: c,
        } = event
        {
            assert_eq!(u, user_id);
            assert_eq!(c, channel_id);
        } else {
            panic!("Wrong event type");
        }
    }

    // ========================================================================
    // HandlerRegistry Tests
    // ========================================================================

    use crate::proto::content_v1::{content, typing_message, Content, TypingMessage};
    use crate::proto::mailbox_v1::EnvelopeType;
    use libsignal_protocol::{DeviceId, ProtocolAddress};
    use prost::Message as ProstMessage;
    use uuid::Uuid;

    fn make_registry() -> (HandlerRegistry, broadcast::Receiver<DomainEvent>) {
        let (tx, rx) = broadcast::channel(64);
        let registry = HandlerRegistry::new(UserId::new(), tx);
        (registry, rx)
    }

    fn make_processed(content_type: ContentType, plaintext: Vec<u8>) -> ProcessedMessage {
        ProcessedMessage::new(
            "msg-test".to_string(),
            Some(ProtocolAddress::new(
                Uuid::new_v4().to_string(),
                DeviceId::try_from(1u32).unwrap(),
            )),
            content_type,
            plaintext,
            1234567890000,
            EnvelopeType::SealedSender,
        )
    }

    #[test]
    fn test_registry_has_default_handlers() {
        let (registry, _rx) = make_registry();
        // 9 handlers: Chat, Attachment, Reaction, Receipt, Typing, KeyExchange, SenderKey, Pin, Sync
        assert_eq!(registry.handler_count(), 9);
    }

    #[test]
    fn test_registry_register_custom_handler() {
        let (mut registry, _rx) = make_registry();
        assert_eq!(registry.handler_count(), 9);

        // Add custom handler
        registry.register(Arc::new(TypingHandler::new()));
        assert_eq!(registry.handler_count(), 10);
    }

    #[test]
    fn test_registry_subscribe() {
        let (registry, _rx) = make_registry();
        // Can create multiple subscribers
        let _rx1 = registry.subscribe();
        let _rx2 = registry.subscribe();
    }

    #[tokio::test]
    async fn test_registry_routes_typing_message() {
        let (registry, mut rx) = make_registry();

        // Create typing message content
        let typing = TypingMessage {
            action: typing_message::Action::Started as i32,
            timestamp: 1704067200000,
            channel_id: Uuid::new_v4().to_string(),
        };

        let content = Content {
            content: Some(content::Content::Typing(typing)),
            ephemeral_reply_to: None,
            timestamp: 1704067200000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed(ContentType::TypingMessage, plaintext);

        let result = registry.route(&processed).await.unwrap();
        assert!(result.is_success());

        // Verify event was emitted
        let event = rx.try_recv().unwrap();
        assert!(matches!(event, DomainEvent::TypingStarted { .. }));
    }

    #[tokio::test]
    async fn test_registry_routes_receipt_message() {
        use crate::proto::content_v1::{receipt_message, ReceiptMessage};

        let (registry, mut rx) = make_registry();

        let receipt = ReceiptMessage {
            r#type: receipt_message::Type::Read as i32,
            timestamps: vec![1704067200000],
            timestamp: 1704067201000,
            channel_id: String::new(),
        };

        let content = Content {
            content: Some(content::Content::Receipt(receipt)),
            ephemeral_reply_to: None,
            timestamp: 1704067201000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed(ContentType::ReceiptMessage, plaintext);

        let result = registry.route(&processed).await.unwrap();
        assert!(result.is_success());

        // Verify event was emitted
        let event = rx.try_recv().unwrap();
        assert!(matches!(event, DomainEvent::MessagesRead { .. }));
    }

    #[tokio::test]
    async fn test_registry_returns_ignored_for_unknown() {
        let (registry, _rx) = make_registry();

        // Unknown content type
        let processed = make_processed(ContentType::Unknown(99), vec![]);

        let result = registry.route(&processed).await.unwrap();
        assert!(result.is_ignored());
    }

    #[tokio::test]
    async fn test_registry_routes_null_message() {
        let (registry, _rx) = make_registry();

        // NullMessage has no handler currently
        let processed = make_processed(ContentType::NullMessage, vec![]);

        let result = registry.route(&processed).await.unwrap();
        assert!(result.is_ignored());
    }

    #[tokio::test]
    async fn test_registry_routes_sender_key_distribution() {
        use crate::proto::content_v1::SenderKeyDistribution;

        let (registry, mut rx) = make_registry();

        let sender_key = SenderKeyDistribution {
            channel_id: Uuid::new_v4().to_string(),
            distribution_message: vec![0x01, 0x02, 0x03, 0x04],
            timestamp: 1704067200000,
        };

        let content = Content {
            content: Some(content::Content::SenderKey(sender_key)),
            ephemeral_reply_to: None,
            timestamp: 1704067200000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed(ContentType::SenderKeyDistribution, plaintext);

        let result = registry.route(&processed).await.unwrap();
        assert!(result.is_success());

        // Verify event was emitted
        let event = rx.try_recv().unwrap();
        assert!(matches!(event, DomainEvent::SenderKeyReceived { .. }));
    }

    #[tokio::test]
    async fn test_registry_routes_sync_message_delete_for_me() {
        use crate::proto::content_v1::DeleteForMeSync;

        let (registry, mut rx) = make_registry();

        let delete_sync = DeleteForMeSync {
            conversation_id: Uuid::new_v4().to_string(),
            target_author_aci: Uuid::new_v4().to_string(),
            target_sent_timestamp: 1704067200000,
            timestamp: 1704067201000,
        };

        let content = Content {
            content: Some(content::Content::DeleteForMeSync(delete_sync)),
            ephemeral_reply_to: None,
            timestamp: 1704067201000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed(ContentType::SyncMessage, plaintext);

        let result = registry.route(&processed).await.unwrap();
        assert!(result.is_success());

        // Verify event was emitted
        let event = rx.try_recv().unwrap();
        assert!(matches!(event, DomainEvent::DeleteForMeSync { .. }));
    }
}
