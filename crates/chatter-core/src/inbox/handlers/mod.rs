//! Inbox domain events + shared anon content dispatcher.
//!
//! `DomainEvent` is the high-level event type emitted by every inbox dispatcher
//! (polling handlers, `services::inbox_processor`, `AnonDmService`,
//! `AnonGroupService`). Subscribers (state stores, UI) consume these via the
//! broadcast bus in `events::EventBus`.
//!
//! `base_anon` holds the shared content dispatcher used by both
//! `AnonDmService::process_incoming_content` and
//! `AnonGroupService::process_incoming_content`.

pub mod base_anon;

use crate::domain::{Attachment, ChannelId, Message, MessageContent, MessageId, UserId};
use crate::domain::message::ConversationType;
use crate::domain::anon_dm::AnonDmId;
use crate::domain::anon_group::AnonGroupId;
use crate::inbox::events::FriendRequestInfo;

// ============================================================================
// Domain Events
// ============================================================================

/// Domain events emitted by inbox dispatchers.
///
/// These events flow to UI and storage layers via broadcast channels.
/// Dispatchers emit events; consumers subscribe and react.
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
        /// Conversation containing the message.
        channel_id: ChannelId,
        /// Type of conversation (Channel, AnonDm, AnonGroup, etc.).
        conversation_type: ConversationType,
        /// Sender's original timestamp (fallback for UI matching when message_id doesn't match).
        target_sent_timestamp: u64,
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
        /// Target message's original sent timestamp in nanoseconds.
        /// Used for Signal-style (sender, timestamp) UI matching.
        target_sent_timestamp: u64,
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
        user_id: UserId,
        channel_id: ChannelId,
    },
    /// Typing indicator stopped.
    TypingStopped {
        user_id: UserId,
        channel_id: ChannelId,
    },
    /// Read receipt received.
    MessagesRead {
        reader_id: UserId,
        message_timestamps: Vec<u64>,
        receipt_timestamp: u64,
        /// Optional channel context (for anon DM receipts).
        channel_id: Option<ChannelId>,
    },
    /// Delivery receipt received.
    MessagesDelivered {
        recipient_id: UserId,
        message_timestamps: Vec<u64>,
        receipt_timestamp: u64,
    },
    /// Reaction added to message.
    ReactionAdded {
        /// Target message sender (target_author_aci in Signal protocol).
        sender_id: UserId,
        /// Target message sent timestamp in nanoseconds (target_sent_timestamp).
        sent_at_ns: u64,
        reactor_id: UserId,
        emoji: String,
        /// When the reaction was sent (ms since epoch).
        timestamp: u64,
    },
    /// Reaction removed from message.
    ReactionRemoved {
        sender_id: UserId,
        sent_at_ns: u64,
        reactor_id: UserId,
        emoji: String,
        timestamp: u64,
    },
    /// View-once message was viewed (for ephemeral/disappearing messages).
    ViewedReceipt {
        message_timestamp: u64,
        from_user_id: UserId,
        receipt_timestamp: u64,
    },
    /// Message was pinned.
    MessagePinned {
        message_id: MessageId,
        pinner_id: UserId,
        channel_id: ChannelId,
        /// Original sent timestamp (nanoseconds) for UI matching.
        target_sent_timestamp: u64,
    },
    /// Message was unpinned.
    MessageUnpinned {
        message_id: MessageId,
        unpinner_id: UserId,
        channel_id: ChannelId,
        target_sent_timestamp: u64,
    },
    /// Key exchange received (mailbox DM encryption setup).
    ///
    /// Key can be 32 bytes (regular key exchange) or 64 bytes (DM bootstrap).
    /// Bootstrap format: first 32 bytes = mailbox pk, next 32 = conversation ephemeral.
    KeyExchangeReceived {
        sender_id: UserId,
        conversation_id: ChannelId,
        ephemeral_public_key: Vec<u8>,
        timestamp: u64,
    },
    /// Sender key distribution received (group encryption setup).
    SenderKeyReceived {
        sender_id: UserId,
        sender_device_id: u32,
        channel_id: ChannelId,
        /// Serialized sender key distribution message (libsignal format).
        distribution_message: Vec<u8>,
        timestamp: u64,
    },
    /// Group mailbox address received (NDSS '21 per-group ephemeral mailbox).
    GroupMailboxReceived {
        sender_id: UserId,
        channel_id: ChannelId,
        /// Sender's 32-byte Ed25519 public key for their group mailbox.
        mailbox_public_key: [u8; 32],
        timestamp: u64,
    },
    /// Multi-device sync: message deleted locally on another device.
    ///
    /// Different from MessageDeleted (delete for everyone) — this only
    /// syncs local deletion across user's devices.
    DeleteForMeSync {
        conversation_id: ChannelId,
        target_author_id: UserId,
        target_message_id: MessageId,
        timestamp: u64,
    },
    /// Saved message synced from another device (Note to Self).
    SavedMessageSynced {
        message: crate::domain::message::Message,
    },

    // ========================================================================
    // Multi-Device Lifecycle Events
    // ========================================================================

    /// A new linked device has joined this account.
    ///
    /// Fired on the *approving* primary device immediately after a successful
    /// `provision_device` upload, and on *all other already-linked devices*
    /// once the server fans out a device-add notification (server support
    /// pending — see `project_multidevice_fanout_audit.md`).
    ///
    /// UI should surface a non-blocking toast / banner so the user can
    /// confirm or revoke an unexpected pairing.
    DeviceLinked {
        /// Server-assigned device ID for the newly-linked device.
        device_id: u32,
        /// User-supplied device name from the QR payload, if any.
        device_name: Option<String>,
        /// Platform/model string supplied by the new device.
        device_info: Option<String>,
        /// True if this notification is being delivered to the device that
        /// approved the link (versus a third device receiving fan-out).
        approved_by_self: bool,
    },

    /// Server-initiated Signal session reset. Client wipes its local
    /// ratchet / chain-key state with `peer_user_id` (or a specific
    /// `peer_device_id` if set) so the next send rebuilds via X3DH.
    /// Emitted by the gRPC notification stream consumer on SESSION_RESET.
    SessionResetRequested {
        peer_user_id: UserId,
        /// When None, drop every session with every device of peer_user_id.
        peer_device_id: Option<u32>,
    },

    /// A peer who shares a channel with the local user added or revoked a
    /// device. Consumer must invalidate cached Signal sessions for
    /// `peer_user_id` and re-distribute sender keys on next group send.
    PeerDevicesChanged {
        peer_user_id: UserId,
        /// The device that was added or revoked.
        device_id: u32,
        /// `"added"` or `"revoked"`. Clients may use this to decide
        /// whether to proactively fetch the new device's prekeys (added)
        /// or just prune stale session state (revoked).
        change_type: String,
    },

    /// Incoming call presented by the call orchestrator. Bridged from
    /// `CallOrchestratorEvent::IncomingPresented` so the desktop
    /// notification listener (or any other domain-bus subscriber) can
    /// surface an urgent toast even when the in-app call modal isn't
    /// open. `peer_name` is the orchestrator's best-effort display name
    /// (user-supplied or falls back to peer_id).
    IncomingCall {
        call_id: String,
        peer_id: UserId,
        peer_name: String,
        is_video: bool,
        /// Group channel context when the call is rung into a channel,
        /// None for 1:1 DM calls.
        channel_context: Option<String>,
    },

    /// Call hangup received from peer (remote call ended).
    CallHangupReceived {
        call_id: String,
        peer_id: UserId,
        reason: String,
    },
    /// Call key answer received (callee's ephemeral key for PFS).
    CallKeyAnswerReceived {
        call_id: String,
        peer_id: UserId,
        /// Callee's ephemeral X25519 public key (32 bytes).
        ephemeral_public_key: [u8; 32],
    },
    /// Group call key received (distributed via pairwise sealed sender).
    GroupCallKeyReceived {
        call_id: String,
        sender_id: UserId,
        /// AES-256 encryption key (32 bytes).
        key_material: [u8; 32],
        /// Key epoch (increments on rotation).
        key_epoch: u32,
        channel_id: ChannelId,
    },

    // ========================================================================
    // Anonymous Group Events (NDSS'21 Style)
    // ========================================================================

    AnonGroupInviteReceived {
        group_id: AnonGroupId,
        group_name: String,
        inviter_id: UserId,
        inviter_mailbox_pk: [u8; 32],
        profile_key: Option<[u8; 32]>,
        inviter_display_name: Option<String>,
        inviter_avatar: Option<Vec<u8>>,
    },

    AnonGroupJoined {
        group_id: AnonGroupId,
    },

    AnonGroupMemberJoined {
        group_id: AnonGroupId,
        user_id: UserId,
        profile_key: Option<[u8; 32]>,
        display_name: Option<String>,
        avatar: Option<Vec<u8>>,
    },

    AnonGroupMemberLeft {
        group_id: AnonGroupId,
        user_id: UserId,
    },

    AnonGroupMemberKicked {
        group_id: AnonGroupId,
        user_id: UserId,
        /// Admin who performed the kick.
        kicked_by: UserId,
    },

    AnonGroupBootstrapComplete {
        group_id: AnonGroupId,
    },

    AnonGroupMemberListReceived {
        group_id: AnonGroupId,
        members: Vec<crate::proto::content_v1::AnonGroupMemberInfo>,
    },

    // ========================================================================
    // Anonymous DM Events (Client-Side Managed)
    // ========================================================================

    AnonDmRequestReceived {
        conversation_id: AnonDmId,
        sender_id: UserId,
        sender_mailbox_pk: [u8; 32],
        intro_message: Option<String>,
        profile_key: Option<[u8; 32]>,
        sender_display_name: Option<String>,
        sender_avatar: Option<Vec<u8>>,
    },

    AnonDmRequestAccepted {
        conversation_id: AnonDmId,
        peer_id: UserId,
        peer_mailbox_pk: [u8; 32],
        profile_key: Option<[u8; 32]>,
        peer_display_name: String,
        peer_avatar: Option<Vec<u8>>,
    },

    UserPresenceChanged {
        user_id: UserId,
        presence: crate::domain::user::PresenceStatus,
        timestamp: u64,
    },

    AnonDmProfileUpdated {
        conversation_id: crate::domain::anon_dm::AnonDmId,
        user_id: UserId,
        display_name: Option<String>,
        avatar: Option<Vec<u8>>,
        avatar_url: Option<String>,
        bio: Option<String>,
        timestamp: u64,
    },

    /// Server-managed user profile updated (display_name, avatar, status_message,
    /// presence). Emitted from the user events stream after the local user_repo
    /// has been upserted with the new profile.
    UserProfileUpdated {
        user_id: UserId,
        display_name: String,
        status_message: Option<String>,
        /// Decrypted avatar bytes (raw image), if the profile carries one and
        /// we hold the peer's profile key. Lets the UI patch the sidebar/header
        /// avatar in-place on this event, same as it already does for the name.
        avatar_bytes: Option<Vec<u8>>,
        timestamp: u64,
    },

    /// NDSS'21 ECIES init message received (private friend contact).
    AnonDmInitReceived {
        conversation_id: AnonDmId,
        sender_id: UserId,
        sender_display_name: String,
        sender_avatar: Option<Vec<u8>>,
        peer_mailbox_pk: [u8; 32],
        /// Initiator's Signal identity key (33 bytes with 0x05 prefix).
        peer_identity_key: [u8; 33],
        /// Initiator's delivery token for sealed sender.
        peer_delivery_token: [u8; 12],
        intro_message: String,
        /// Server timestamp (for ordering).
        server_timestamp: u64,
    },

    AnonCallOfferReceived {
        conversation_id: AnonDmId,
        sender_id: UserId,
        call_room_id: Vec<u8>,
        room_name: String,
        /// Call type (voice = 1, video = 2).
        call_type: i32,
        caller_display_name: String,
        /// E2EE encryption key (32 bytes).
        call_encryption_key: Vec<u8>,
        timestamp: u64,
    },

    AnonCallAnswerReceived {
        conversation_id: AnonDmId,
        sender_id: UserId,
        call_room_id: Vec<u8>,
        accepted: bool,
        callee_display_name: String,
        timestamp: u64,
    },

    AnonCallHangupReceived {
        conversation_id: AnonDmId,
        sender_id: UserId,
        call_room_id: Vec<u8>,
        reason: i32,
        timestamp: u64,
    },

    AnonGroupCallOfferReceived {
        group_id: AnonGroupId,
        sender_id: UserId,
        call_room_id: Vec<u8>,
        room_name: String,
        call_type: i32,
        caller_display_name: String,
        call_encryption_key: Vec<u8>,
        timestamp: u64,
    },

    AnonGroupCallAnswerReceived {
        group_id: AnonGroupId,
        sender_id: UserId,
        call_room_id: Vec<u8>,
        accepted: bool,
        callee_display_name: String,
        timestamp: u64,
    },

    AnonGroupCallHangupReceived {
        group_id: AnonGroupId,
        sender_id: UserId,
        call_room_id: Vec<u8>,
        reason: i32,
        timestamp: u64,
    },

    // ========================================================================
    // UI-Specific Events
    // ========================================================================

    /// Call key offer received (incoming call with ephemeral key for PFS).
    CallKeyOfferReceived {
        call_id: String,
        peer_id: UserId,
        /// Caller's ephemeral X25519 public key (32 bytes).
        ephemeral_public_key: [u8; 32],
        /// Call type: "Voice Call" or "Video Call".
        call_type: String,
        /// DM channel ID for context.
        channel_id: String,
        /// Whether caller's identity is verified (known identity key).
        is_identity_verified: bool,
        identity_warning: String,
        caller_name: String,
        timestamp: u64,
    },

    /// DM bootstrap completed — shared secret established with peer.
    /// UI should enable composer and flush pending messages.
    DmBootstrapCompleted {
        peer_id: UserId,
    },

    UnreadCountUpdated {
        channel_id: ChannelId,
        peer_id: Option<UserId>,
        unread_count: u32,
    },

    FriendRequestsUpdated {
        received: Vec<FriendRequestInfo>,
        sent: Vec<FriendRequestInfo>,
    },

    /// Friend request accepted. DMs are channel-free (Signal-style) — the
    /// local DM conversation is keyed by `friend_id` directly.
    FriendAccepted {
        friend_id: UserId,
    },

    /// Anonymous group metadata updated (name, description, roles).
    AnonGroupUpdated {
        group_id: AnonGroupId,
        update: crate::proto::content_v1::AnonGroupUpdate,
        updated_by: UserId,
    },

    // ========================================================================
    // Local Send Events
    // ========================================================================

    MessageSent {
        message_id: MessageId,
        channel_id: ChannelId,
        sender_id: UserId,
        content: String,
        sent_at: chrono::DateTime<chrono::Utc>,
    },

    MessageSendFailed {
        channel_id: ChannelId,
        error: String,
    },

    ChannelMemberJoined {
        channel_id: ChannelId,
        user_id: UserId,
        display_name: String,
        /// System message created for this event.
        message: Message,
    },

    ChannelMemberLeft {
        channel_id: ChannelId,
        user_id: UserId,
        display_name: String,
        message: Message,
    },

    // ========================================================================
    // Jarvis AI Events
    // ========================================================================

    JarvisResponse {
        text: String,
        context_id: String,
    },

    JarvisStats {
        stats: String,
    },

    JarvisCleared {
        context_id: String,
    },

    // ========================================================================
    // Sync Events
    // ========================================================================

    /// Background sync cycle completed.
    SyncComplete {
        channels_synced: usize,
        messages_synced: usize,
        duration_ms: u64,
    },

    /// Self-hosted ntfy push wake-up received.
    ///
    /// Fires whenever the `PushListener` receives an SSE `message` event from
    /// the configured ntfy instance. The core uses this to trigger a one-shot
    /// mailbox sync so any new messages are pulled; the UI can also react
    /// (e.g., to animate an incoming-message indicator). The variant is
    /// deliberately narrow — only the topic and privacy kind are exposed so
    /// no server-supplied body text can leak into domain-logic code paths.
    PushWakeup {
        /// The ntfy topic the wake-up arrived on (already HMAC-obfuscated
        /// by the server — purely opaque to the client).
        topic: String,
        /// Which privacy mode the backend is running in.
        kind: PushKind,
    },
}

/// Privacy mode of a push notification received from self-hosted ntfy.
///
/// Mirrors the server's `NTFY_PUSH_MODE` setting (see
/// `internal/services/notification/push/ntfy.go`). Derived from the message
/// body: in `content_free` mode the backend always sends the literal string
/// `"New activity"`; anything else implies `full` mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushKind {
    /// Server is in content-free mode: wake-up only, no content leaks to ntfy.
    ContentFree,
    /// Server is in full mode: title/body contain sender + preview.
    Full,
}

impl DomainEvent {
    /// Get the channel ID associated with this event, if any.
    pub fn channel_id(&self) -> Option<ChannelId> {
        match self {
            DomainEvent::MessageReceived { message } => Some(message.conversation_id),
            DomainEvent::MessageEdited { channel_id, .. } => Some(*channel_id),
            DomainEvent::MessageDeleted { channel_id, .. } => *channel_id,
            DomainEvent::AttachmentReady { channel_id, .. } => Some(*channel_id),
            DomainEvent::TypingStarted { channel_id, .. } => Some(*channel_id),
            DomainEvent::TypingStopped { channel_id, .. } => Some(*channel_id),
            DomainEvent::MessagesRead { channel_id, .. } => *channel_id,
            DomainEvent::MessagesDelivered { .. } => None,
            DomainEvent::ViewedReceipt { .. } => None,
            DomainEvent::ReactionAdded { .. } => None,
            DomainEvent::ReactionRemoved { .. } => None,
            DomainEvent::MessagePinned { channel_id, .. } => Some(*channel_id),
            DomainEvent::MessageUnpinned { channel_id, .. } => Some(*channel_id),
            DomainEvent::KeyExchangeReceived { conversation_id, .. } => Some(*conversation_id),
            DomainEvent::SenderKeyReceived { channel_id, .. } => Some(*channel_id),
            DomainEvent::GroupMailboxReceived { channel_id, .. } => Some(*channel_id),
            DomainEvent::DeleteForMeSync { conversation_id, .. } => Some(*conversation_id),
            DomainEvent::SavedMessageSynced { message } => Some(message.conversation_id),
            DomainEvent::DeviceLinked { .. } => None,
            DomainEvent::IncomingCall { .. } => None,
            DomainEvent::CallHangupReceived { .. } => None,
            DomainEvent::CallKeyAnswerReceived { .. } => None,
            DomainEvent::GroupCallKeyReceived { channel_id, .. } => Some(*channel_id),
            DomainEvent::AnonGroupInviteReceived { .. } => None,
            DomainEvent::AnonGroupJoined { .. } => None,
            DomainEvent::AnonGroupMemberJoined { .. } => None,
            DomainEvent::AnonGroupMemberLeft { .. } => None,
            DomainEvent::AnonGroupMemberKicked { .. } => None,
            DomainEvent::AnonGroupBootstrapComplete { .. } => None,
            DomainEvent::AnonGroupMemberListReceived { .. } => None,
            DomainEvent::AnonDmRequestReceived { .. } => None,
            DomainEvent::AnonDmRequestAccepted { .. } => None,
            DomainEvent::AnonDmInitReceived { .. } => None,
            DomainEvent::AnonCallOfferReceived { .. } => None,
            DomainEvent::AnonCallAnswerReceived { .. } => None,
            DomainEvent::AnonCallHangupReceived { .. } => None,
            DomainEvent::AnonGroupCallOfferReceived { .. } => None,
            DomainEvent::AnonGroupCallAnswerReceived { .. } => None,
            DomainEvent::AnonGroupCallHangupReceived { .. } => None,
            DomainEvent::CallKeyOfferReceived { .. } => None,
            DomainEvent::DmBootstrapCompleted { .. } => None,
            DomainEvent::UnreadCountUpdated { channel_id, .. } => Some(*channel_id),
            DomainEvent::FriendRequestsUpdated { .. } => None,
            DomainEvent::FriendAccepted { .. } => None,
            DomainEvent::AnonGroupUpdated { .. } => None,
            DomainEvent::MessageSent { channel_id, .. } => Some(*channel_id),
            DomainEvent::MessageSendFailed { channel_id, .. } => Some(*channel_id),
            DomainEvent::ChannelMemberJoined { channel_id, .. } => Some(*channel_id),
            DomainEvent::ChannelMemberLeft { channel_id, .. } => Some(*channel_id),
            DomainEvent::UserPresenceChanged { .. } => None,
            DomainEvent::AnonDmProfileUpdated { .. } => None,
            DomainEvent::UserProfileUpdated { .. } => None,
            DomainEvent::JarvisResponse { .. } => None,
            DomainEvent::JarvisStats { .. } => None,
            DomainEvent::JarvisCleared { .. } => None,
            DomainEvent::SyncComplete { .. } => None,
            DomainEvent::PushWakeup { .. } => None,
            DomainEvent::SessionResetRequested { .. } => None,
            DomainEvent::PeerDevicesChanged { .. } => None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::message::ConversationType;

    #[test]
    fn test_domain_event_is_transient() {
        let channel_id = ChannelId::new();
        let user_id = UserId::new();

        let typing = DomainEvent::TypingStarted { user_id, channel_id };
        assert!(typing.is_transient());

        let msg = DomainEvent::MessageReceived {
            message: Message::new_text(channel_id, ConversationType::Channel, Some(user_id), "test".to_string(), 0),
        };
        assert!(!msg.is_transient());
    }

    #[test]
    fn test_domain_event_channel_id() {
        let channel_id = ChannelId::new();
        let user_id = UserId::new();

        let msg = DomainEvent::MessageReceived {
            message: Message::new_text(channel_id, ConversationType::Channel, Some(user_id), "test".to_string(), 0),
        };
        assert_eq!(msg.channel_id(), Some(channel_id));

        let read = DomainEvent::MessagesRead {
            reader_id: user_id,
            message_timestamps: vec![123],
            receipt_timestamp: 456,
            channel_id: Some(channel_id),
        };
        assert_eq!(read.channel_id(), Some(channel_id));

        let message_id = MessageId::new();
        let delete_sync = DomainEvent::DeleteForMeSync {
            conversation_id: channel_id,
            target_author_id: user_id,
            target_message_id: message_id,
            timestamp: 1704067200000,
        };
        assert_eq!(delete_sync.channel_id(), Some(channel_id));
        assert!(!delete_sync.is_transient());
        assert!(!delete_sync.is_message_event());
    }
}
