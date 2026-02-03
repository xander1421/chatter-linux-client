//! Typing indicator handler.
//!
//! Handle incoming typing indicator messages. Typing indicators are transient
//! events that show when another user is actively typing in a conversation.

use std::str::FromStr;

use prost::Message as ProstMessage;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::domain::{ChannelId, UserId};
use crate::inbox::processor::{ContentType, ProcessedMessage};
use crate::proto::content_v1::{content, typing_message, Content, TypingMessage};

use super::{DomainEvent, HandlerContext, HandlerError, HandlerOutcome, HandlerResult, InboxHandler};

// ============================================================================
// UUID Helper
// ============================================================================

/// Create a deterministic UUID from a string.
fn uuid_from_string(s: &str) -> Uuid {
    let hash = Sha256::digest(s.as_bytes());
    let mut uuid_bytes = [0u8; 16];
    uuid_bytes.copy_from_slice(&hash[..16]);
    uuid_bytes[6] = (uuid_bytes[6] & 0x0f) | 0x40;
    uuid_bytes[8] = (uuid_bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(uuid_bytes)
}

// ============================================================================
// TypingHandler
// ============================================================================

/// Handler for TypingMessage (typing indicators).
///
/// Typing indicators inform other participants that a user is actively typing
/// in a conversation. They are transient events that:
/// - Should not be persisted to storage
/// - Typically expire after 10 seconds (per Phase 20 decision)
/// - Are used to update UI typing indicators
///
/// # Signal Protocol
///
/// Typing messages contain:
/// - `action`: Started or Stopped
/// - `timestamp`: When the indicator was generated
/// - `channel_id`: Optional channel ID (empty for DMs)
///
/// For DMs, the channel is derived from the sender and local user IDs.
///
/// # Stateless Pattern
///
/// TypingHandler has no fields and takes all context via parameters.
pub struct TypingHandler;

impl TypingHandler {
    /// Create a new typing handler.
    pub fn new() -> Self {
        Self
    }
}

impl Default for TypingHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl InboxHandler for TypingHandler {
    async fn handle(&self, message: &ProcessedMessage, ctx: &HandlerContext) -> HandlerResult {
        // Decode Content proto from plaintext
        let content: Content = ProstMessage::decode(message.plaintext.as_slice())
            .map_err(|e| HandlerError::parse_failed(e.to_string()))?;

        // Extract TypingMessage from oneof
        let typing: TypingMessage = match content.content {
            Some(content::Content::Typing(t)) => t,
            _ => return Ok(HandlerOutcome::Ignored),
        };

        // Get user who is typing from message sender
        let user_id = message
            .sender_address
            .as_ref()
            .map(|addr| {
                UserId::from_str(addr.name()).unwrap_or_else(|_| {
                    // Fallback: generate deterministic ID from address name
                    UserId::from_uuid(uuid_from_string(addr.name()))
                })
            })
            .ok_or_else(|| HandlerError::missing_context("sender_address"))?;

        // Parse channel_id - empty string means DM
        let channel_id = if typing.channel_id.is_empty() {
            // For DMs, derive channel from sender and local user
            ChannelId::from_dm_participants(&user_id, &ctx.local_user_id)
        } else {
            // Parse channel ID from string
            ChannelId::from_str(&typing.channel_id).unwrap_or_else(|_| {
                // Fallback: generate deterministic ID from channel string
                ChannelId::from_uuid(uuid_from_string(&typing.channel_id))
            })
        };

        // Dispatch based on action
        let action = typing_message::Action::try_from(typing.action)
            .unwrap_or(typing_message::Action::Unspecified);

        match action {
            typing_message::Action::Started => {
                ctx.emit(DomainEvent::TypingStarted {
                    user_id,
                    channel_id,
                });
            }
            typing_message::Action::Stopped => {
                ctx.emit(DomainEvent::TypingStopped {
                    user_id,
                    channel_id,
                });
            }
            typing_message::Action::Unspecified => {
                tracing::debug!("Received typing indicator with unspecified action, ignoring");
                return Ok(HandlerOutcome::Ignored);
            }
        }

        Ok(HandlerOutcome::Processed)
    }

    fn content_types(&self) -> &[ContentType] {
        &[ContentType::TypingMessage]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::mailbox_v1::EnvelopeType;
    use libsignal_protocol::{DeviceId, ProtocolAddress};
    use tokio::sync::broadcast;

    fn make_context() -> (HandlerContext, broadcast::Receiver<DomainEvent>) {
        let (tx, rx) = broadcast::channel(16);
        (HandlerContext::new(UserId::new(), tx), rx)
    }

    fn make_processed_message(plaintext: Vec<u8>) -> ProcessedMessage {
        ProcessedMessage::new(
            "msg-123".to_string(),
            Some(ProtocolAddress::new(
                Uuid::new_v4().to_string(),
                DeviceId::try_from(1u32).unwrap(),
            )),
            ContentType::TypingMessage,
            plaintext,
            1234567890000,
            EnvelopeType::SealedSender,
        )
    }

    #[tokio::test]
    async fn test_typing_handler_started() {
        let handler = TypingHandler::new();
        let (ctx, mut rx) = make_context();

        let typing = TypingMessage {
            action: typing_message::Action::Started as i32,
            timestamp: 1704067200000,
            channel_id: Uuid::new_v4().to_string(),
        };

        let content = Content {
            content: Some(content::Content::Typing(typing.clone())),
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
        if let DomainEvent::TypingStarted { channel_id, .. } = event {
            // Channel ID should match (parsed from string)
            assert_eq!(channel_id.to_string(), typing.channel_id);
        } else {
            panic!("Expected TypingStarted event");
        }
    }

    #[tokio::test]
    async fn test_typing_handler_stopped() {
        let handler = TypingHandler::new();
        let (ctx, mut rx) = make_context();

        let typing = TypingMessage {
            action: typing_message::Action::Stopped as i32,
            timestamp: 1704067201000,
            channel_id: Uuid::new_v4().to_string(),
        };

        let content = Content {
            content: Some(content::Content::Typing(typing)),
            ephemeral_reply_to: None,
            timestamp: 1704067201000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed_message(plaintext);

        let result = handler.handle(&processed, &ctx).await.unwrap();
        assert!(result.is_success());

        // Verify event was emitted
        let event = rx.try_recv().unwrap();
        assert!(matches!(event, DomainEvent::TypingStopped { .. }));
    }

    #[tokio::test]
    async fn test_typing_handler_dm_channel_derivation() {
        let handler = TypingHandler::new();
        let (ctx, mut rx) = make_context();

        // Empty channel_id indicates DM - should derive from participants
        let typing = TypingMessage {
            action: typing_message::Action::Started as i32,
            timestamp: 1704067200000,
            channel_id: String::new(), // Empty = DM
        };

        let content = Content {
            content: Some(content::Content::Typing(typing)),
            ephemeral_reply_to: None,
            timestamp: 1704067200000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed_message(plaintext);

        let result = handler.handle(&processed, &ctx).await.unwrap();
        assert!(result.is_success());

        // Verify event was emitted with derived channel ID
        let event = rx.try_recv().unwrap();
        if let DomainEvent::TypingStarted {
            channel_id,
            user_id,
        } = event
        {
            // The channel should be derived from the sender and local user
            let expected_channel =
                ChannelId::from_dm_participants(&user_id, &ctx.local_user_id);
            assert_eq!(channel_id, expected_channel);
        } else {
            panic!("Expected TypingStarted event");
        }
    }

    #[tokio::test]
    async fn test_typing_handler_unspecified_action() {
        let handler = TypingHandler::new();
        let (ctx, _rx) = make_context();

        let typing = TypingMessage {
            action: typing_message::Action::Unspecified as i32,
            timestamp: 1704067200000,
            channel_id: String::new(),
        };

        let content = Content {
            content: Some(content::Content::Typing(typing)),
            ephemeral_reply_to: None,
            timestamp: 1704067200000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed_message(plaintext);

        let result = handler.handle(&processed, &ctx).await.unwrap();
        assert!(result.is_ignored()); // Unspecified action is ignored
    }

    #[tokio::test]
    async fn test_typing_handler_ignores_non_typing() {
        let handler = TypingHandler::new();
        let (ctx, _rx) = make_context();

        // Create a Content with receipt message instead of typing
        let content = Content {
            content: Some(content::Content::Receipt(
                crate::proto::content_v1::ReceiptMessage {
                    r#type: 1,
                    timestamps: vec![],
                    timestamp: 0,
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
    async fn test_typing_handler_requires_sender() {
        let handler = TypingHandler::new();
        let (ctx, _rx) = make_context();

        let typing = TypingMessage {
            action: typing_message::Action::Started as i32,
            timestamp: 1704067200000,
            channel_id: String::new(),
        };

        let content = Content {
            content: Some(content::Content::Typing(typing)),
            ephemeral_reply_to: None,
            timestamp: 1704067200000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();

        // Create message without sender
        let processed = ProcessedMessage::new(
            "msg-123".to_string(),
            None, // No sender
            ContentType::TypingMessage,
            plaintext,
            1234567890000,
            EnvelopeType::SealedSender,
        );

        let result = handler.handle(&processed, &ctx).await;
        assert!(result.is_err());
        assert!(matches!(result, Err(HandlerError::MissingContext { .. })));
    }

    #[tokio::test]
    async fn test_typing_handler_parse_error() {
        let handler = TypingHandler::new();
        let (ctx, _rx) = make_context();

        // Invalid protobuf bytes
        let processed = make_processed_message(vec![0xFF, 0xFF, 0xFF]);

        let result = handler.handle(&processed, &ctx).await;
        assert!(result.is_err());
        assert!(matches!(result, Err(HandlerError::ParseFailed { .. })));
    }

    #[test]
    fn test_content_types() {
        let handler = TypingHandler::new();
        assert_eq!(handler.content_types(), &[ContentType::TypingMessage]);
    }
}
