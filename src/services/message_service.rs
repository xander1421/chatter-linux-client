//! Message service.
//!
//! Send, edit, delete, forward, and quote messages.

use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};

use crate::domain::ids::{AttachmentId, ChannelId, IntegrationId, MessageId, UserId};
use crate::domain::message::{Message, MessageContent, MixedContent, ReactionCount};
use crate::error::{NetworkError, StorageError};
use crate::network::grpc::message_client::MessageClient;
use crate::proto::chat_v1;
use crate::storage::repositories::message_repo::MessageRepository;
use crate::storage::repositories::settings_repo::{keys, SettingsRepository};

/// Message service errors.
#[derive(Debug, thiserror::Error)]
pub enum MessageError {
    /// Operation requires authentication.
    #[error("not logged in")]
    NotLoggedIn,

    /// Target channel does not exist.
    #[error("channel not found: {0}")]
    ChannelNotFound(ChannelId),

    /// Message to edit/delete was not found.
    #[error("message not found: {0}")]
    MessageNotFound(MessageId),

    /// User is not the message author (can't edit/delete).
    #[error("not the message author")]
    NotAuthor,

    /// Content validation failed.
    #[error("invalid content: {0}")]
    InvalidContent(String),

    /// Network operation failed.
    #[error("network error: {0}")]
    Network(#[from] NetworkError),

    /// Storage operation failed.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

/// Message service for all message operations.
///
/// Orchestrates message sending, editing, and deletion by coordinating
/// network client and local storage.
pub struct MessageService {
    settings_repo: Arc<SettingsRepository>,
    message_repo: Arc<MessageRepository>,
}

impl MessageService {
    /// Create a new MessageService with dependencies.
    pub fn new(settings_repo: Arc<SettingsRepository>, message_repo: Arc<MessageRepository>) -> Self {
        Self {
            settings_repo,
            message_repo,
        }
    }

    /// Get the currently logged-in user ID.
    fn current_user_id(&self) -> Result<UserId, MessageError> {
        let user_id_str = self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .map_err(MessageError::Storage)?
            .ok_or(MessageError::NotLoggedIn)?;

        user_id_str
            .parse()
            .map_err(|_| MessageError::InvalidContent("invalid user ID in settings".to_string()))
    }

    /// Send a message to a channel.
    ///
    /// # Arguments
    ///
    /// * `message_client` - Mutable reference to message client
    /// * `channel_id` - Target channel ID
    /// * `text` - Message text content
    /// * `attachment_ids` - Optional attachment IDs
    /// * `thread_id` - Optional parent message ID for threading
    /// * `mentioned_integrations` - AI integrations mentioned in message
    /// * `targets_ai_bot` - Whether message targets AI bot
    #[allow(clippy::too_many_arguments)]
    pub async fn send_message(
        &self,
        message_client: &mut MessageClient,
        channel_id: &ChannelId,
        text: &str,
        attachment_ids: Vec<AttachmentId>,
        thread_id: Option<&MessageId>,
        mentioned_integrations: Vec<IntegrationId>,
        targets_ai_bot: bool,
    ) -> Result<Message, MessageError> {
        // Check logged in
        let sender_id = self.current_user_id()?;

        // Validate content - must have text or attachments
        let trimmed_text = text.trim();
        if trimmed_text.is_empty() && attachment_ids.is_empty() {
            return Err(MessageError::InvalidContent(
                "message must have text or attachments".to_string(),
            ));
        }

        // Build proto content
        let content = chat_v1::message::Content {
            r#type: Some(chat_v1::message::content::Type::Text(trimmed_text.to_string())),
        };

        // Call server
        let proto_message = message_client
            .send_message(
                &channel_id.to_string(),
                content,
                thread_id.map(|id| id.to_string()).as_deref(),
                false, // ephemeral (Phase 34)
                None,  // ephemeral_ttl
                false, // encrypt (separate concern)
                None,  // crypto_options
                mentioned_integrations.iter().map(|id| id.to_string()).collect(),
                targets_ai_bot,
                attachment_ids.iter().map(|id| id.to_string()).collect(),
            )
            .await?;

        // Convert to domain message
        let message = proto_to_domain_message(&proto_message, sender_id)?;

        // Store locally
        self.message_repo.insert(&message)?;

        Ok(message)
    }
}

/// Convert a proto Message to a domain Message.
fn proto_to_domain_message(
    proto: &chat_v1::Message,
    sender_id: UserId,
) -> Result<Message, MessageError> {
    // Parse message ID
    let id: MessageId = proto
        .id
        .parse()
        .map_err(|_| MessageError::InvalidContent("invalid message ID".to_string()))?;

    // Parse channel ID
    let channel_id: ChannelId = proto
        .channel_id
        .parse()
        .map_err(|_| MessageError::InvalidContent("invalid channel ID".to_string()))?;

    // Parse thread ID (parent_id in proto)
    let thread_id: Option<MessageId> = if proto.parent_id.is_empty() {
        None
    } else {
        Some(
            proto
                .parent_id
                .parse()
                .map_err(|_| MessageError::InvalidContent("invalid parent ID".to_string()))?,
        )
    };

    // Parse mentioned integrations
    let mentioned_integrations: Vec<IntegrationId> = proto
        .mentioned_integrations
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect();

    // Convert proto content to domain content
    let content = proto_content_to_domain(proto.content.as_ref())?;

    // Convert reaction counts
    let reaction_counts: Vec<ReactionCount> = proto
        .reaction_counts
        .iter()
        .map(|rc| ReactionCount::new(rc.emoji.clone(), rc.count as u32))
        .collect();

    // Convert timestamp (proto uses millis)
    let created_at = millis_to_datetime(proto.timestamp);
    let updated_at = if proto.updated_at > 0 {
        millis_to_datetime(proto.updated_at)
    } else {
        created_at
    };
    let edited_at = if proto.edited && proto.edited_at > 0 {
        Some(millis_to_datetime(proto.edited_at))
    } else {
        None
    };

    Ok(Message {
        id,
        channel_id,
        sender_id: Some(sender_id),
        content,
        thread_id,
        reply_to: None, // Not exposed in proto yet
        attachments: Vec::new(), // Handled separately via mentioned_file_ids
        is_pinned: proto.pinned,
        is_edited: proto.edited,
        is_deleted: false,
        created_at,
        updated_at,
        edited_at,
        reaction_counts,
        current_user_reactions: proto.current_user_reactions.clone(),
        mentioned_integrations,
        targets_ai_bot: proto.targets_ai_bot,
    })
}

/// Convert proto content to domain MessageContent.
fn proto_content_to_domain(
    proto_content: Option<&chat_v1::message::Content>,
) -> Result<MessageContent, MessageError> {
    let content = proto_content.ok_or_else(|| {
        MessageError::InvalidContent("message has no content".to_string())
    })?;

    match &content.r#type {
        Some(chat_v1::message::content::Type::Text(text)) => {
            Ok(MessageContent::Text(text.clone()))
        }
        Some(chat_v1::message::content::Type::Encrypted(_)) => {
            // Encrypted content - store as placeholder text for now
            // Full decryption is handled by crypto layer
            Ok(MessageContent::Text("[Encrypted message]".to_string()))
        }
        Some(chat_v1::message::content::Type::File(file_meta)) => {
            // File-only message - use MixedContent with no text
            let attachment_id: AttachmentId = file_meta
                .id
                .parse()
                .map_err(|_| MessageError::InvalidContent("invalid file ID".to_string()))?;
            Ok(MessageContent::Mixed(MixedContent::attachments_only(vec![attachment_id])))
        }
        None => {
            // No content type - default to empty text
            Ok(MessageContent::Text(String::new()))
        }
        _ => {
            // Other types (reactions, system, call, AI response) - handle as text placeholder
            Ok(MessageContent::Text("[Special message]".to_string()))
        }
    }
}

/// Convert milliseconds timestamp to DateTime<Utc>.
fn millis_to_datetime(millis: u64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(millis as i64)
        .single()
        .unwrap_or_else(Utc::now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_error_display() {
        let err = MessageError::NotLoggedIn;
        assert!(err.to_string().contains("not logged in"));

        let err = MessageError::InvalidContent("empty".to_string());
        assert!(err.to_string().contains("empty"));

        let channel_id = ChannelId::new();
        let err = MessageError::ChannelNotFound(channel_id);
        assert!(err.to_string().contains("channel not found"));

        let message_id = MessageId::new();
        let err = MessageError::MessageNotFound(message_id);
        assert!(err.to_string().contains("message not found"));

        let err = MessageError::NotAuthor;
        assert!(err.to_string().contains("not the message author"));
    }

    #[test]
    fn test_proto_content_to_domain_text() {
        use chat_v1::message::content::Type;
        use chat_v1::message::Content;

        let proto_content = Content {
            r#type: Some(Type::Text("Hello world".to_string())),
        };

        let result = proto_content_to_domain(Some(&proto_content)).unwrap();
        match result {
            MessageContent::Text(text) => assert_eq!(text, "Hello world"),
            _ => panic!("Expected Text variant"),
        }
    }

    #[test]
    fn test_proto_content_to_domain_none() {
        let result = proto_content_to_domain(None);
        assert!(result.is_err());
    }

    #[test]
    fn test_millis_to_datetime() {
        let millis = 1706140800000u64; // 2024-01-25 00:00:00 UTC
        let dt = millis_to_datetime(millis);
        assert_eq!(dt.timestamp_millis(), millis as i64);
    }
}
