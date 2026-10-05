//! Pin service.
//!
//! Pin/unpin messages and pinned bar navigation.

use std::sync::Arc;

use crate::domain::ids::{ChannelId, MessageId};
use crate::domain::{Attachment, ConversationType, FileCategory, Message};
use crate::services::attachment::AttachmentService;
use crate::services::message::MessageService;
use crate::storage::Database;

/// Pin service errors.
#[derive(Debug, thiserror::Error)]
pub enum PinError {
    /// Storage error.
    #[error("storage error: {0}")]
    Storage(#[from] crate::error::StorageError),

    /// Message service error.
    #[error("message service error: {0}")]
    MessageService(#[from] crate::services::message::MessageError),
}

/// Result type for pin operations.
pub type PinResult<T> = Result<T, PinError>;

/// Pinned message with preview data.
#[derive(Debug, Clone)]
pub struct PinnedMessageData {
    /// The message ID.
    pub message_id: MessageId,
    /// Preview text for the pinned message.
    pub preview: String,
    /// Whether this message has attachments.
    pub has_attachments: bool,
}

/// Pin service for message pinning.
pub struct PinService {
    message_service: Arc<MessageService>,
    attachment_service: Arc<AttachmentService>,
}

impl PinService {
    /// Create a new pin service.
    pub fn new(
        _database: Arc<Database>,
        message_service: Arc<MessageService>,
        attachment_service: Arc<AttachmentService>,
    ) -> Self {
        Self {
            message_service,
            attachment_service,
        }
    }

    /// Load pinned messages for a channel.
    pub async fn load_pinned_for_channel(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> PinResult<Vec<PinnedMessageData>> {
        // Get pinned messages from message service
        let pinned_messages = self
            .message_service
            .get_pinned_messages_local(channel_id, conversation_type)?;

        if pinned_messages.is_empty() {
            return Ok(Vec::new());
        }

        // Build pinned message data with previews
        let mut pinned_data = Vec::with_capacity(pinned_messages.len());

        for msg in &pinned_messages {
            // Resolve attachments for preview generation
            let attachments: Vec<Attachment> = msg
                .attachments
                .iter()
                .filter_map(|att_id| self.attachment_service.get(att_id).ok().flatten())
                .collect();

            let preview = Self::generate_preview(&msg, &attachments);

            pinned_data.push(PinnedMessageData {
                message_id: msg.id,
                preview,
                has_attachments: !attachments.is_empty(),
            });
        }

        Ok(pinned_data)
    }

    /// Get a specific pinned message with preview.
    pub fn get_pinned_message(
        &self,
        message_id: &MessageId,
    ) -> PinResult<Option<PinnedMessageData>> {
        // Get the message
        let Some(message) = self.message_service.get_message_local(message_id)? else {
            return Ok(None);
        };

        // Resolve attachments
        let attachments: Vec<Attachment> = message
            .attachments
            .iter()
            .filter_map(|att_id| self.attachment_service.get(att_id).ok().flatten())
            .collect();

        let preview = Self::generate_preview(&message, &attachments);

        Ok(Some(PinnedMessageData {
            message_id: message.id,
            preview,
            has_attachments: !attachments.is_empty(),
        }))
    }

    /// Pin a message.
    pub async fn pin_message(
        &self,
        _channel_id: &ChannelId,
        message_id: &MessageId,
    ) -> PinResult<()> {
        // Update message as pinned in database
        self.message_service.set_message_pinned(message_id, true)?;
        Ok(())
    }

    /// Unpin a message.
    pub async fn unpin_message(&self, message_id: &MessageId) -> PinResult<()> {
        // Update message as unpinned in database
        self.message_service.set_message_pinned(message_id, false)?;
        Ok(())
    }

    /// Generate a preview text for a pinned message.
    fn generate_preview(message: &Message, attachments: &[Attachment]) -> String {
        // First try to get text content
        if let Some(text) = message.as_text() {
            if !text.is_empty() {
                return if text.chars().count() > 80 {
                    let truncated: String = text.chars().take(77).collect();
                    format!("{}...", truncated)
                } else {
                    text.to_string()
                };
            }
        }

        // If no text, describe attachments
        if !attachments.is_empty() {
            if attachments.len() == 1 {
                let att = &attachments[0];
                let category = att.metadata.category();
                let (emoji, type_name) = match category {
                    FileCategory::Image => ("📷", "Photo"),
                    FileCategory::Video => ("🎬", "Video"),
                    FileCategory::Audio => ("🎵", "Voice message"),
                    FileCategory::Document => ("📄", "Document"),
                    FileCategory::Code => ("📝", "Code"),
                    FileCategory::Archive => ("📦", "Archive"),
                    FileCategory::Other => ("📎", "File"),
                };
                return format!("{} {}", emoji, type_name);
            } else {
                return format!("📎 {} attachments", attachments.len());
            }
        }

        // Fallback for unresolved attachment IDs
        if !message.attachments.is_empty() {
            if message.attachments.len() == 1 {
                "📎 Attachment".to_string()
            } else {
                format!("📎 {} attachments", message.attachments.len())
            }
        } else {
            "[Pinned message]".to_string()
        }
    }
}
