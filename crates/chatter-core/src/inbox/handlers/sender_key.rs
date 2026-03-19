//! Sender key distribution handler.
//!
//! Handle incoming sender key distribution messages for group messaging.
//! Sender keys enable efficient N-to-N encryption in group channels.

use std::str::FromStr;

use prost::Message as ProstMessage;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::domain::{ChannelId, UserId};
use crate::inbox::processor::{ContentType, ProcessedMessage};
use crate::proto::content_v1::{content, Content, SenderKeyDistribution};

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
// SenderKeyHandler
// ============================================================================

/// Handler for SenderKeyDistribution messages (group sender key distribution).
///
/// Sender key distribution messages share the sender's group encryption key
/// with other members. This enables efficient group encryption where each
/// sender has one key that all group members can decrypt with.
///
/// # Signal Protocol
///
/// Sender key distribution contains:
/// - `channel_id`: The group/channel this key is for
/// - `distribution_message`: Serialized SKDM from libsignal (opaque bytes)
/// - `timestamp`: When the distribution was sent
///
/// The `distribution_message` is passed to the crypto layer (SenderKeyManager)
/// for processing. This handler just parses and routes.
///
/// # Stateless Pattern
///
/// SenderKeyHandler has no fields and takes all context via parameters.
pub struct SenderKeyHandler;

impl SenderKeyHandler {
    /// Create a new sender key handler.
    pub fn new() -> Self {
        Self
    }
}

impl Default for SenderKeyHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl InboxHandler for SenderKeyHandler {
    async fn handle(&self, message: &ProcessedMessage, ctx: &HandlerContext) -> HandlerResult {
        // Decode Content proto from plaintext
        let content: Content = ProstMessage::decode(message.plaintext.as_slice())
            .map_err(|e| HandlerError::parse_failed(e.to_string()))?;

        // Extract SenderKeyDistribution from oneof
        let sender_key: SenderKeyDistribution = match content.content {
            Some(content::Content::SenderKey(sk)) => sk,
            _ => return Ok(HandlerOutcome::Ignored),
        };

        // Validate distribution_message is non-empty
        if sender_key.distribution_message.is_empty() {
            return Err(HandlerError::validation_failed(
                "distribution_message is empty",
            ));
        }

        // Validate channel_id is non-empty
        if sender_key.channel_id.is_empty() {
            return Err(HandlerError::validation_failed(
                "channel_id is empty",
            ));
        }

        // Get sender from message sender (required for sender key)
        let sender_address = message
            .sender_address
            .as_ref()
            .ok_or_else(|| HandlerError::missing_context("sender_address"))?;

        let sender_id = UserId::from_str(sender_address.name()).unwrap_or_else(|_| {
            // Fallback: generate deterministic ID from address name
            UserId::from_uuid(uuid_from_string(sender_address.name()))
        });

        let sender_device_id: u32 = sender_address.device_id().into();

        // Parse channel_id as ChannelId
        let channel_id = ChannelId::from_str(&sender_key.channel_id)
            .unwrap_or_else(|_| {
                ChannelId::from_uuid(uuid_from_string(&sender_key.channel_id))
            });

        // Extract sender's mailbox public key from ephemeral_reply_to (NDSS '21)
        // This is bundled with SKDM so we learn the sender's group mailbox address
        let sender_mailbox_pk: Option<[u8; 32]> = content.ephemeral_reply_to
            .as_ref()
            .and_then(|pk| {
                if pk.len() == 32 {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(pk);
                    Some(arr)
                } else {
                    tracing::warn!(
                        "Invalid ephemeral_reply_to length: {} (expected 32)",
                        pk.len()
                    );
                    None
                }
            });

        // Note: The actual SKDM processing happens in MessageGateway::route_encrypted_message()
        // -> process_sender_key_distribution(). This handler is for the HandlerRegistry path
        // which emits domain events for UI/state updates.

        // Emit SenderKeyReceived event (for UI/state updates)
        ctx.emit(DomainEvent::SenderKeyReceived {
            sender_id,
            sender_device_id,
            channel_id,
            distribution_message: sender_key.distribution_message.clone(),
            timestamp: sender_key.timestamp,
        });

        // If sender included their mailbox address, emit GroupMailboxReceived event
        if let Some(mailbox_pk) = sender_mailbox_pk {
            ctx.emit(DomainEvent::GroupMailboxReceived {
                sender_id,
                channel_id,
                mailbox_public_key: mailbox_pk,
                timestamp: content.timestamp,
            });
            tracing::debug!(
                channel_id = %channel_id,
                sender_id = %sender_id,
                "Extracted sender's group mailbox from SKDM"
            );
        }

        Ok(HandlerOutcome::Processed)
    }

    fn content_types(&self) -> &[ContentType] {
        // SenderKeyDistribution maps to ContentType::SenderKeyDistribution (type field 7)
        &[ContentType::SenderKeyDistribution]
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

    fn make_processed_message(plaintext: Vec<u8>, content_type: ContentType) -> ProcessedMessage {
        ProcessedMessage::new(
            "msg-123".to_string(),
            Some(ProtocolAddress::new(
                Uuid::new_v4().to_string(),
                DeviceId::try_from(1u32).unwrap(),
            )),
            content_type,
            plaintext,
            1234567890000,
            EnvelopeType::SealedSender,
        )
    }

    #[tokio::test]
    async fn test_sender_key_handler_valid_distribution() {
        let handler = SenderKeyHandler::new();
        let (ctx, mut rx) = make_context();

        // Create valid sender key distribution
        let distribution_message: Vec<u8> = vec![0x01, 0x02, 0x03, 0x04]; // Mock SKDM

        let sender_key = SenderKeyDistribution {
            channel_id: Uuid::new_v4().to_string(),
            distribution_message: distribution_message.clone(),
            timestamp: 1704067200000,
        };

        let content = Content {
            content: Some(content::Content::SenderKey(sender_key.clone())),
            ephemeral_reply_to: None,
            timestamp: 1704067200000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed_message(plaintext, ContentType::SenderKeyDistribution);

        let result = handler.handle(&processed, &ctx).await.unwrap();
        assert!(result.is_success());

        // Verify event was emitted
        let event = rx.try_recv().unwrap();
        if let DomainEvent::SenderKeyReceived {
            channel_id,
            distribution_message: dm,
            timestamp,
            ..
        } = event
        {
            assert_eq!(channel_id.to_string(), sender_key.channel_id);
            assert_eq!(dm, distribution_message);
            assert_eq!(timestamp, 1704067200000);
        } else {
            panic!("Expected SenderKeyReceived event");
        }
    }

    #[tokio::test]
    async fn test_sender_key_handler_empty_distribution_message() {
        let handler = SenderKeyHandler::new();
        let (ctx, _rx) = make_context();

        let sender_key = SenderKeyDistribution {
            channel_id: Uuid::new_v4().to_string(),
            distribution_message: vec![], // Invalid: empty
            timestamp: 1704067200000,
        };

        let content = Content {
            content: Some(content::Content::SenderKey(sender_key)),
            ephemeral_reply_to: None,
            timestamp: 1704067200000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed_message(plaintext, ContentType::SenderKeyDistribution);

        let result = handler.handle(&processed, &ctx).await;
        assert!(result.is_err());
        if let Err(HandlerError::ValidationFailed { reason }) = result {
            assert!(reason.contains("distribution_message"));
        } else {
            panic!("Expected ValidationFailed error");
        }
    }

    #[tokio::test]
    async fn test_sender_key_handler_missing_sender() {
        let handler = SenderKeyHandler::new();
        let (ctx, _rx) = make_context();

        let sender_key = SenderKeyDistribution {
            channel_id: Uuid::new_v4().to_string(),
            distribution_message: vec![0x01, 0x02],
            timestamp: 1704067200000,
        };

        let content = Content {
            content: Some(content::Content::SenderKey(sender_key)),
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
            ContentType::SenderKeyDistribution,
            plaintext,
            1234567890000,
            EnvelopeType::SealedSender,
        );

        let result = handler.handle(&processed, &ctx).await;
        assert!(result.is_err());
        assert!(matches!(result, Err(HandlerError::MissingContext { .. })));
    }

    #[tokio::test]
    async fn test_sender_key_handler_ignores_non_sender_key() {
        let handler = SenderKeyHandler::new();
        let (ctx, _rx) = make_context();

        // Create a Content with typing message instead of sender key
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
        let processed = make_processed_message(plaintext, ContentType::SenderKeyDistribution);

        let result = handler.handle(&processed, &ctx).await.unwrap();
        assert!(result.is_ignored());
    }

    #[tokio::test]
    async fn test_sender_key_handler_empty_channel_id() {
        let handler = SenderKeyHandler::new();
        let (ctx, _rx) = make_context();

        let sender_key = SenderKeyDistribution {
            channel_id: String::new(), // Invalid: empty
            distribution_message: vec![0x01, 0x02],
            timestamp: 1704067200000,
        };

        let content = Content {
            content: Some(content::Content::SenderKey(sender_key)),
            ephemeral_reply_to: None,
            timestamp: 1704067200000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed_message(plaintext, ContentType::SenderKeyDistribution);

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
        let handler = SenderKeyHandler::new();
        assert_eq!(handler.content_types(), &[ContentType::SenderKeyDistribution]);
    }
}
