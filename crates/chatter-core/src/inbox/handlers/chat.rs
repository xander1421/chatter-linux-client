//! Chat message handler.
//!
//! Handles incoming DataMessage content: new messages, delete requests,
//! and message edits. Emits DomainEvent for UI and storage consumption.

use std::str::FromStr;

use prost::Message as ProstMessage;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::domain::{
    AttachmentId, ChannelId, IntegrationId, Message, MessageContent, MessageId, MixedContent,
    UserId,
};
use crate::inbox::processor::{ContentType, ProcessedMessage};
use crate::proto::content_v1::{content, Content, DataMessage, Delete};

use super::{DomainEvent, HandlerContext, HandlerError, HandlerOutcome, HandlerResult, InboxHandler};

// ============================================================================
// UUID Helpers
// ============================================================================

/// Create a deterministic UUID from bytes using SHA256.
///
/// Takes the first 16 bytes of SHA256 hash and sets version/variant bits.
fn uuid_from_bytes(bytes: &[u8]) -> Uuid {
    let hash = Sha256::digest(bytes);
    let mut uuid_bytes = [0u8; 16];
    uuid_bytes.copy_from_slice(&hash[..16]);
    // Set version 4 (random) and variant 1 (RFC 4122)
    uuid_bytes[6] = (uuid_bytes[6] & 0x0f) | 0x40;
    uuid_bytes[8] = (uuid_bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(uuid_bytes)
}

/// Create a deterministic UUID from a string.
fn uuid_from_string(s: &str) -> Uuid {
    uuid_from_bytes(s.as_bytes())
}

/// Create a deterministic UUID from a u64 timestamp.
fn uuid_from_timestamp(ts: u64) -> Uuid {
    uuid_from_bytes(&ts.to_le_bytes())
}

/// Create a deterministic UUID from (author, timestamp) tuple.
/// Signal uses this tuple to uniquely identify messages.
fn uuid_from_author_and_timestamp(author: &UserId, ts: u64) -> Uuid {
    let mut bytes = Vec::with_capacity(48); // 16 (UUID) + 8 (u64) + some padding
    bytes.extend_from_slice(author.as_uuid().as_bytes());
    bytes.extend_from_slice(&ts.to_le_bytes());
    uuid_from_bytes(&bytes)
}

// ============================================================================
// ChatHandler
// ============================================================================

/// Handler for DataMessage (regular chat messages).
///
/// Processes:
/// - New text messages
/// - Messages with attachments
/// - Quote/reply messages
/// - Delete requests (delete for everyone)
/// - AI-targeted messages
/// - Ephemeral messages
///
/// # Stateless Pattern
///
/// ChatHandler has no fields and takes all context via parameters.
/// This avoids mutable aliasing and simplifies testing.
pub struct ChatHandler;

impl ChatHandler {
    /// Create a new chat handler.
    pub fn new() -> Self {
        Self
    }

    /// Build a domain Message from DataMessage proto.
    fn build_message(
        &self,
        data_message: &DataMessage,
        processed: &ProcessedMessage,
        _ctx: &HandlerContext,
    ) -> Result<Message, HandlerError> {
        // Parse channel_id from proto (required for group context, derived for DMs)
        let channel_id = if !data_message.channel_id.is_empty() {
            ChannelId::from_str(&data_message.channel_id).map_err(|_| {
                HandlerError::validation_failed(format!(
                    "invalid channel_id: {}",
                    data_message.channel_id
                ))
            })?
        } else {
            // For DMs, derive channel from sender (symmetric channel ID)
            // In practice, DM channel IDs would be established during conversation creation
            // For now, require explicit channel_id
            return Err(HandlerError::validation_failed("missing channel_id"));
        };

        // Extract sender from ProcessedMessage (sealed sender decryption result)
        let sender_id = processed.sender_address.as_ref().map(|addr| {
            // ProtocolAddress name is the UUID string
            UserId::from_str(addr.name()).unwrap_or_else(|_| {
                // Fallback: generate deterministic ID from address name
                UserId::from_uuid(uuid_from_string(addr.name()))
            })
        });

        // Build message content
        let content = self.build_content(data_message)?;

        // Parse reply_to from quote
        let reply_to = data_message.quote.as_ref().map(|q| {
            // Signal uses timestamp as message ID, we derive a MessageId from it
            MessageId::from_uuid(uuid_from_timestamp(q.id))
        });

        // Parse attachment IDs (must match upload path - uses from_cdn_key)
        let attachments: Vec<AttachmentId> = data_message
            .attachments
            .iter()
            .map(|ptr| AttachmentId::from_cdn_key(&ptr.cdn_key))
            .collect();

        // Parse mentioned integrations (AI bots)
        let mentioned_integrations: Vec<IntegrationId> = data_message
            .mentioned_integrations
            .iter()
            .filter_map(|id_str| IntegrationId::from_str(id_str).ok())
            .collect();

        // Build the message with sender's timestamp for Signal-style dedup
        // created_at is derived from sent_at in Message::new_text()
        let mut message = Message::new_text(channel_id, sender_id, String::new(), data_message.timestamp);
        message.content = content;
        message.reply_to = reply_to;
        message.attachments = attachments;
        message.mentioned_integrations = mentioned_integrations;
        message.targets_ai_bot = data_message.targets_ai_bot;

        // Set thread_id if this is a reply
        if let Some(quote) = &data_message.quote {
            // The reply_to becomes the thread root
            message.thread_id = Some(MessageId::from_uuid(uuid_from_timestamp(quote.id)));
        }

        Ok(message)
    }

    /// Build MessageContent from DataMessage.
    fn build_content(&self, data_message: &DataMessage) -> Result<MessageContent, HandlerError> {
        let has_text = !data_message.body.is_empty();
        let has_attachments = !data_message.attachments.is_empty();

        match (has_text, has_attachments) {
            (true, false) => Ok(MessageContent::Text(data_message.body.clone())),
            (false, true) => {
                // Attachments only
                let attachment_ids: Vec<AttachmentId> = data_message
                    .attachments
                    .iter()
                    .map(|ptr| AttachmentId::from_cdn_key(&ptr.cdn_key))
                    .collect();
                Ok(MessageContent::Mixed(MixedContent::attachments_only(
                    attachment_ids,
                )))
            }
            (true, true) => {
                // Mixed content
                let attachment_ids: Vec<AttachmentId> = data_message
                    .attachments
                    .iter()
                    .map(|ptr| AttachmentId::from_cdn_key(&ptr.cdn_key))
                    .collect();
                Ok(MessageContent::Mixed(MixedContent::new(
                    data_message.body.clone(),
                    attachment_ids,
                )))
            }
            (false, false) => {
                // Empty message - could be valid for ephemeral/system
                Ok(MessageContent::Text(String::new()))
            }
        }
    }

    /// Handle delete request (delete for everyone).
    ///
    /// Signal identifies messages by (authorId, timestamp) tuple.
    /// For "delete for everyone", you can only delete your own messages,
    /// so the author of the message = sender of the delete (from sealed sender envelope).
    async fn handle_delete(
        &self,
        delete: &Delete,
        processed: &ProcessedMessage,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        // Extract author from sealed sender envelope.
        // This is the person who sent the delete AND the author of the message being deleted
        // (Signal only allows deleting your own messages for "delete for everyone").
        let author_id = processed
            .sender_address
            .as_ref()
            .map(|addr| {
                UserId::from_str(addr.name()).unwrap_or_else(|_| {
                    UserId::from_uuid(uuid_from_string(addr.name()))
                })
            })
            .ok_or_else(|| HandlerError::missing_context("sender_address"))?;

        // Derive message ID from (author, timestamp) tuple - Signal's message identification
        // We hash both values together to create a deterministic UUID
        let message_id = MessageId::from_uuid(uuid_from_author_and_timestamp(
            &author_id,
            delete.target_sent_timestamp,
        ));

        // For DM deletes, channel_id is None - the event handler will derive it from author_id.
        // For group deletes, the channel context would come from the conversation.
        // The sealed sender envelope tells us who sent the delete, which is sufficient
        // for DMs (the channel is the DM conversation with that sender).
        let channel_id = None;

        tracing::info!(
            ?author_id,
            timestamp = delete.target_sent_timestamp,
            ?message_id,
            ?channel_id,
            "Inbox handler: processing delete-for-everyone request"
        );

        // Emit delete event
        ctx.emit(DomainEvent::MessageDeleted {
            message_id,
            author_id,
            channel_id,
        });

        tracing::debug!(?message_id, "Delete event emitted");
        Ok(HandlerOutcome::Processed)
    }
}

impl Default for ChatHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl InboxHandler for ChatHandler {
    async fn handle(&self, message: &ProcessedMessage, ctx: &HandlerContext) -> HandlerResult {
        // Decode Content proto from plaintext
        let content: Content = ProstMessage::decode(message.plaintext.as_slice())
            .map_err(|e| HandlerError::parse_failed(e.to_string()))?;

        // Extract DataMessage from oneof
        let data_message = match content.content {
            Some(content::Content::DataMessage(dm)) => dm,
            _ => return Ok(HandlerOutcome::Ignored),
        };

        // Check for delete request
        if let Some(delete) = &data_message.delete {
            return self.handle_delete(delete, message, ctx).await;
        }

        // Build domain Message
        let msg = self.build_message(&data_message, message, ctx)?;

        // Emit event
        ctx.emit(DomainEvent::MessageReceived { message: msg });

        Ok(HandlerOutcome::Processed)
    }

    fn content_types(&self) -> &[ContentType] {
        &[ContentType::DataMessage]
    }
}

// ============================================================================
// Helpers
// ============================================================================

// Note: Use crate::util::time::TimestampNanos for timestamp conversion

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;
    use libsignal_protocol::{DeviceId, ProtocolAddress};
    use tokio::sync::broadcast;
    use crate::proto::mailbox_v1::EnvelopeType;

    fn make_context() -> HandlerContext {
        let (tx, _rx) = broadcast::channel(16);
        HandlerContext::new(UserId::new(), tx)
    }

    fn make_processed_message(plaintext: Vec<u8>) -> ProcessedMessage {
        ProcessedMessage::new(
            "msg-123".to_string(),
            Some(ProtocolAddress::new(
                Uuid::new_v4().to_string(),
                DeviceId::try_from(1u32).unwrap(),
            )),
            ContentType::DataMessage,
            plaintext,
            1234567890000,
            EnvelopeType::SealedSender,
        )
    }

    #[test]
    fn test_timestamp_nanos_to_datetime() {
        use crate::util::time::TimestampNanos;
        // 2024-01-01 00:00:00 UTC in nanoseconds
        let ts = TimestampNanos::from_nanos(1704067200_000_000_000u64);
        let dt = ts.to_datetime().unwrap();
        assert_eq!(dt.year(), 2024);
        assert_eq!(dt.month(), 1);
        assert_eq!(dt.day(), 1);

        assert!(TimestampNanos::from_nanos(0).to_datetime().is_none());
    }

    #[tokio::test]
    async fn test_chat_handler_ignores_non_data_message() {
        let handler = ChatHandler::new();
        let ctx = make_context();

        // Create a Content with typing message instead of data message
        let content = Content {
            content: Some(content::Content::Typing(
                crate::proto::content_v1::TypingMessage {
                    action: 1,
                    timestamp: 0,
                    channel_id: String::new(),
                },
            )),
            ephemeral_reply_to: None,
            timestamp: 0,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed_message(plaintext);

        let result = handler.handle(&processed, &ctx).await.unwrap();
        assert!(result.is_ignored());
    }

    #[tokio::test]
    async fn test_chat_handler_processes_text_message() {
        let handler = ChatHandler::new();
        let (tx, mut rx) = broadcast::channel(16);
        let ctx = HandlerContext::new(UserId::new(), tx);

        let channel_id = ChannelId::new();
        let data_message = DataMessage {
            body: "Hello, world!".to_string(),
            channel_id: channel_id.to_string(),
            timestamp: 1704067200000,
            ..Default::default()
        };

        let content = Content {
            content: Some(content::Content::DataMessage(data_message)),
            ephemeral_reply_to: None,
            timestamp: 1704067200000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed_message(plaintext);

        let result = handler.handle(&processed, &ctx).await.unwrap();
        assert!(result.is_success());

        // Verify event was emitted
        let event = rx.try_recv().unwrap();
        if let DomainEvent::MessageReceived { message } = event {
            assert_eq!(message.as_text(), Some("Hello, world!"));
            assert_eq!(message.conversation_id, channel_id);
        } else {
            panic!("Expected MessageReceived event");
        }
    }

    #[tokio::test]
    async fn test_chat_handler_processes_delete() {
        let handler = ChatHandler::new();
        let (tx, mut rx) = broadcast::channel(16);
        let ctx = HandlerContext::new(UserId::new(), tx);

        let delete = Delete {
            target_sent_timestamp: 1704067200000,
        };

        let data_message = DataMessage {
            delete: Some(delete),
            ..Default::default()
        };

        let content = Content {
            content: Some(content::Content::DataMessage(data_message)),
            ephemeral_reply_to: None,
            timestamp: 1704067201000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed_message(plaintext);

        let result = handler.handle(&processed, &ctx).await.unwrap();
        assert!(result.is_success());

        // Verify delete event was emitted
        let event = rx.try_recv().unwrap();
        assert!(matches!(event, DomainEvent::MessageDeleted { .. }));
    }

    #[tokio::test]
    async fn test_chat_handler_requires_channel_id() {
        let handler = ChatHandler::new();
        let ctx = make_context();

        let data_message = DataMessage {
            body: "Hello".to_string(),
            channel_id: String::new(), // Missing channel_id
            ..Default::default()
        };

        let content = Content {
            content: Some(content::Content::DataMessage(data_message)),
            ephemeral_reply_to: None,
            timestamp: 0,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed_message(plaintext);

        let result = handler.handle(&processed, &ctx).await;
        assert!(result.is_err());
        if let Err(HandlerError::ValidationFailed { reason }) = result {
            assert!(reason.contains("channel_id"));
        } else {
            panic!("Expected ValidationFailed error");
        }
    }

    #[test]
    fn test_content_types() {
        let handler = ChatHandler::new();
        assert_eq!(handler.content_types(), &[ContentType::DataMessage]);
    }

    #[tokio::test]
    async fn test_chat_handler_parse_error() {
        let handler = ChatHandler::new();
        let ctx = make_context();

        // Invalid protobuf bytes
        let processed = make_processed_message(vec![0xFF, 0xFF, 0xFF]);

        let result = handler.handle(&processed, &ctx).await;
        assert!(result.is_err());
        assert!(matches!(result, Err(HandlerError::ParseFailed { .. })));
    }
}
