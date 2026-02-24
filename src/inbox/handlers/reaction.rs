//! Reaction handler.
//!
//! Handle incoming emoji reactions to messages. Reactions toggle on/off
//! based on the `remove` flag in the proto message.

use std::str::FromStr;

use prost::Message as ProstMessage;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::domain::{MessageId, UserId};
use crate::inbox::processor::{ContentType, ProcessedMessage};
use crate::proto::content_v1::{content, Content, Reaction};

use super::{DomainEvent, HandlerContext, HandlerError, HandlerOutcome, HandlerResult, InboxHandler};

// ============================================================================
// UUID Helpers
// ============================================================================

/// Create a deterministic UUID from a u64 timestamp.
fn uuid_from_timestamp(ts: u64) -> Uuid {
    let hash = Sha256::digest(ts.to_le_bytes());
    let mut uuid_bytes = [0u8; 16];
    uuid_bytes.copy_from_slice(&hash[..16]);
    // Set version 4 (random) and variant 1 (RFC 4122)
    uuid_bytes[6] = (uuid_bytes[6] & 0x0f) | 0x40;
    uuid_bytes[8] = (uuid_bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(uuid_bytes)
}

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
// ReactionHandler
// ============================================================================

/// Handler for Reaction messages (emoji reactions to messages).
///
/// Reactions are toggle-based: the same user sending the same emoji
/// to the same message toggles it on/off. The `remove` flag indicates
/// whether this is an add or remove operation.
///
/// # Signal Protocol
///
/// Reactions identify target messages by (author_aci, sent_timestamp) tuple.
/// This matches Signal's message identification scheme.
///
/// # Stateless Pattern
///
/// ReactionHandler has no fields and takes all context via parameters.
pub struct ReactionHandler;

impl ReactionHandler {
    /// Create a new reaction handler.
    pub fn new() -> Self {
        Self
    }
}

impl Default for ReactionHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl InboxHandler for ReactionHandler {
    async fn handle(&self, message: &ProcessedMessage, ctx: &HandlerContext) -> HandlerResult {
        // Decode Content proto from plaintext
        let content: Content = ProstMessage::decode(message.plaintext.as_slice())
            .map_err(|e| HandlerError::parse_failed(e.to_string()))?;

        // Extract Reaction from oneof
        let reaction: Reaction = match content.content {
            Some(content::Content::Reaction(r)) => r,
            _ => return Ok(HandlerOutcome::Ignored),
        };

        // Validate required fields
        if reaction.emoji.is_empty() {
            return Err(HandlerError::validation_failed("reaction emoji is empty"));
        }

        if reaction.target_author_aci.is_empty() {
            return Err(HandlerError::validation_failed(
                "reaction target_author_aci is empty",
            ));
        }

        // Get reactor from message sender
        let reactor_id = message
            .sender_address
            .as_ref()
            .map(|addr| {
                UserId::from_str(addr.name()).unwrap_or_else(|_| {
                    // Fallback: generate deterministic ID from address name
                    UserId::from_uuid(uuid_from_string(addr.name()))
                })
            })
            .ok_or_else(|| HandlerError::missing_context("sender_address"))?;

        // Parse target message author from ACI
        let author_id =
            UserId::from_str(&reaction.target_author_aci).unwrap_or_else(|_| {
                UserId::from_uuid(uuid_from_string(&reaction.target_author_aci))
            });

        // Derive message ID from target timestamp (Signal uses timestamps as message IDs)
        let message_id = MessageId::from_uuid(uuid_from_timestamp(reaction.target_sent_timestamp));

        // Emit appropriate event based on remove flag
        let action = if reaction.remove { "remove" } else { "add" };
        tracing::info!(
            ?message_id,
            ?author_id,
            ?reactor_id,
            emoji = %reaction.emoji,
            action = %action,
            timestamp = reaction.timestamp,
            "Inbox handler: processing reaction"
        );

        let event = if reaction.remove {
            DomainEvent::ReactionRemoved {
                message_id,
                author_id,
                reactor_id,
                emoji: reaction.emoji.clone(),
                timestamp: reaction.timestamp,
            }
        } else {
            DomainEvent::ReactionAdded {
                message_id,
                author_id,
                reactor_id,
                emoji: reaction.emoji.clone(),
                timestamp: reaction.timestamp,
            }
        };

        ctx.emit(event);
        tracing::debug!(action = %action, "Reaction event emitted");
        Ok(HandlerOutcome::Processed)
    }

    fn content_types(&self) -> &[ContentType] {
        // Reaction is not a top-level ContentType in processor.rs
        // It's embedded in the Content proto oneof. We handle DataMessage routing
        // but this handler is for the Reaction content type specifically.
        // For now, return empty - the router will need to inspect Content proto.
        &[]
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
            ContentType::DataMessage, // Placeholder, actual type determined by proto
            plaintext,
            1234567890000,
            EnvelopeType::SealedSender,
        )
    }

    #[tokio::test]
    async fn test_reaction_handler_add_reaction() {
        let handler = ReactionHandler::new();
        let (ctx, mut rx) = make_context();

        let reaction = Reaction {
            emoji: "👍".to_string(),
            remove: false,
            target_author_aci: Uuid::new_v4().to_string(),
            target_sent_timestamp: 1704067200000,
            timestamp: 1704067201000,
        };

        let content = Content {
            content: Some(content::Content::Reaction(reaction)),
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
        if let DomainEvent::ReactionAdded {
            emoji, timestamp, ..
        } = event
        {
            assert_eq!(emoji, "👍");
            assert_eq!(timestamp, 1704067201000);
        } else {
            panic!("Expected ReactionAdded event");
        }
    }

    #[tokio::test]
    async fn test_reaction_handler_remove_reaction() {
        let handler = ReactionHandler::new();
        let (ctx, mut rx) = make_context();

        let reaction = Reaction {
            emoji: "❤️".to_string(),
            remove: true,
            target_author_aci: Uuid::new_v4().to_string(),
            target_sent_timestamp: 1704067200000,
            timestamp: 1704067201000,
        };

        let content = Content {
            content: Some(content::Content::Reaction(reaction)),
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
        if let DomainEvent::ReactionRemoved { emoji, .. } = event {
            assert_eq!(emoji, "❤️");
        } else {
            panic!("Expected ReactionRemoved event");
        }
    }

    #[tokio::test]
    async fn test_reaction_handler_validates_empty_emoji() {
        let handler = ReactionHandler::new();
        let (ctx, _rx) = make_context();

        let reaction = Reaction {
            emoji: String::new(), // Invalid: empty
            remove: false,
            target_author_aci: Uuid::new_v4().to_string(),
            target_sent_timestamp: 1704067200000,
            timestamp: 1704067201000,
        };

        let content = Content {
            content: Some(content::Content::Reaction(reaction)),
            ephemeral_reply_to: None,
            timestamp: 1704067201000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed_message(plaintext);

        let result = handler.handle(&processed, &ctx).await;
        assert!(result.is_err());
        if let Err(HandlerError::ValidationFailed { reason }) = result {
            assert!(reason.contains("emoji"));
        } else {
            panic!("Expected ValidationFailed error");
        }
    }

    #[tokio::test]
    async fn test_reaction_handler_validates_missing_author() {
        let handler = ReactionHandler::new();
        let (ctx, _rx) = make_context();

        let reaction = Reaction {
            emoji: "👍".to_string(),
            remove: false,
            target_author_aci: String::new(), // Invalid: empty
            target_sent_timestamp: 1704067200000,
            timestamp: 1704067201000,
        };

        let content = Content {
            content: Some(content::Content::Reaction(reaction)),
            ephemeral_reply_to: None,
            timestamp: 1704067201000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed_message(plaintext);

        let result = handler.handle(&processed, &ctx).await;
        assert!(result.is_err());
        if let Err(HandlerError::ValidationFailed { reason }) = result {
            assert!(reason.contains("target_author_aci"));
        } else {
            panic!("Expected ValidationFailed error");
        }
    }

    #[tokio::test]
    async fn test_reaction_handler_ignores_non_reaction() {
        let handler = ReactionHandler::new();
        let (ctx, _rx) = make_context();

        // Create a Content with typing message instead of reaction
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
    async fn test_reaction_handler_requires_sender() {
        let handler = ReactionHandler::new();
        let (ctx, _rx) = make_context();

        let reaction = Reaction {
            emoji: "👍".to_string(),
            remove: false,
            target_author_aci: Uuid::new_v4().to_string(),
            target_sent_timestamp: 1704067200000,
            timestamp: 1704067201000,
        };

        let content = Content {
            content: Some(content::Content::Reaction(reaction)),
            ephemeral_reply_to: None,
            timestamp: 1704067201000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();

        // Create message without sender
        let processed = ProcessedMessage::new(
            "msg-123".to_string(),
            None, // No sender
            ContentType::DataMessage,
            plaintext,
            1234567890000,
            EnvelopeType::SealedSender,
        );

        let result = handler.handle(&processed, &ctx).await;
        assert!(result.is_err());
        assert!(matches!(result, Err(HandlerError::MissingContext { .. })));
    }

    #[test]
    fn test_content_types() {
        let handler = ReactionHandler::new();
        // Reactions are embedded in Content proto, not a top-level ContentType
        assert!(handler.content_types().is_empty());
    }
}
