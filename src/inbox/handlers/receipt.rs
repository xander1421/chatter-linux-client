//! Read/delivery receipt handler.
//!
//! Handle incoming receipts for message delivery confirmation and read status.
//! Receipts are transient messages that update message state without creating
//! new messages.

use std::str::FromStr;

use prost::Message as ProstMessage;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::domain::UserId;
use crate::inbox::processor::{ContentType, ProcessedMessage};
use crate::proto::content_v1::{content, receipt_message, Content, ReceiptMessage};

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
// ReceiptHandler
// ============================================================================

/// Handler for ReceiptMessage (delivery/read/viewed receipts).
///
/// Receipts inform the sender that their messages have been:
/// - Delivered to the recipient's device
/// - Read by the recipient
/// - Viewed (for view-once/ephemeral messages)
///
/// # Signal Protocol
///
/// Signal uses message timestamps as identifiers. A receipt contains a list
/// of timestamps indicating which messages the receipt applies to.
///
/// # Receipt Types
///
/// - `Delivery`: Message reached the recipient's device
/// - `Read`: Recipient opened/viewed the message
/// - `Viewed`: Recipient viewed a view-once message (triggers deletion)
///
/// # Stateless Pattern
///
/// ReceiptHandler has no fields and takes all context via parameters.
pub struct ReceiptHandler;

impl ReceiptHandler {
    /// Create a new receipt handler.
    pub fn new() -> Self {
        Self
    }
}

impl Default for ReceiptHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl InboxHandler for ReceiptHandler {
    async fn handle(&self, message: &ProcessedMessage, ctx: &HandlerContext) -> HandlerResult {
        // Decode Content proto from plaintext
        let content: Content = ProstMessage::decode(message.plaintext.as_slice())
            .map_err(|e| HandlerError::parse_failed(e.to_string()))?;

        // Extract ReceiptMessage from oneof
        let receipt: ReceiptMessage = match content.content {
            Some(content::Content::Receipt(r)) => r,
            _ => return Ok(HandlerOutcome::Ignored),
        };

        // Get sender (who sent the receipt)
        let from_user_id = message
            .sender_address
            .as_ref()
            .map(|addr| {
                UserId::from_str(addr.name()).unwrap_or_else(|_| {
                    // Fallback: generate deterministic ID from address name
                    UserId::from_uuid(uuid_from_string(addr.name()))
                })
            })
            .ok_or_else(|| HandlerError::missing_context("sender_address"))?;

        // Empty timestamps list is technically valid (no-op)
        if receipt.timestamps.is_empty() {
            tracing::debug!("Received receipt with empty timestamps, treating as no-op");
            return Ok(HandlerOutcome::Processed);
        }

        // Dispatch based on receipt type
        let receipt_type = receipt_message::Type::try_from(receipt.r#type)
            .unwrap_or(receipt_message::Type::Unspecified);

        match receipt_type {
            receipt_message::Type::Delivery => {
                ctx.emit(DomainEvent::MessagesDelivered {
                    recipient_id: from_user_id,
                    message_timestamps: receipt.timestamps.clone(),
                    receipt_timestamp: receipt.timestamp,
                });
            }
            receipt_message::Type::Read => {
                ctx.emit(DomainEvent::MessagesRead {
                    reader_id: from_user_id,
                    message_timestamps: receipt.timestamps.clone(),
                    receipt_timestamp: receipt.timestamp,
                });
            }
            receipt_message::Type::Viewed => {
                // Viewed receipts are typically for single messages (view-once)
                // Emit one event per timestamp for granular handling
                for ts in &receipt.timestamps {
                    ctx.emit(DomainEvent::ViewedReceipt {
                        message_timestamp: *ts,
                        from_user_id,
                        receipt_timestamp: receipt.timestamp,
                    });
                }
            }
            receipt_message::Type::Unspecified => {
                tracing::warn!("Received receipt with unspecified type, ignoring");
                return Ok(HandlerOutcome::Ignored);
            }
        }

        Ok(HandlerOutcome::Processed)
    }

    fn content_types(&self) -> &[ContentType] {
        &[ContentType::ReceiptMessage]
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
            ContentType::ReceiptMessage,
            plaintext,
            1234567890000,
            EnvelopeType::SealedSender,
        )
    }

    #[tokio::test]
    async fn test_receipt_handler_delivery() {
        let handler = ReceiptHandler::new();
        let (ctx, mut rx) = make_context();

        let receipt = ReceiptMessage {
            r#type: receipt_message::Type::Delivery as i32,
            timestamps: vec![1704067200000, 1704067201000],
            timestamp: 1704067202000,
        };

        let content = Content {
            content: Some(content::Content::Receipt(receipt)),
            ephemeral_reply_to: None,
            timestamp: 1704067202000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed_message(plaintext);

        let result = handler.handle(&processed, &ctx).await.unwrap();
        assert!(result.is_success());

        // Verify event was emitted
        let event = rx.try_recv().unwrap();
        if let DomainEvent::MessagesDelivered {
            message_timestamps,
            receipt_timestamp,
            ..
        } = event
        {
            assert_eq!(message_timestamps.len(), 2);
            assert_eq!(message_timestamps[0], 1704067200000);
            assert_eq!(message_timestamps[1], 1704067201000);
            assert_eq!(receipt_timestamp, 1704067202000);
        } else {
            panic!("Expected MessagesDelivered event");
        }
    }

    #[tokio::test]
    async fn test_receipt_handler_read() {
        let handler = ReceiptHandler::new();
        let (ctx, mut rx) = make_context();

        let receipt = ReceiptMessage {
            r#type: receipt_message::Type::Read as i32,
            timestamps: vec![1704067200000],
            timestamp: 1704067203000,
        };

        let content = Content {
            content: Some(content::Content::Receipt(receipt)),
            ephemeral_reply_to: None,
            timestamp: 1704067203000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed_message(plaintext);

        let result = handler.handle(&processed, &ctx).await.unwrap();
        assert!(result.is_success());

        // Verify event was emitted
        let event = rx.try_recv().unwrap();
        if let DomainEvent::MessagesRead {
            message_timestamps,
            receipt_timestamp,
            ..
        } = event
        {
            assert_eq!(message_timestamps.len(), 1);
            assert_eq!(message_timestamps[0], 1704067200000);
            assert_eq!(receipt_timestamp, 1704067203000);
        } else {
            panic!("Expected MessagesRead event");
        }
    }

    #[tokio::test]
    async fn test_receipt_handler_viewed() {
        let handler = ReceiptHandler::new();
        let (ctx, mut rx) = make_context();

        let receipt = ReceiptMessage {
            r#type: receipt_message::Type::Viewed as i32,
            timestamps: vec![1704067200000, 1704067201000],
            timestamp: 1704067204000,
        };

        let content = Content {
            content: Some(content::Content::Receipt(receipt)),
            ephemeral_reply_to: None,
            timestamp: 1704067204000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed_message(plaintext);

        let result = handler.handle(&processed, &ctx).await.unwrap();
        assert!(result.is_success());

        // Verify events were emitted (one per timestamp)
        let event1 = rx.try_recv().unwrap();
        if let DomainEvent::ViewedReceipt {
            message_timestamp,
            receipt_timestamp,
            ..
        } = event1
        {
            assert_eq!(message_timestamp, 1704067200000);
            assert_eq!(receipt_timestamp, 1704067204000);
        } else {
            panic!("Expected ViewedReceipt event");
        }

        let event2 = rx.try_recv().unwrap();
        if let DomainEvent::ViewedReceipt {
            message_timestamp, ..
        } = event2
        {
            assert_eq!(message_timestamp, 1704067201000);
        } else {
            panic!("Expected second ViewedReceipt event");
        }
    }

    #[tokio::test]
    async fn test_receipt_handler_empty_timestamps() {
        let handler = ReceiptHandler::new();
        let (ctx, _rx) = make_context();

        let receipt = ReceiptMessage {
            r#type: receipt_message::Type::Read as i32,
            timestamps: vec![], // Empty - should be processed as no-op
            timestamp: 1704067200000,
        };

        let content = Content {
            content: Some(content::Content::Receipt(receipt)),
            ephemeral_reply_to: None,
            timestamp: 1704067200000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed_message(plaintext);

        let result = handler.handle(&processed, &ctx).await.unwrap();
        assert!(result.is_success()); // Processed as no-op, not an error
    }

    #[tokio::test]
    async fn test_receipt_handler_unspecified_type() {
        let handler = ReceiptHandler::new();
        let (ctx, _rx) = make_context();

        let receipt = ReceiptMessage {
            r#type: receipt_message::Type::Unspecified as i32,
            timestamps: vec![1704067200000],
            timestamp: 1704067200000,
        };

        let content = Content {
            content: Some(content::Content::Receipt(receipt)),
            ephemeral_reply_to: None,
            timestamp: 1704067200000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed_message(plaintext);

        let result = handler.handle(&processed, &ctx).await.unwrap();
        assert!(result.is_ignored()); // Unspecified type is ignored
    }

    #[tokio::test]
    async fn test_receipt_handler_ignores_non_receipt() {
        let handler = ReceiptHandler::new();
        let (ctx, _rx) = make_context();

        // Create a Content with typing message instead of receipt
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
    async fn test_receipt_handler_requires_sender() {
        let handler = ReceiptHandler::new();
        let (ctx, _rx) = make_context();

        let receipt = ReceiptMessage {
            r#type: receipt_message::Type::Read as i32,
            timestamps: vec![1704067200000],
            timestamp: 1704067200000,
        };

        let content = Content {
            content: Some(content::Content::Receipt(receipt)),
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
            ContentType::ReceiptMessage,
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
        let handler = ReceiptHandler::new();
        assert_eq!(handler.content_types(), &[ContentType::ReceiptMessage]);
    }

    #[tokio::test]
    async fn test_receipt_handler_parse_error() {
        let handler = ReceiptHandler::new();
        let (ctx, _rx) = make_context();

        // Invalid protobuf bytes
        let processed = make_processed_message(vec![0xFF, 0xFF, 0xFF]);

        let result = handler.handle(&processed, &ctx).await;
        assert!(result.is_err());
        assert!(matches!(result, Err(HandlerError::ParseFailed { .. })));
    }
}
