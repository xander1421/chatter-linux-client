//! Pin/unpin event handler.
//!
//! Handle incoming message pin/unpin events. Pins mark important messages
//! in channels for easy reference.

use std::str::FromStr;

use prost::Message as ProstMessage;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::domain::{ChannelId, MessageId, UserId};
use crate::inbox::processor::{ContentType, ProcessedMessage};
use crate::proto::content_v1::{content, Content, PinMessage};

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
// PinHandler
// ============================================================================

/// Handler for PinMessage (message pin/unpin events).
///
/// Pin events mark or unmark messages as important in a channel.
/// Pinned messages are typically shown in a dedicated UI section
/// for easy access.
///
/// # Signal Protocol
///
/// Pin messages contain:
/// - `target_author_aci`: Author of the target message
/// - `target_sent_timestamp`: Timestamp of the target message (message ID)
/// - `unpin`: True to unpin, false to pin
/// - `channel_id`: Channel containing the message
///
/// # Stateless Pattern
///
/// PinHandler has no fields and takes all context via parameters.
pub struct PinHandler;

impl PinHandler {
    /// Create a new pin handler.
    pub fn new() -> Self {
        Self
    }
}

impl Default for PinHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl InboxHandler for PinHandler {
    async fn handle(&self, message: &ProcessedMessage, ctx: &HandlerContext) -> HandlerResult {
        // Decode Content proto from plaintext
        let content: Content = ProstMessage::decode(message.plaintext.as_slice())
            .map_err(|e| HandlerError::parse_failed(e.to_string()))?;

        // Extract PinMessage from oneof
        let pin: PinMessage = match content.content {
            Some(content::Content::Pin(p)) => p,
            _ => return Ok(HandlerOutcome::Ignored),
        };

        // Validate channel_id is non-empty
        if pin.channel_id.is_empty() {
            return Err(HandlerError::validation_failed(
                "channel_id is empty",
            ));
        }

        // Validate target_author_aci is non-empty
        if pin.target_author_aci.is_empty() {
            return Err(HandlerError::validation_failed(
                "target_author_aci is empty",
            ));
        }

        // Validate target_sent_timestamp is non-zero
        if pin.target_sent_timestamp == 0 {
            return Err(HandlerError::validation_failed(
                "target_sent_timestamp must be non-zero",
            ));
        }

        // Get pinner from message sender, or use local_user_id for sealed sender
        let pinner_id = message
            .sender_address
            .as_ref()
            .map(|addr| {
                UserId::from_str(addr.name()).unwrap_or_else(|_| {
                    // Fallback: generate deterministic ID from address name
                    UserId::from_uuid(uuid_from_string(addr.name()))
                })
            })
            .unwrap_or(ctx.local_user_id);

        // Parse channel_id as ChannelId
        let channel_id = ChannelId::from_str(&pin.channel_id)
            .unwrap_or_else(|_| {
                ChannelId::from_uuid(uuid_from_string(&pin.channel_id))
            });

        // Derive message ID from target timestamp (Signal uses timestamps as message IDs)
        let message_id = MessageId::from_uuid(uuid_from_timestamp(pin.target_sent_timestamp));

        // Emit appropriate event based on unpin flag
        if pin.unpin {
            ctx.emit(DomainEvent::MessageUnpinned {
                message_id,
                unpinner_id: pinner_id,
                channel_id,
            });
        } else {
            ctx.emit(DomainEvent::MessagePinned {
                message_id,
                pinner_id,
                channel_id,
            });
        }

        Ok(HandlerOutcome::Processed)
    }

    fn content_types(&self) -> &[ContentType] {
        // PinMessage is embedded in the Content proto oneof (tag 5).
        // It doesn't map directly to a top-level ContentType.
        // Similar to ReactionHandler, we return empty and rely on Content parsing.
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
            ContentType::DataMessage,
            plaintext,
            1234567890000,
            EnvelopeType::SealedSender,
        )
    }

    #[tokio::test]
    async fn test_pin_handler_pin_message() {
        let handler = PinHandler::new();
        let (ctx, mut rx) = make_context();

        let pin = PinMessage {
            target_author_aci: Uuid::new_v4().to_string(),
            target_sent_timestamp: 1704067200000,
            unpin: false,
            timestamp: 1704067201000,
            channel_id: Uuid::new_v4().to_string(),
        };

        let content = Content {
            content: Some(content::Content::Pin(pin.clone())),
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
        if let DomainEvent::MessagePinned {
            message_id,
            channel_id,
            ..
        } = event
        {
            // Message ID derived from timestamp
            let expected_message_id = MessageId::from_uuid(uuid_from_timestamp(1704067200000));
            assert_eq!(message_id, expected_message_id);
            assert_eq!(channel_id.to_string(), pin.channel_id);
        } else {
            panic!("Expected MessagePinned event");
        }
    }

    #[tokio::test]
    async fn test_pin_handler_unpin_message() {
        let handler = PinHandler::new();
        let (ctx, mut rx) = make_context();

        let pin = PinMessage {
            target_author_aci: Uuid::new_v4().to_string(),
            target_sent_timestamp: 1704067200000,
            unpin: true, // Unpin!
            timestamp: 1704067201000,
            channel_id: Uuid::new_v4().to_string(),
        };

        let content = Content {
            content: Some(content::Content::Pin(pin.clone())),
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
        if let DomainEvent::MessageUnpinned {
            message_id,
            channel_id,
            ..
        } = event
        {
            let expected_message_id = MessageId::from_uuid(uuid_from_timestamp(1704067200000));
            assert_eq!(message_id, expected_message_id);
            assert_eq!(channel_id.to_string(), pin.channel_id);
        } else {
            panic!("Expected MessageUnpinned event");
        }
    }

    #[tokio::test]
    async fn test_pin_handler_missing_channel_id() {
        let handler = PinHandler::new();
        let (ctx, _rx) = make_context();

        let pin = PinMessage {
            target_author_aci: Uuid::new_v4().to_string(),
            target_sent_timestamp: 1704067200000,
            unpin: false,
            timestamp: 1704067201000,
            channel_id: String::new(), // Invalid: empty
        };

        let content = Content {
            content: Some(content::Content::Pin(pin)),
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
            assert!(reason.contains("channel_id"));
        } else {
            panic!("Expected ValidationFailed error");
        }
    }

    #[tokio::test]
    async fn test_pin_handler_missing_target_author() {
        let handler = PinHandler::new();
        let (ctx, _rx) = make_context();

        let pin = PinMessage {
            target_author_aci: String::new(), // Invalid: empty
            target_sent_timestamp: 1704067200000,
            unpin: false,
            timestamp: 1704067201000,
            channel_id: Uuid::new_v4().to_string(),
        };

        let content = Content {
            content: Some(content::Content::Pin(pin)),
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
    async fn test_pin_handler_zero_timestamp() {
        let handler = PinHandler::new();
        let (ctx, _rx) = make_context();

        let pin = PinMessage {
            target_author_aci: Uuid::new_v4().to_string(),
            target_sent_timestamp: 0, // Invalid: zero
            unpin: false,
            timestamp: 1704067201000,
            channel_id: Uuid::new_v4().to_string(),
        };

        let content = Content {
            content: Some(content::Content::Pin(pin)),
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
            assert!(reason.contains("target_sent_timestamp"));
        } else {
            panic!("Expected ValidationFailed error");
        }
    }

    #[tokio::test]
    async fn test_pin_handler_ignores_non_pin() {
        let handler = PinHandler::new();
        let (ctx, _rx) = make_context();

        // Create a Content with typing message instead of pin
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
    async fn test_pin_handler_uses_local_user_when_no_sender() {
        let handler = PinHandler::new();
        let (ctx, mut rx) = make_context();

        let pin = PinMessage {
            target_author_aci: Uuid::new_v4().to_string(),
            target_sent_timestamp: 1704067200000,
            unpin: false,
            timestamp: 1704067201000,
            channel_id: Uuid::new_v4().to_string(),
        };

        let content = Content {
            content: Some(content::Content::Pin(pin)),
            ephemeral_reply_to: None,
            timestamp: 1704067201000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();

        // Create message without sender (sealed sender case)
        let processed = ProcessedMessage::new(
            "msg-123".to_string(),
            None, // No sender
            ContentType::DataMessage,
            plaintext,
            1234567890000,
            EnvelopeType::SealedSender,
        );

        let result = handler.handle(&processed, &ctx).await.unwrap();
        assert!(result.is_success());

        // Verify event uses local_user_id as pinner
        let event = rx.try_recv().unwrap();
        if let DomainEvent::MessagePinned { pinner_id, .. } = event {
            assert_eq!(pinner_id, ctx.local_user_id);
        } else {
            panic!("Expected MessagePinned event");
        }
    }

    #[test]
    fn test_content_types() {
        let handler = PinHandler::new();
        // PinMessage is embedded in Content proto, not a top-level ContentType
        assert!(handler.content_types().is_empty());
    }
}
