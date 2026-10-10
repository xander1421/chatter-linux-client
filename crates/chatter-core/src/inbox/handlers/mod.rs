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

use crate::domain::anon_dm::AnonDmId;
use crate::domain::anon_group::AnonGroupId;
use crate::domain::message::ConversationType;
use crate::domain::{Attachment, ChannelId, Message, MessageContent, MessageId, UserId};
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
        /// Author of the edited message (only an author can edit). With
        /// `target_sent_timestamp` this is the Signal-style message key.
        author_id: UserId,
        /// The edited message's original sent timestamp.
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
        /// Sender's clock (TypingMessage.timestamp, ns); 0 = none. Orders
        /// this against a later-sent STOPPED — see `ChatState::accept_typing`.
        timestamp: u64,
    },
    /// Typing indicator stopped (a STOPPED, or a message from the sender).
    TypingStopped {
        user_id: UserId,
        channel_id: ChannelId,
        /// Sender's clock (TypingMessage.timestamp or message sent_at, ns).
        timestamp: u64,
    },
    /// Persisted aggregate receipt state changed for one outgoing message.
    MessageDeliveryChanged {
        message_id: MessageId,
        channel_id: ChannelId,
        status: String,
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
        scope: crate::domain::reaction::ReactionScope,
        /// Target message sender (target_author_aci in Signal protocol).
        sender_id: UserId,
        /// Target message sent timestamp in nanoseconds (target_sent_timestamp).
        sent_at_ns: u64,
        reactor_id: UserId,
        emoji: String,
        /// When the reaction was sent (ms since epoch).
        timestamp: u64,
    },
    /// Complete persisted reaction snapshot changed; carries no invented delta.
    ReactionSnapshotChanged {
        scope: crate::domain::reaction::ReactionScope,
        sender_id: UserId,
        sent_at_ns: u64,
    },
    /// Reaction removed from message.
    ReactionRemoved {
        scope: crate::domain::reaction::ReactionScope,
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
        /// Storage kind of the pinned row. A view takes only the pin events
        /// of its own kind (`SavedMessages` rows share their id with no
        /// channel or DM view, and the reverse).
        conversation_type: ConversationType,
        /// Original sent timestamp (nanoseconds) for UI matching.
        target_sent_timestamp: u64,
    },
    /// Message was unpinned.
    MessageUnpinned {
        message_id: MessageId,
        unpinner_id: UserId,
        channel_id: ChannelId,
        conversation_type: ConversationType,
        target_sent_timestamp: u64,
    },
    /// Key exchange received (mailbox DM encryption setup).
    ///
    /// Key can be 32 bytes (regular key exchange) or a 96-byte
    /// `MailboxAnnouncement` (DM bootstrap).
    /// Bootstrap format: mailbox pk || conversation ephemeral || mailbox encryption pk.
    KeyExchangeReceived {
        sender_id: UserId,
        /// The sending device — where the bootstrap message / reply goes.
        sender_device_id: u32,
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
        /// Exact own Saved deletion was committed during admission.
        saved_deletion_persisted: bool,
    },
    /// Messages in these conversations were read — here
    /// (`mark_conversation_read`) or on another of our devices (`ReadSync`,
    /// `from_other_device`). Their unread counts dropped and their
    /// notifications are stale.
    ConversationsRead {
        conversation_ids: Vec<ChannelId>,
        from_other_device: bool,
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

    /// SENDER half of the 1:1 session self-heal completed: a peer's
    /// `DecryptionErrorMessage` was processed for `(peer_user_id,
    /// peer_device_id)`. `archived` is true when the broken session was
    /// archived (next send re-establishes via PreKeySignalMessage); false
    /// when the report was stale or no session existed. Emitted for
    /// observability (logs/tests) — the archive already happened.
    DmSessionHealed {
        peer_user_id: UserId,
        peer_device_id: u32,
        archived: bool,
    },

    /// A peer presented a DIFFERENT identity key than the one pinned for their
    /// account (a reinstall — or a MITM), and the change has been parked for an
    /// explicit user decision. The conversation is wedged in BOTH directions
    /// until it is accepted, so this exists to let an open conversation raise
    /// the prompt without waiting for a reload.
    ///
    /// `attributed` is
    /// [`crate::error::IdentityChangeSource::is_authenticated`]: false means
    /// `peer_user_id` is server-supplied envelope metadata that nothing proved,
    /// and the UI must not name the peer as a fact.
    PeerIdentityChanged {
        peer_user_id: UserId,
        attributed: bool,
    },

    /// A peer who shares a channel with the local user added, revoked or
    /// re-registered a device. Handled by
    /// `MessageService::handle_peer_device_change`.
    PeerDevicesChanged {
        peer_user_id: UserId,
        /// The device that was added or revoked.
        device_id: u32,
        /// `"added"`, `"revoked"` or `"reregistered"`.
        change_type: String,
    },

    /// Call hangup received from peer (remote call ended).
    CallHangupReceived {
        call_id: String,
        peer_id: UserId,
        reason: crate::services::gateway::CallHangupReason,
    },
    /// Call key answer received (callee's ephemeral key for PFS).
    CallKeyAnswerReceived {
        call_id: String,
        peer_id: UserId,
        /// Callee's ephemeral X25519 public key (32 bytes).
        ephemeral_public_key: [u8; 32],
    },
    /// Channel group-call signal (pairwise sealed sender): ring, key, or
    /// key request. Handled by the call key policy (`client/mod.rs`).
    GroupCallSignalReceived {
        call_id: String,
        sender_id: UserId,
        channel_id: ChannelId,
        signal: crate::domain::call::GroupCallSignal,
    },

    // ========================================================================
    // Anonymous Group Events (NDSS'21 Style)
    // ========================================================================
    AnonGroupInviteReceived {
        group_id: AnonGroupId,
        group_name: String,
        inviter_id: UserId,
        /// The inviter's device the invite came from; the group binds the
        /// inviter (owner) to it.
        inviter_device_id: u32,
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
        /// Initiator's device (owns `peer_mailbox_pk`); the chat binds to it.
        sender_device_id: u32,
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
        /// Whether caller's identity is verified (known identity key).
        is_identity_verified: bool,
        identity_warning: String,
        caller_name: String,
        timestamp: u64,
    },

    /// DM bootstrap completed and the queued messages in `sent_timestamps`
    /// went out (the post-bootstrap flush, either side). What did not go out
    /// is reported by `MessageSendFailed`.
    DmBootstrapCompleted {
        peer_id: UserId,
        /// `DataMessage.timestamp` (= the bubble's `sent_at`) of each sent one.
        sent_timestamps: Vec<u64>,
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
    /// A send that finished in the background (the post-bootstrap flush)
    /// failed; the user has no other way to learn the message did not go.
    MessageSendFailed {
        channel_id: ChannelId,
        error: String,
        /// `DataMessage.timestamp` (= the bubble's `sent_at`) of each message
        /// that did not go out — their status becomes `"failed"`.
        message_timestamps: Vec<u64>,
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

    /// Channel metadata (name / description / icon) was changed by an admin.
    /// Fanned out from the server's `ChannelUpdated` event so other members'
    /// UIs live-refresh the channel header instead of showing the stale name.
    ChannelUpdated {
        channel_id: ChannelId,
        name: String,
        icon: Option<Vec<u8>>,
    },

    /// A member's role or mute state changed (promote / demote, mute, unmute,
    /// mute expiry). Fanned out from the server's `MemberUpdated` event so the
    /// member list and the muted member's composer notice update at once.
    ChannelMemberUpdated {
        channel_id: ChannelId,
        user_id: UserId,
        role: crate::domain::channel::MemberRole,
    },

    // ── Channel lifecycle (INFRA-29) ─────────────────────────────────────
    // Every arm of the server's `ChannelEvent` oneof now reaches the UI. The
    // consumer used to handle 4 of 11 and end in `_ => debug!("Unhandled")`,
    // so a deleted channel never disappeared, a ban never surfaced, and — the
    // serious one — `OperationFailed`, the saga-compensation signal, was
    // discarded, leaving optimistic client state wrong forever.
    /// A channel appeared for this user (created by them elsewhere, or an
    /// admin added them). Core has upserted the local row.
    ChannelCreated {
        channel_id: ChannelId,
        name: String,
    },

    /// Archived state changed server-side. Core has updated the local row.
    ChannelArchivedChanged {
        channel_id: ChannelId,
        archived: bool,
    },

    /// A moderation action landed on a channel (kick / ban / mute / unban /
    /// unmute). `is_self` is true when the local user is the target — in that
    /// case core has already dropped the local membership for a kick or ban.
    ChannelModerationAction {
        channel_id: ChannelId,
        user_id: UserId,
        moderator_id: UserId,
        /// Proto action name: "KICK", "BAN", "MUTE", "UNBAN", "UNMUTE".
        action: String,
        reason: Option<String>,
        is_self: bool,
    },

    /// An invitation to a channel arrived for this user.
    ChannelInviteReceived {
        channel_id: ChannelId,
        inviter_id: UserId,
        message: Option<String>,
        /// Unix nanoseconds; 0 when the invite does not expire.
        expires_at: u64,
    },

    /// Someone answered an invitation.
    ChannelInviteResponded {
        channel_id: ChannelId,
        inviter_id: UserId,
        accepted: bool,
    },

    /// An asynchronous channel mutation FAILED server-side and its Temporal
    /// saga compensated. Every channel mutation RPC is a stream precisely so
    /// the server can report this; a client that ignores it keeps optimistic
    /// state the server has already rolled back.
    ///
    /// Core re-reads the channel from the server before emitting, so local
    /// storage already reflects the rollback; the UI's job is to stop showing
    /// the operation as in-flight and tell the user.
    ChannelOperationFailed {
        channel_id: ChannelId,
        /// Correlation id supplied by the server, empty when it sent none.
        operation_id: String,
        /// Proto error code name, e.g. "PERMISSION_DENIED".
        code: String,
        message: String,
    },

    /// Something about a channel's VERIFIED membership the user must see: a
    /// change that failed verification or a fork (sending is blocked), or a
    /// member only the server vouches for (no keys) / a removal only the
    /// server reported. Decided in core; the UI renders it.
    ChannelMembershipNotice {
        channel_id: ChannelId,
        notice: crate::services::channel::membership_log::MembershipNotice,
    },

    // ========================================================================
    // Sync Events
    // ========================================================================
    /// Background sync cycle completed.
    SyncComplete {
        channels_synced: usize,
        messages_synced: usize,
        duration_ms: u64,
        /// The server's block list differed from the local one (a block or
        /// unblock made on another device): every list that hides blocked
        /// people must be rebuilt.
        blocks_changed: bool,
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

    /// The call-history recorder wrote or closed a missed incoming call, so
    /// the Calls tab badge (`ChatterClient::missed_call_count`) may have
    /// moved. Emitted after the row is written: a reader sees it.
    MissedCallsChanged,
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
            DomainEvent::MessageDeliveryChanged { channel_id, .. } => Some(*channel_id),
            DomainEvent::MessagesRead { channel_id, .. } => *channel_id,
            DomainEvent::MessagesDelivered { .. } => None,
            DomainEvent::ViewedReceipt { .. } => None,
            DomainEvent::ReactionAdded { scope, .. } => Some(scope.id),
            DomainEvent::ReactionSnapshotChanged { scope, .. } => Some(scope.id),
            DomainEvent::ReactionRemoved { scope, .. } => Some(scope.id),
            DomainEvent::MessagePinned { channel_id, .. } => Some(*channel_id),
            DomainEvent::MessageUnpinned { channel_id, .. } => Some(*channel_id),
            DomainEvent::KeyExchangeReceived {
                conversation_id, ..
            } => Some(*conversation_id),
            DomainEvent::SenderKeyReceived { channel_id, .. } => Some(*channel_id),
            DomainEvent::GroupMailboxReceived { channel_id, .. } => Some(*channel_id),
            DomainEvent::DeleteForMeSync {
                conversation_id, ..
            } => Some(*conversation_id),
            DomainEvent::SavedMessageSynced { message } => Some(message.conversation_id),
            DomainEvent::ConversationsRead { .. } => None,
            DomainEvent::DeviceLinked { .. } => None,
            DomainEvent::CallHangupReceived { .. } => None,
            DomainEvent::CallKeyAnswerReceived { .. } => None,
            DomainEvent::GroupCallSignalReceived { channel_id, .. } => Some(*channel_id),
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
            DomainEvent::CallKeyOfferReceived { .. } => None,
            DomainEvent::DmBootstrapCompleted { .. } => None,
            DomainEvent::FriendRequestsUpdated { .. } => None,
            DomainEvent::FriendAccepted { .. } => None,
            DomainEvent::AnonGroupUpdated { .. } => None,
            DomainEvent::MessageSendFailed { channel_id, .. } => Some(*channel_id),
            DomainEvent::ChannelMemberJoined { channel_id, .. } => Some(*channel_id),
            DomainEvent::ChannelMemberLeft { channel_id, .. } => Some(*channel_id),
            DomainEvent::ChannelUpdated { channel_id, .. } => Some(*channel_id),
            DomainEvent::ChannelMemberUpdated { channel_id, .. } => Some(*channel_id),
            DomainEvent::ChannelCreated { channel_id, .. } => Some(*channel_id),
            DomainEvent::ChannelArchivedChanged { channel_id, .. } => Some(*channel_id),
            DomainEvent::ChannelModerationAction { channel_id, .. } => Some(*channel_id),
            DomainEvent::ChannelInviteReceived { channel_id, .. } => Some(*channel_id),
            DomainEvent::ChannelInviteResponded { channel_id, .. } => Some(*channel_id),
            DomainEvent::ChannelOperationFailed { channel_id, .. } => Some(*channel_id),
            DomainEvent::ChannelMembershipNotice { channel_id, .. } => Some(*channel_id),
            DomainEvent::UserPresenceChanged { .. } => None,
            DomainEvent::AnonDmProfileUpdated { .. } => None,
            DomainEvent::UserProfileUpdated { .. } => None,
            DomainEvent::SyncComplete { .. } => None,
            DomainEvent::PushWakeup { .. } => None,
            DomainEvent::MissedCallsChanged => None,
            DomainEvent::DmSessionHealed { .. } => None,
            DomainEvent::PeerIdentityChanged { .. } => None,
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

        let typing = DomainEvent::TypingStarted {
            user_id,
            channel_id,
            timestamp: 0,
        };
        assert!(typing.is_transient());

        let msg = DomainEvent::MessageReceived {
            message: Message::new_text(
                channel_id,
                ConversationType::Channel,
                Some(user_id),
                "test".to_string(),
                0,
            ),
        };
        assert!(!msg.is_transient());
    }

    #[test]
    fn test_domain_event_channel_id() {
        let channel_id = ChannelId::new();
        let user_id = UserId::new();

        let msg = DomainEvent::MessageReceived {
            message: Message::new_text(
                channel_id,
                ConversationType::Channel,
                Some(user_id),
                "test".to_string(),
                0,
            ),
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
            saved_deletion_persisted: false,
        };
        assert_eq!(delete_sync.channel_id(), Some(channel_id));
        assert!(!delete_sync.is_transient());
        assert!(!delete_sync.is_message_event());
    }
}
