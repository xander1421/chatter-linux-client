//! Message domain entity.
//!
//! Represents chat messages, threads, and message content.

use crate::domain::ids::{AttachmentId, ChannelId, IntegrationId, MessageId, UserId};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

// ============================================================================
// Message
// ============================================================================

/// Represents a chat message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub channel_id: ChannelId,
    /// None for sealed sender messages (server doesn't know sender).
    pub sender_id: Option<UserId>,
    pub content: MessageContent,
    /// Parent message ID for threads.
    pub thread_id: Option<MessageId>,
    /// Message being replied to (quote).
    pub reply_to: Option<MessageId>,
    pub attachments: Vec<AttachmentId>,
    pub is_pinned: bool,
    pub is_edited: bool,
    pub is_deleted: bool,
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
        channel_id: ChannelId,
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
            channel_id,
            sender_id,
            content: MessageContent::Text(text),
            thread_id: None,
            reply_to: None,
            attachments: Vec::new(),
            is_pinned: false,
            is_edited: false,
            is_deleted: false,
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
    pub channel_id: ChannelId,
    pub reply_count: u32,
    pub last_reply_at: Option<DateTime<Utc>>,
    pub participant_ids: Vec<UserId>,
}

impl Thread {
    /// Create a new thread from a root message.
    #[must_use]
    pub fn new(root_message_id: MessageId, channel_id: ChannelId) -> Self {
        Self {
            root_message_id,
            channel_id,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_new_text() {
        let channel_id = ChannelId::new();
        let sender_id = Some(UserId::new());
        let msg = Message::new_text(channel_id, sender_id, "Hello, world!".to_string(), 0);

        assert_eq!(msg.channel_id, channel_id);
        assert_eq!(msg.sender_id, sender_id);
        assert_eq!(msg.as_text(), Some("Hello, world!"));
        assert!(!msg.is_pinned);
        assert!(!msg.is_edited);
        assert!(!msg.is_deleted);
    }

    #[test]
    fn test_message_as_text() {
        let channel_id = ChannelId::new();

        // Text content
        let text_msg = Message::new_text(channel_id, None, "Test".to_string(), 0);
        assert_eq!(text_msg.as_text(), Some("Test"));

        // Mixed content
        let mut mixed_msg = Message::new_text(channel_id, None, "".to_string(), 0);
        mixed_msg.content = MessageContent::Mixed(MixedContent::text_only("Mixed text".to_string()));
        assert_eq!(mixed_msg.as_text(), Some("Mixed text"));

        // Encrypted content (no text extraction)
        let mut enc_msg = Message::new_text(channel_id, None, "".to_string(), 0);
        enc_msg.content = MessageContent::Encrypted(EncryptedContent::new(
            vec![1, 2, 3],
            EncryptedMessageType::Signal,
        ));
        assert_eq!(enc_msg.as_text(), None);
    }

    #[test]
    fn test_thread_lifecycle() {
        let root_id = MessageId::new();
        let channel_id = ChannelId::new();
        let mut thread = Thread::new(root_id, channel_id);

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
        let channel_id = ChannelId::new();
        let mut msg = Message::new_text(channel_id, None, "Test".to_string(), 0);
        msg.reaction_counts.push(ReactionCount::new("👍".to_string(), 5));
        msg.reaction_counts.push(ReactionCount::new("❤️".to_string(), 3));

        assert_eq!(msg.total_reactions(), 8);
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
