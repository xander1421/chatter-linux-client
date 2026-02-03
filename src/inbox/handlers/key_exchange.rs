//! Key exchange handler.
//!
//! Handle incoming ephemeral key exchange messages for mailbox encryption.
//! Key exchanges establish shared secrets for DM conversations.

use std::str::FromStr;

use prost::Message as ProstMessage;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::domain::{ChannelId, UserId};
use crate::inbox::processor::{ContentType, ProcessedMessage};
use crate::proto::content_v1::{content, Content, KeyExchange};

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
// KeyExchangeHandler
// ============================================================================

/// Handler for KeyExchange messages (mailbox key exchange).
///
/// Key exchange messages establish ephemeral keys for mailbox encryption in DMs.
/// They contain an Ed25519 public key (32 bytes) and the conversation ID.
///
/// # Signal Protocol
///
/// Key exchanges identify the sender and target conversation:
/// - `ephemeral_pk`: 32-byte Ed25519 public key
/// - `conversation_id`: Channel/conversation this key applies to
/// - `timestamp`: When the key exchange was initiated
///
/// # Stateless Pattern
///
/// KeyExchangeHandler has no fields and takes all context via parameters.
pub struct KeyExchangeHandler;

impl KeyExchangeHandler {
    /// Create a new key exchange handler.
    pub fn new() -> Self {
        Self
    }
}

impl Default for KeyExchangeHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl InboxHandler for KeyExchangeHandler {
    async fn handle(&self, message: &ProcessedMessage, ctx: &HandlerContext) -> HandlerResult {
        // Decode Content proto from plaintext
        let content: Content = ProstMessage::decode(message.plaintext.as_slice())
            .map_err(|e| HandlerError::parse_failed(e.to_string()))?;

        // Extract KeyExchange from oneof
        let key_exchange: KeyExchange = match content.content {
            Some(content::Content::KeyExchange(k)) => k,
            _ => return Ok(HandlerOutcome::Ignored),
        };

        // Validate ephemeral_pk is exactly 32 bytes (Ed25519 public key)
        if key_exchange.ephemeral_pk.len() != 32 {
            return Err(HandlerError::validation_failed(format!(
                "ephemeral_pk must be 32 bytes, got {}",
                key_exchange.ephemeral_pk.len()
            )));
        }

        // Validate conversation_id is non-empty
        if key_exchange.conversation_id.is_empty() {
            return Err(HandlerError::validation_failed(
                "conversation_id is empty",
            ));
        }

        // Get sender from message sender (required for key exchange)
        let sender_id = message
            .sender_address
            .as_ref()
            .map(|addr| {
                UserId::from_str(addr.name()).unwrap_or_else(|_| {
                    // Fallback: generate deterministic ID from address name
                    UserId::from_uuid(uuid_from_string(addr.name()))
                })
            })
            .ok_or_else(|| HandlerError::missing_context("sender_address"))?;

        // Parse conversation_id as ChannelId
        let conversation_id = ChannelId::from_str(&key_exchange.conversation_id)
            .unwrap_or_else(|_| {
                ChannelId::from_uuid(uuid_from_string(&key_exchange.conversation_id))
            });

        // Convert ephemeral_pk to fixed-size array
        let mut ephemeral_public_key = [0u8; 32];
        ephemeral_public_key.copy_from_slice(&key_exchange.ephemeral_pk);

        // Emit KeyExchangeReceived event
        ctx.emit(DomainEvent::KeyExchangeReceived {
            sender_id,
            conversation_id,
            ephemeral_public_key,
            timestamp: key_exchange.timestamp,
        });

        Ok(HandlerOutcome::Processed)
    }

    fn content_types(&self) -> &[ContentType] {
        // KeyExchange is embedded in the Content proto oneof (tag 7).
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
    async fn test_key_exchange_handler_valid_32_byte_key() {
        let handler = KeyExchangeHandler::new();
        let (ctx, mut rx) = make_context();

        // Create valid 32-byte public key
        let ephemeral_pk: Vec<u8> = (0..32).collect();

        let key_exchange = KeyExchange {
            ephemeral_pk,
            conversation_id: Uuid::new_v4().to_string(),
            timestamp: 1704067200000,
        };

        let content = Content {
            content: Some(content::Content::KeyExchange(key_exchange.clone())),
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
        if let DomainEvent::KeyExchangeReceived {
            conversation_id,
            ephemeral_public_key,
            timestamp,
            ..
        } = event
        {
            assert_eq!(conversation_id.to_string(), key_exchange.conversation_id);
            assert_eq!(&ephemeral_public_key[..], &key_exchange.ephemeral_pk[..]);
            assert_eq!(timestamp, 1704067200000);
        } else {
            panic!("Expected KeyExchangeReceived event");
        }
    }

    #[tokio::test]
    async fn test_key_exchange_handler_invalid_key_length() {
        let handler = KeyExchangeHandler::new();
        let (ctx, _rx) = make_context();

        // Create invalid 16-byte public key (should be 32)
        let ephemeral_pk: Vec<u8> = (0..16).collect();

        let key_exchange = KeyExchange {
            ephemeral_pk,
            conversation_id: Uuid::new_v4().to_string(),
            timestamp: 1704067200000,
        };

        let content = Content {
            content: Some(content::Content::KeyExchange(key_exchange)),
            ephemeral_reply_to: None,
            timestamp: 1704067200000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed_message(plaintext);

        let result = handler.handle(&processed, &ctx).await;
        assert!(result.is_err());
        if let Err(HandlerError::ValidationFailed { reason }) = result {
            assert!(reason.contains("32 bytes"));
            assert!(reason.contains("16"));
        } else {
            panic!("Expected ValidationFailed error");
        }
    }

    #[tokio::test]
    async fn test_key_exchange_handler_missing_sender() {
        let handler = KeyExchangeHandler::new();
        let (ctx, _rx) = make_context();

        let ephemeral_pk: Vec<u8> = (0..32).collect();

        let key_exchange = KeyExchange {
            ephemeral_pk,
            conversation_id: Uuid::new_v4().to_string(),
            timestamp: 1704067200000,
        };

        let content = Content {
            content: Some(content::Content::KeyExchange(key_exchange)),
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
            ContentType::DataMessage,
            plaintext,
            1234567890000,
            EnvelopeType::SealedSender,
        );

        let result = handler.handle(&processed, &ctx).await;
        assert!(result.is_err());
        assert!(matches!(result, Err(HandlerError::MissingContext { .. })));
    }

    #[tokio::test]
    async fn test_key_exchange_handler_ignores_non_key_exchange() {
        let handler = KeyExchangeHandler::new();
        let (ctx, _rx) = make_context();

        // Create a Content with typing message instead of key exchange
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
    async fn test_key_exchange_handler_empty_conversation_id() {
        let handler = KeyExchangeHandler::new();
        let (ctx, _rx) = make_context();

        let ephemeral_pk: Vec<u8> = (0..32).collect();

        let key_exchange = KeyExchange {
            ephemeral_pk,
            conversation_id: String::new(), // Invalid: empty
            timestamp: 1704067200000,
        };

        let content = Content {
            content: Some(content::Content::KeyExchange(key_exchange)),
            ephemeral_reply_to: None,
            timestamp: 1704067200000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed_message(plaintext);

        let result = handler.handle(&processed, &ctx).await;
        assert!(result.is_err());
        if let Err(HandlerError::ValidationFailed { reason }) = result {
            assert!(reason.contains("conversation_id"));
        } else {
            panic!("Expected ValidationFailed error");
        }
    }

    #[test]
    fn test_content_types() {
        let handler = KeyExchangeHandler::new();
        // KeyExchange is embedded in Content proto, not a top-level ContentType
        assert!(handler.content_types().is_empty());
    }
}
