use super::SystemMessage;
use crate::domain::ids::{AttachmentId, IntegrationId, UserId};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

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
    /// A call declined before it connected, by either side (the Calls tab's
    /// "Declined").
    Declined,
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
