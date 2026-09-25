//! Message domain entity.
//!
//! Represents chat messages, threads, and message content.

use crate::domain::ids::{AttachmentId, ChannelId, IntegrationId, MessageId, UserId};
use crate::error::StorageError;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

// ============================================================================
// ConversationType
// ============================================================================

/// Discriminant for the type of conversation a message belongs to.
///
/// Used in the unified `messages` table to distinguish between channel messages,
/// direct messages, anonymous DMs, anonymous groups, etc.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConversationType {
    /// Server-managed channel (public, private, or group).
    Channel,
    /// Friend DM (1:1, channel-free, keyed by the peer). One conversation
    /// whichever transport carried a message — sealed-sender streaming or,
    /// in Shield mode, the NDSS'21 mailbox.
    ServerDm,
    /// Anonymous DM (NDSS'21 style, client-managed).
    AnonDm,
    /// Anonymous group (NDSS'21 style, client-managed).
    AnonGroup,
    /// Saved messages / personal vault.
    SavedMessages,
    /// Public bitchat mesh room, or a geohash channel. Client-managed and
    /// serverless: these messages never reach the server, so no server-side
    /// authorization applies to them.
    ///
    /// The transport scope lives in the conversation id rather than here —
    /// `PeerId` is already namespaced (`nostr:` for geohash chat, plain hex for
    /// the BLE mesh room), and a second variant would give it two sources of
    /// truth.
    Mesh,
    /// One-to-one bitchat conversation, end-to-end encrypted over the mesh
    /// (Noise) or over Nostr (gift-wrapped). Also serverless.
    MeshDm,
}

impl ConversationType {
    /// Convert to database string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Channel => "channel",
            Self::ServerDm => "server_dm",
            Self::AnonDm => "anon_dm",
            Self::AnonGroup => "anon_group",
            Self::SavedMessages => "saved_messages",
            Self::Mesh => "mesh",
            Self::MeshDm => "mesh_dm",
        }
    }

    /// Serverless conversations carried by the bitchat mesh.
    ///
    /// These never make a server RPC, so nothing about them is authorized
    /// server-side. Anything keyed on "the server knows about this conversation"
    /// must exclude them.
    pub fn is_mesh(&self) -> bool {
        matches!(self, Self::Mesh | Self::MeshDm)
    }

    /// Whether an inbound message in this conversation should create a DM
    /// sidebar entry for a new peer. Only the friend DM does.
    pub fn creates_dm_sidebar_entry(&self) -> bool {
        matches!(self, Self::ServerDm)
    }

    /// Anonymous conversations (NDSS'21 style, client-managed). Replies here
    /// carry a self-contained quote snapshot and reference their parent via a
    /// synthetic timestamp-derived `MessageId` that never matches the parent's
    /// random stored id — so parent-by-id lookups (e.g. the "Original message
    /// not found" check) are meaningless and must be skipped.
    pub fn is_anon(&self) -> bool {
        matches!(self, Self::AnonDm | Self::AnonGroup)
    }

    /// Parse from database string representation.
    ///
    /// Returns an error for unknown conversation types instead of silently
    /// falling back to `Channel`.
    pub fn from_str_value(s: &str) -> Result<Self, StorageError> {
        match s {
            "channel" => Ok(Self::Channel),
            "server_dm" => Ok(Self::ServerDm),
            "anon_dm" => Ok(Self::AnonDm),
            "anon_group" => Ok(Self::AnonGroup),
            "saved_messages" => Ok(Self::SavedMessages),
            "mesh" => Ok(Self::Mesh),
            "mesh_dm" => Ok(Self::MeshDm),
            _ => Err(StorageError::InvalidData(format!(
                "Unknown conversation type: {s}"
            ))),
        }
    }
}

impl Default for ConversationType {
    fn default() -> Self {
        Self::Channel
    }
}

impl std::fmt::Display for ConversationType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ============================================================================
// UnreadAnchor
// ============================================================================

/// Where the user left off in a conversation.
///
/// Opening a conversation clears `is_read` for the whole conversation, which
/// destroys the boundary — so this is captured *inside*
/// `UnifiedMessageRepository::mark_conversation_read`, in the same call, before
/// the `UPDATE`. There is deliberately no way to ask for it afterwards: that
/// ordering bug is what makes "open where I left off" quietly degrade to "open
/// at the bottom" and nobody notices.
///
/// Our own sent messages land `is_read = 1` (`insert_outgoing`), so the
/// boundary always points at a message somebody else sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UnreadAnchor {
    /// `sent_at` of the oldest unread message. `None` when nothing was unread —
    /// the common case, and the caller should open at the newest message.
    pub first_unread_sent_at: Option<u64>,
    /// How many unread messages the conversation held. May exceed one page.
    pub unread_count: u32,
}

impl UnreadAnchor {
    /// Index of the unread divider in an ascending (oldest-first) page.
    ///
    /// The divider sits *above* the first message at or after the boundary, so
    /// the returned index is the row the viewport should open on. `None` means
    /// open at the newest message: either nothing was unread, or the page does
    /// not reach back far enough to contain the boundary (every loaded row is
    /// older than it, which cannot happen for a newest-N page but is handled
    /// rather than panicked on).
    ///
    /// Row 0 is a legitimate answer — a conversation whose entire loaded page
    /// is unread opens at the top with the divider above the first row.
    #[must_use]
    pub fn divider_index(&self, page: &[Message]) -> Option<usize> {
        let boundary = self.first_unread_sent_at?;
        page.iter().position(|m| m.sent_at >= boundary)
    }
}

// ============================================================================
// Message
// ============================================================================

/// Represents a chat message in the unified message pipeline.
///
/// All message types (channel, DM, anon DM, anon group) use this struct.
/// The `conversation_type` field discriminates what kind of conversation
/// the message belongs to, and `conversation_id` holds the conversation UUID
/// (a ChannelId for channels, or a converted AnonDmId/AnonGroupId for
/// anonymous conversations — they share the same UUID representation).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    /// The conversation this message belongs to.
    /// For channels: the channel UUID. For anon DM/group: the AnonDmId/AnonGroupId
    /// converted to ChannelId (they share the same UUID representation).
    pub conversation_id: ChannelId,
    /// What type of conversation this message is in.
    pub conversation_type: ConversationType,
    /// None for sealed sender messages (server doesn't know sender).
    pub sender_id: Option<UserId>,
    /// Signal device ID of the sender (for multi-device).
    pub device_id: Option<u32>,
    pub content: MessageContent,
    /// Raw encrypted content blob (separate from content_json for efficient storage).
    pub encrypted_content: Option<Vec<u8>>,
    /// Parent message ID for threads.
    pub thread_id: Option<MessageId>,
    /// Message being replied to (quote).
    pub reply_to: Option<MessageId>,
    /// Quote context: author of the quoted message.
    #[serde(default)]
    pub reply_to_author_id: Option<UserId>,
    /// Quote context: preview text of the quoted message.
    #[serde(default)]
    pub reply_to_text: Option<String>,
    /// Quote context: JPEG/PNG thumbnail bytes of the first image/video
    /// attachment in the quoted message. Carried over the wire inside
    /// `Quote.attachments[0].thumbnail` (Signal-compatible).
    #[serde(default)]
    pub reply_to_thumbnail: Option<Vec<u8>>,
    /// Quote context: MIME type of the quoted attachment, e.g. "image/jpeg".
    #[serde(default)]
    pub reply_to_thumbnail_mime: Option<String>,
    pub attachments: Vec<AttachmentId>,
    pub is_pinned: bool,
    pub is_edited: bool,
    pub is_deleted: bool,
    /// Whether this message has been read by the local user.
    pub is_read: bool,
    /// Sender's timestamp (used for Signal-style deduplication with sender_id).
    /// This is when the sender created the message, not when we received it.
    pub sent_at: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub edited_at: Option<DateTime<Utc>>,
    /// Reaction counts by emoji.
    pub reaction_counts: Vec<ReactionCount>,
    /// Current user's reactions (for UI state).
    pub current_user_reactions: Vec<String>,
    /// AI integrations mentioned in message.
    pub mentioned_integrations: Vec<IntegrationId>,
    pub targets_ai_bot: bool,
    /// Delivery status: "", "sent", "delivered", "read".
    pub delivery_status: String,
}

impl Message {
    /// Create a new text message with the given channel, sender, text, and sender's timestamp.
    ///
    /// The `sent_at` parameter is the sender's timestamp (nanoseconds since epoch),
    /// used for Signal-style deduplication with `(sender_id, sent_at)`.
    /// `created_at` is derived from `sent_at` - they represent the same moment.
    #[must_use]
    pub fn new_text(
        conversation_id: ChannelId,
        conversation_type: ConversationType,
        sender_id: Option<UserId>,
        text: String,
        sent_at: u64,
    ) -> Self {
        // Derive created_at from sent_at (they're the same moment)
        let created_at = crate::util::time::TimestampNanos::from_nanos(sent_at)
            .to_datetime()
            .unwrap_or_else(Utc::now);
        tracing::debug!(
            sent_at_nanos = sent_at,
            created_at = %created_at,
            "Message::new_text - timestamp conversion"
        );
        Self {
            id: MessageId::new(),
            conversation_id,
            conversation_type,
            sender_id,
            device_id: None,
            content: MessageContent::Text(text),
            encrypted_content: None,
            thread_id: None,
            reply_to: None,
            reply_to_author_id: None,
            reply_to_text: None,
            reply_to_thumbnail: None,
            reply_to_thumbnail_mime: None,
            attachments: Vec::new(),
            is_pinned: false,
            is_edited: false,
            is_deleted: false,
            is_read: false,
            sent_at,
            created_at,
            updated_at: created_at,
            edited_at: None,
            reaction_counts: Vec::new(),
            current_user_reactions: Vec::new(),
            mentioned_integrations: Vec::new(),
            targets_ai_bot: false,
            delivery_status: String::new(),
        }
    }

    /// Check if this message is the root of a thread.
    /// A message is a thread root if it has no thread_id (not a reply) but has replies.
    /// Note: This requires external knowledge of reply count.
    #[must_use]
    pub fn is_thread_root(&self, has_replies: bool) -> bool {
        self.thread_id.is_none() && has_replies
    }

    /// Check if this message is a reply in a thread.
    #[must_use]
    pub fn is_thread_reply(&self) -> bool {
        self.thread_id.is_some()
    }

    /// Extract text content if this is a Text or Mixed variant.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match &self.content {
            MessageContent::Text(text) => Some(text),
            MessageContent::Mixed(mixed) => mixed.text.as_deref(),
            MessageContent::AI(ai) => Some(&ai.text),
            _ => None,
        }
    }

    /// Check if the message has any attachments.
    #[must_use]
    pub fn has_attachments(&self) -> bool {
        !self.attachments.is_empty()
    }

    /// Mark the message as edited.
    pub fn mark_edited(&mut self) {
        self.is_edited = true;
        self.edited_at = Some(Utc::now());
        self.updated_at = Utc::now();
    }

    /// Mark the message as deleted (soft delete).
    pub fn mark_deleted(&mut self) {
        self.is_deleted = true;
        self.updated_at = Utc::now();
    }

    /// Check if this is a sealed sender (anonymous) message.
    #[must_use]
    pub fn is_sealed_sender(&self) -> bool {
        matches!(self.content, MessageContent::SealedSender(_))
    }

    /// Get the total reaction count.
    #[must_use]
    pub fn total_reactions(&self) -> u32 {
        self.reaction_counts.iter().map(|r| r.count).sum()
    }

    /// Compose a one-line sidebar preview for this message.
    ///
    /// - Text messages (emoji-only included — an emoji IS the text) are
    ///   truncated to 50 chars.
    /// - Empty-body messages take `attachment_label` when the caller resolved
    ///   one ("🖼️ Photo", "📎 report.pdf"), else "📎 N attachment(s)".
    /// - Empty-body messages with no attachments render as "[Message]".
    /// - If `current_user_id` is provided and matches this message's sender,
    ///   the preview is prefixed with "You: " (text messages only).
    /// - A pinned message carries a 📌 marker, so the row says *why* this is
    ///   the last thing that happened here.
    ///
    /// Attachment names and MIME types live in storage, not on the message, so
    /// the typed label is passed in — `ChatterClient::message_sidebar_preview`
    /// is the one caller that resolves it.
    ///
    /// Lives in core because "how do we describe a message in the sidebar"
    /// is a domain decision shared by every UI surface, not a presentation
    /// concern owned by Slint.
    #[must_use]
    pub fn sidebar_preview(
        &self,
        current_user_id: Option<&crate::domain::ids::UserId>,
        attachment_label: Option<&str>,
    ) -> String {
        let is_own = matches!(
            (current_user_id, self.sender_id),
            (Some(u), Some(s)) if *u == s
        );
        let pin = if self.is_pinned { "\u{1F4CC} " } else { "" };
        let text = self.as_text().unwrap_or("");
        let att_count = self.attachments.len();

        if text.is_empty() {
            if let Some(label) = attachment_label {
                return format!("{}{}", pin, label);
            }
            if att_count > 0 {
                let suffix = if att_count == 1 { "" } else { "s" };
                return format!("{}\u{1F4CE} {} attachment{}", pin, att_count, suffix);
            }
            return format!("{}[Message]", pin);
        }
        let truncated = truncate_preview_text(text, 50);
        if is_own {
            format!("{}You: {}", pin, truncated)
        } else {
            format!("{}{}", pin, truncated)
        }
    }
}

/// Truncate text for a single-line preview, cutting to `max` chars and
/// appending `...` when the original overflowed. Counts by chars, not bytes,
/// so emoji and multibyte glyphs are counted correctly.
#[must_use]
pub fn truncate_preview_text(text: &str, max: usize) -> String {
    if text.chars().count() > max {
        // Reserve 3 chars for the ellipsis.
        let head_len = max.saturating_sub(3);
        let truncated: String = text.chars().take(head_len).collect();
        format!("{}...", truncated)
    } else {
        text.to_string()
    }
}

// ============================================================================
// Reaction Count
// ============================================================================

/// Reaction count for a specific emoji.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReactionCount {
    pub emoji: String,
    pub count: u32,
}

impl ReactionCount {
    /// Create a new reaction count.
    #[must_use]
    pub fn new(emoji: String, count: u32) -> Self {
        Self { emoji, count }
    }

    /// Apply an add/remove to an aggregate reaction-counts vec in place.
    ///
    /// - `remove=false`: increment (or insert at count=1).
    /// - `remove=true`: decrement; drop the entry when the count reaches 0.
    ///
    /// Used by the UI to keep a message's `reaction_counts` in sync with
    /// inbound reaction events without round-tripping through a presentation
    /// string.
    pub fn apply(counts: &mut Vec<ReactionCount>, emoji: &str, remove: bool) {
        if let Some(existing) = counts.iter_mut().find(|r| r.emoji == emoji) {
            if remove {
                existing.count = existing.count.saturating_sub(1);
            } else {
                existing.count = existing.count.saturating_add(1);
            }
        } else if !remove {
            counts.push(ReactionCount::new(emoji.to_string(), 1));
        }
        counts.retain(|r| r.count > 0);
    }

    /// Format an aggregate reaction-counts vec as a compact summary string
    /// (e.g. "👍 3  ❤️ 2"). Used for accessibility labels only — never for
    /// round-tripping state.
    pub fn summary_string(counts: &[ReactionCount]) -> String {
        counts
            .iter()
            .map(|r| {
                if r.count == 1 {
                    r.emoji.clone()
                } else {
                    format!("{} {}", r.emoji, r.count)
                }
            })
            .collect::<Vec<_>>()
            .join("  ")
    }
}

// ============================================================================
// Message Content Types
// ============================================================================

/// Message content types.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum MessageContent {
    /// Plain text message.
    Text(String),
    /// Signal Protocol encrypted content.
    Encrypted(EncryptedContent),
    /// Sealed sender (encrypted + anonymous).
    SealedSender(SealedSenderContent),
    /// System message (join, leave, etc).
    System(SystemMessage),
    /// Call notification (started, ended).
    Call(CallMessage),
    /// AI-generated message.
    AI(AIMessage),
    /// Mixed text + files.
    Mixed(MixedContent),
}

impl MessageContent {
    /// Check if this is a system message.
    #[must_use]
    pub fn is_system(&self) -> bool {
        matches!(self, Self::System(_))
    }

    /// Check if this is encrypted content.
    #[must_use]
    pub fn is_encrypted(&self) -> bool {
        matches!(self, Self::Encrypted(_) | Self::SealedSender(_))
    }
}

// ============================================================================
// Encrypted Content
// ============================================================================

/// Encrypted message content (Signal Protocol).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EncryptedContent {
    pub ciphertext: Vec<u8>,
    /// Sender key ID for group messages.
    pub sender_key_id: Option<u32>,
    pub message_type: EncryptedMessageType,
}

impl EncryptedContent {
    /// Create a new encrypted content blob.
    #[must_use]
    pub fn new(ciphertext: Vec<u8>, message_type: EncryptedMessageType) -> Self {
        Self {
            ciphertext,
            sender_key_id: None,
            message_type,
        }
    }

    /// Create encrypted content with a sender key (for groups).
    #[must_use]
    pub fn with_sender_key(mut self, sender_key_id: u32) -> Self {
        self.sender_key_id = Some(sender_key_id);
        self
    }
}

/// Encrypted message type variants.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EncryptedMessageType {
    /// Standard Signal protocol (PreKey or Whisper message).
    Signal,
    /// Sender key for groups.
    SenderKey,
}

// ============================================================================
// Sealed Sender Content
// ============================================================================

/// Sealed sender message (hides sender from server).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SealedSenderContent {
    pub ciphertext: Vec<u8>,
}

impl SealedSenderContent {
    /// Create a new sealed sender content blob.
    #[must_use]
    pub fn new(ciphertext: Vec<u8>) -> Self {
        Self { ciphertext }
    }
}

// ============================================================================
// System Message
// ============================================================================

/// System message types.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SystemMessage {
    UserJoined(UserId),
    UserLeft(UserId),
    ChannelCreated,
    ChannelRenamed { old_name: String, new_name: String },
    UserAdded { by: UserId, added: UserId },
    UserRemoved { by: UserId, removed: UserId },
    /// A message from this peer arrived but could not be decrypted, and the
    /// ciphertext is gone (the inbox acks every polled envelope, so it is not
    /// redelivered). The session has been healed via `DecryptionErrorMessage`,
    /// but THAT message is lost — without this entry the user just sees a gap.
    ///
    /// Signal's own receiver-side answer to a bad decrypt: Signal-Android
    /// inserts a "Delivery issue" conversation update
    /// (`ThreadRecord_delivery_issue`).
    DeliveryIssue,
    /// This peer's identity key changed (a reinstall — or a MITM), and the
    /// message that surfaced it could not be decrypted. Like `DeliveryIssue`
    /// the ciphertext is gone, but unlike `DeliveryIssue` the SESSION cannot
    /// heal itself: no `DecryptionErrorMessage` is sent, resending fails
    /// identically forever, and the conversation stays wedged in both
    /// directions until a human accepts the new key.
    ///
    /// **Deliberately not folded into `DeliveryIssue`.** That variant's text
    /// tells the user the session has been reset and to ask for a resend, which
    /// is exactly the wrong advice here, and its resolution is automatic where
    /// this one requires a trust decision.
    ///
    /// `attributed` mirrors [`crate::error::IdentityChangeSource::is_authenticated`]:
    /// false means the peer id came from server-supplied envelope metadata and
    /// nothing proved it, so the wording must not assert who this was.
    IdentityChanged { attributed: bool },
}

impl SystemMessage {
    /// Get a human-readable description of the system message.
    #[must_use]
    pub fn description(&self) -> String {
        match self {
            Self::UserJoined(_) => "joined the channel".to_string(),
            Self::UserLeft(_) => "left the channel".to_string(),
            Self::ChannelCreated => "created the channel".to_string(),
            Self::ChannelRenamed { old_name, new_name } => {
                format!("renamed the channel from \"{}\" to \"{}\"", old_name, new_name)
            }
            Self::UserAdded { .. } => "added a member".to_string(),
            Self::UserRemoved { .. } => "removed a member".to_string(),
            // Rendered as "{sender_name} {description}", so this reads
            // "Alice sent a message that couldn't be delivered. ...".
            Self::DeliveryIssue => "sent a message that couldn't be delivered. The secure \
                 session has been reset — ask them to send it again."
                .to_string(),
            // Also rendered as "{sender_name} {description}". The unattributed
            // wording must not state as fact that this was them: the only thing
            // that named them was the server.
            Self::IdentityChanged { attributed: true } => "changed their safety number, so \
                 their message could not be read. Usually this means they reinstalled the \
                 app, but it can also mean someone is intercepting this conversation. \
                 Messages stay blocked until you accept the new safety number."
                .to_string(),
            Self::IdentityChanged { attributed: false } => "may have changed their safety \
                 number, so the message could not be read. The server says it came from \
                 them, but nothing proved that. Compare safety numbers over another channel \
                 before you accept."
                .to_string(),
        }
    }
}

// ============================================================================
// Call Message
// ============================================================================

/// Call message for call notifications in chat.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CallMessage {
    pub call_id: String,
    pub call_type: CallMessageType,
    pub initiator_id: UserId,
    pub initiator_name: String,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub duration: Option<Duration>,
    pub participant_count: u32,
}

impl CallMessage {
    /// Create a new call started message.
    #[must_use]
    pub fn started(call_id: String, initiator_id: UserId, initiator_name: String) -> Self {
        Self {
            call_id,
            call_type: CallMessageType::Started,
            initiator_id,
            initiator_name,
            started_at: Some(Utc::now()),
            ended_at: None,
            duration: None,
            participant_count: 1,
        }
    }

    /// Create a call ended message.
    #[must_use]
    pub fn ended(mut self) -> Self {
        self.call_type = CallMessageType::Ended;
        self.ended_at = Some(Utc::now());
        if let Some(started) = self.started_at {
            self.duration = Some(Utc::now() - started);
        }
        self
    }

    /// Get the duration in a human-readable format.
    #[must_use]
    pub fn duration_display(&self) -> Option<String> {
        self.duration.map(|d| {
            let secs = d.num_seconds();
            if secs < 60 {
                format!("{}s", secs)
            } else if secs < 3600 {
                format!("{}m {}s", secs / 60, secs % 60)
            } else {
                format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
            }
        })
    }
}

/// Call message type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallMessageType {
    Started,
    Ended,
    Missed,
}

// ============================================================================
// AI Message
// ============================================================================

/// AI-generated message content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AIMessage {
    pub text: String,
    pub integration_id: IntegrationId,
    pub model: Option<String>,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
}

impl AIMessage {
    /// Create a new AI message.
    #[must_use]
    pub fn new(text: String, integration_id: IntegrationId) -> Self {
        Self {
            text,
            integration_id,
            model: None,
            prompt_tokens: None,
            completion_tokens: None,
        }
    }

    /// Get the total token count.
    #[must_use]
    pub fn total_tokens(&self) -> Option<u32> {
        match (self.prompt_tokens, self.completion_tokens) {
            (Some(p), Some(c)) => Some(p + c),
            _ => None,
        }
    }
}

// ============================================================================
// Mixed Content
// ============================================================================

/// Mixed content (text + files).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MixedContent {
    pub text: Option<String>,
    pub attachment_ids: Vec<AttachmentId>,
}

impl MixedContent {
    /// Create mixed content with text only.
    #[must_use]
    pub fn text_only(text: String) -> Self {
        Self {
            text: Some(text),
            attachment_ids: Vec::new(),
        }
    }

    /// Create mixed content with attachments only.
    #[must_use]
    pub fn attachments_only(attachment_ids: Vec<AttachmentId>) -> Self {
        Self {
            text: None,
            attachment_ids,
        }
    }

    /// Create mixed content with both text and attachments.
    #[must_use]
    pub fn new(text: String, attachment_ids: Vec<AttachmentId>) -> Self {
        Self {
            text: Some(text),
            attachment_ids,
        }
    }

    /// Check if this has any content.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_none() && self.attachment_ids.is_empty()
    }
}

// ============================================================================
// Thread
// ============================================================================

/// Message thread (conversation branch).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Thread {
    pub root_message_id: MessageId,
    pub conversation_id: ChannelId,
    pub reply_count: u32,
    pub last_reply_at: Option<DateTime<Utc>>,
    pub participant_ids: Vec<UserId>,
}

impl Thread {
    /// Create a new thread from a root message.
    #[must_use]
    pub fn new(root_message_id: MessageId, conversation_id: ChannelId) -> Self {
        Self {
            root_message_id,
            conversation_id,
            reply_count: 0,
            last_reply_at: None,
            participant_ids: Vec::new(),
        }
    }

    /// Add a reply to the thread.
    pub fn add_reply(&mut self, participant_id: UserId) {
        self.reply_count += 1;
        self.last_reply_at = Some(Utc::now());
        if !self.participant_ids.contains(&participant_id) {
            self.participant_ids.push(participant_id);
        }
    }

    /// Check if the thread has any replies.
    #[must_use]
    pub fn has_replies(&self) -> bool {
        self.reply_count > 0
    }
}

/// Where a newly-received message belongs in an already-ordered list, or that
/// it's a duplicate. See [`ordered_insert_position`].
#[derive(Debug, PartialEq, Eq)]
pub enum InsertPosition {
    /// A message with the same `(timestamp_ns, sender_id)` is already present.
    Duplicate,
    /// Insert the new message at this index to keep the list ordered.
    At(usize),
}

/// Signal-style message ordering policy, kept in the domain so every renderer
/// applies the same rule instead of re-implementing it.
///
/// Messages are ordered by `timestamp_ns` ascending, ties broken by ascending
/// `sender_id`. Deduplication is by the full `(timestamp_ns, sender_id)` key.
///
/// `existing` must already be ordered by this same rule; each entry is that
/// row's `(timestamp_ns, sender_id)`. Returns [`InsertPosition::Duplicate`] if
/// the new key is already present, else the index to insert at.
pub fn ordered_insert_position(
    existing: &[(u64, &str)],
    new_ts: u64,
    new_sender: &str,
) -> InsertPosition {
    if existing.iter().any(|(ts, s)| *ts == new_ts && *s == new_sender) {
        return InsertPosition::Duplicate;
    }
    let pos = existing
        .iter()
        .position(|(ts, s)| match ts.cmp(&new_ts) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Equal => *s > new_sender,
            std::cmp::Ordering::Less => false,
        })
        .unwrap_or(existing.len());
    InsertPosition::At(pos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordered_insert_position_dedup_and_ordering() {
        let existing = [(10u64, "b"), (20, "a"), (20, "c")];
        // Duplicate key.
        assert_eq!(ordered_insert_position(&existing, 20, "a"), InsertPosition::Duplicate);
        // Earliest → front.
        assert_eq!(ordered_insert_position(&existing, 5, "z"), InsertPosition::At(0));
        // Latest → end.
        assert_eq!(ordered_insert_position(&existing, 30, "a"), InsertPosition::At(3));
        // Tie at ts=20: "b" sits between "a" and "c".
        assert_eq!(ordered_insert_position(&existing, 20, "b"), InsertPosition::At(2));
        // Empty list.
        assert_eq!(ordered_insert_position(&[], 1, "a"), InsertPosition::At(0));
    }

    /// The unread divider must land on the first message at-or-after the
    /// boundary, never "somewhere near it". Every case here is a way the chat
    /// opens on the wrong row if `divider_index` drifts.
    #[test]
    fn unread_anchor_divider_index() {
        let cid = ChannelId::new();
        let them = Some(UserId::new());
        let page: Vec<Message> = [10u64, 20, 30, 40, 50]
            .into_iter()
            .map(|ts| Message::new_text(cid, ConversationType::ServerDm, them, "m".into(), ts))
            .collect();

        // Nothing unread → open at the newest message, no divider.
        assert_eq!(UnreadAnchor::default().divider_index(&page), None);

        // Boundary exactly on a loaded row.
        let anchor = UnreadAnchor { first_unread_sent_at: Some(30), unread_count: 3 };
        assert_eq!(anchor.divider_index(&page), Some(2));

        // Boundary between rows → the first row after it.
        let anchor = UnreadAnchor { first_unread_sent_at: Some(35), unread_count: 2 };
        assert_eq!(anchor.divider_index(&page), Some(3));

        // Whole page unread (boundary older than the oldest loaded row) → open
        // at the top, divider above row 0. Not None: "everything is new" is
        // exactly when the divider matters most.
        let anchor = UnreadAnchor { first_unread_sent_at: Some(1), unread_count: 99 };
        assert_eq!(anchor.divider_index(&page), Some(0));

        // Only the newest row is unread.
        let anchor = UnreadAnchor { first_unread_sent_at: Some(50), unread_count: 1 };
        assert_eq!(anchor.divider_index(&page), Some(4));

        // Boundary newer than everything loaded → nothing to anchor on; open
        // at the bottom rather than off the end of the model.
        let anchor = UnreadAnchor { first_unread_sent_at: Some(60), unread_count: 1 };
        assert_eq!(anchor.divider_index(&page), None);

        // Empty page never yields an index.
        let anchor = UnreadAnchor { first_unread_sent_at: Some(10), unread_count: 1 };
        assert_eq!(anchor.divider_index(&[]), None);
    }

    #[test]
    fn test_message_new_text() {
        let conversation_id = ChannelId::new();
        let sender_id = Some(UserId::new());
        let msg = Message::new_text(conversation_id, ConversationType::Channel, sender_id, "Hello, world!".to_string(), 0);

        assert_eq!(msg.conversation_id, conversation_id);
        assert_eq!(msg.sender_id, sender_id);
        assert_eq!(msg.as_text(), Some("Hello, world!"));
        assert!(!msg.is_pinned);
        assert!(!msg.is_edited);
        assert!(!msg.is_deleted);
    }

    #[test]
    fn test_message_as_text() {
        let conversation_id = ChannelId::new();

        // Text content
        let text_msg = Message::new_text(conversation_id, ConversationType::Channel, None, "Test".to_string(), 0);
        assert_eq!(text_msg.as_text(), Some("Test"));

        // Mixed content
        let mut mixed_msg = Message::new_text(conversation_id, ConversationType::Channel, None, "".to_string(), 0);
        mixed_msg.content = MessageContent::Mixed(MixedContent::text_only("Mixed text".to_string()));
        assert_eq!(mixed_msg.as_text(), Some("Mixed text"));

        // Encrypted content (no text extraction)
        let mut enc_msg = Message::new_text(conversation_id, ConversationType::Channel, None, "".to_string(), 0);
        enc_msg.content = MessageContent::Encrypted(EncryptedContent::new(
            vec![1, 2, 3],
            EncryptedMessageType::Signal,
        ));
        assert_eq!(enc_msg.as_text(), None);
    }

    #[test]
    fn test_thread_lifecycle() {
        let root_id = MessageId::new();
        let conversation_id = ChannelId::new();
        let mut thread = Thread::new(root_id, conversation_id);

        assert!(!thread.has_replies());
        assert_eq!(thread.reply_count, 0);

        let user1 = UserId::new();
        let user2 = UserId::new();

        thread.add_reply(user1);
        assert!(thread.has_replies());
        assert_eq!(thread.reply_count, 1);
        assert_eq!(thread.participant_ids.len(), 1);

        thread.add_reply(user2);
        assert_eq!(thread.reply_count, 2);
        assert_eq!(thread.participant_ids.len(), 2);

        // Same user replies again - shouldn't add to participants
        thread.add_reply(user1);
        assert_eq!(thread.reply_count, 3);
        assert_eq!(thread.participant_ids.len(), 2);
    }

    #[test]
    fn test_system_message_description() {
        let user_id = UserId::new();
        let msg = SystemMessage::UserJoined(user_id);
        assert_eq!(msg.description(), "joined the channel");

        let rename_msg = SystemMessage::ChannelRenamed {
            old_name: "old".to_string(),
            new_name: "new".to_string(),
        };
        assert!(rename_msg.description().contains("old"));
        assert!(rename_msg.description().contains("new"));
    }

    #[test]
    fn test_call_message_duration() {
        let call = CallMessage::started(
            "call-123".to_string(),
            UserId::new(),
            "Alice".to_string(),
        );
        assert_eq!(call.call_type, CallMessageType::Started);
        assert!(call.started_at.is_some());
        assert!(call.duration.is_none());
    }

    #[test]
    fn test_reaction_count() {
        let conversation_id = ChannelId::new();
        let mut msg = Message::new_text(conversation_id, ConversationType::Channel, None, "Test".to_string(), 0);
        msg.reaction_counts.push(ReactionCount::new("👍".to_string(), 5));
        msg.reaction_counts.push(ReactionCount::new("❤️".to_string(), 3));

        assert_eq!(msg.total_reactions(), 8);
    }

    /// The sidebar's second line is the one place every activity kind has to
    /// come out as *something*. Each branch of `sidebar_preview` is a row a
    /// user would otherwise see blank.
    #[test]
    fn test_sidebar_preview_covers_every_activity_kind() {
        let conv = ChannelId::new();
        let me = UserId::new();
        let them = UserId::new();
        let text = |sender, body: &str| {
            Message::new_text(conv, ConversationType::Channel, Some(sender), body.to_string(), 0)
        };

        // Plain text, and the same text from us.
        assert_eq!(text(them, "hello").sidebar_preview(Some(&me), None), "hello");
        assert_eq!(text(me, "hello").sidebar_preview(Some(&me), None), "You: hello");
        // Emoji-only is just text — it must survive, not become "[Message]".
        assert_eq!(text(them, "\u{1F389}").sidebar_preview(Some(&me), None), "\u{1F389}");

        // Attachment with no caption: typed label when resolved, count when not.
        let mut file = text(them, "");
        file.attachments = vec![AttachmentId::new(), AttachmentId::new()];
        assert_eq!(
            file.sidebar_preview(Some(&me), Some("\u{1F5BC}\u{FE0F} Photo +1")),
            "\u{1F5BC}\u{FE0F} Photo +1"
        );
        assert_eq!(file.sidebar_preview(Some(&me), None), "\u{1F4CE} 2 attachments");

        // Nothing renderable at all still names the row.
        assert_eq!(text(them, "").sidebar_preview(Some(&me), None), "[Message]");

        // A pin marks every kind, and does not eat the body.
        let mut pinned = text(them, "hello");
        pinned.is_pinned = true;
        assert_eq!(pinned.sidebar_preview(Some(&me), None), "\u{1F4CC} hello");
    }

    #[test]
    fn test_mixed_content() {
        let mixed = MixedContent::new(
            "Caption".to_string(),
            vec![AttachmentId::new()],
        );
        assert!(!mixed.is_empty());
        assert_eq!(mixed.text, Some("Caption".to_string()));

        let empty = MixedContent {
            text: None,
            attachment_ids: Vec::new(),
        };
        assert!(empty.is_empty());
    }
}
