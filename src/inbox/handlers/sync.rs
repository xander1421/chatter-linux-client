//! Multi-device sync message handler.
//!
//! Handle sync messages for multi-device support, specifically DeleteForMeSync
//! which syncs local message deletions across devices.

use std::str::FromStr;

use prost::Message as ProstMessage;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::domain::{ChannelId, MessageId, UserId};
use crate::inbox::processor::{ContentType, ProcessedMessage};
use crate::proto::content_v1::{content, Content, DeleteForMeSync};

use super::{DomainEvent, HandlerContext, HandlerError, HandlerOutcome, HandlerResult, InboxHandler};

// ============================================================================
// UUID Helpers
// ============================================================================

/// Create a deterministic UUID from bytes.
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

// ============================================================================
// SyncHandler
// ============================================================================

/// Handler for multi-device sync messages.
///
/// Currently handles:
/// - `DeleteForMeSync` - Sync local message deletion to other devices
///
/// # Signal Protocol
///
/// Delete-for-me is different from delete-for-everyone:
/// - Delete-for-everyone: Removes message for all recipients (MessageDeleted event)
/// - Delete-for-me: Only removes message locally across user's devices (DeleteForMeSync event)
///
/// The DeleteForMeSync message contains:
/// - `conversation_id`: Channel/conversation where the message exists
/// - `target_author_aci`: Author of the message being deleted (UUID)
/// - `target_sent_timestamp`: Timestamp identifying the message (ms since epoch)
/// - `timestamp`: When this sync was issued (ms since epoch)
///
/// # Stateless Pattern
///
/// SyncHandler has no fields and takes all context via parameters.
pub struct SyncHandler;

impl SyncHandler {
    /// Create a new sync handler.
    pub fn new() -> Self {
        Self
    }
}

impl Default for SyncHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl InboxHandler for SyncHandler {
    async fn handle(&self, message: &ProcessedMessage, ctx: &HandlerContext) -> HandlerResult {
        // Decode Content proto from plaintext
        let content: Content = ProstMessage::decode(message.plaintext.as_slice())
            .map_err(|e| HandlerError::parse_failed(e.to_string()))?;

        // Extract DeleteForMeSync from oneof
        let delete_sync: DeleteForMeSync = match content.content {
            Some(content::Content::DeleteForMeSync(d)) => d,
            _ => return Ok(HandlerOutcome::Ignored),
        };

        // Validate conversation_id is non-empty
        if delete_sync.conversation_id.is_empty() {
            return Err(HandlerError::validation_failed(
                "conversation_id is empty",
            ));
        }

        // Validate target_author_aci is non-empty
        if delete_sync.target_author_aci.is_empty() {
            return Err(HandlerError::validation_failed(
                "target_author_aci is empty",
            ));
        }

        // Validate target_sent_timestamp is non-zero
        if delete_sync.target_sent_timestamp == 0 {
            return Err(HandlerError::validation_failed(
                "target_sent_timestamp must be non-zero",
            ));
        }

        // Parse conversation_id as ChannelId
        let conversation_id = ChannelId::from_str(&delete_sync.conversation_id)
            .unwrap_or_else(|_| {
                ChannelId::from_uuid(uuid_from_string(&delete_sync.conversation_id))
            });

        // Parse target_author_aci as UserId
        let target_author_id = UserId::from_str(&delete_sync.target_author_aci)
            .unwrap_or_else(|_| {
                UserId::from_uuid(uuid_from_string(&delete_sync.target_author_aci))
            });

        // Derive message ID from target timestamp (Signal uses timestamps as message IDs)
        let target_message_id = MessageId::from_uuid(uuid_from_timestamp(delete_sync.target_sent_timestamp));

        // Emit DeleteForMeSync event
        ctx.emit(DomainEvent::DeleteForMeSync {
            conversation_id,
            target_author_id,
            target_message_id,
            timestamp: delete_sync.timestamp,
        });

        Ok(HandlerOutcome::Processed)
    }

    fn content_types(&self) -> &[ContentType] {
        // DeleteForMeSync is routed via ContentType::SyncMessage (type field 2)
        &[ContentType::SyncMessage]
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
    async fn test_sync_handler_valid_delete_for_me() {
        let handler = SyncHandler::new();
        let (ctx, mut rx) = make_context();

        let conversation_id = Uuid::new_v4().to_string();
        let target_author_aci = Uuid::new_v4().to_string();

        let delete_sync = DeleteForMeSync {
            conversation_id: conversation_id.clone(),
            target_author_aci: target_author_aci.clone(),
            target_sent_timestamp: 1704067200000,
            timestamp: 1704067201000,
        };

        let content = Content {
            content: Some(content::Content::DeleteForMeSync(delete_sync)),
            ephemeral_reply_to: None,
            timestamp: 1704067201000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed_message(plaintext, ContentType::SyncMessage);

        let result = handler.handle(&processed, &ctx).await.unwrap();
        assert!(result.is_success());

        // Verify event was emitted
        let event = rx.try_recv().unwrap();
        if let DomainEvent::DeleteForMeSync {
            conversation_id: cid,
            target_author_id,
            target_message_id,
            timestamp,
        } = event
        {
            assert_eq!(cid.to_string(), conversation_id);
            assert_eq!(target_author_id.to_string(), target_author_aci);
            // Verify message ID is derived from timestamp
            let expected_message_id = MessageId::from_uuid(uuid_from_timestamp(1704067200000));
            assert_eq!(target_message_id, expected_message_id);
            assert_eq!(timestamp, 1704067201000);
        } else {
            panic!("Expected DeleteForMeSync event");
        }
    }

    #[tokio::test]
    async fn test_sync_handler_missing_conversation_id() {
        let handler = SyncHandler::new();
        let (ctx, _rx) = make_context();

        let delete_sync = DeleteForMeSync {
            conversation_id: String::new(), // Invalid: empty
            target_author_aci: Uuid::new_v4().to_string(),
            target_sent_timestamp: 1704067200000,
            timestamp: 1704067201000,
        };

        let content = Content {
            content: Some(content::Content::DeleteForMeSync(delete_sync)),
            ephemeral_reply_to: None,
            timestamp: 1704067201000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed_message(plaintext, ContentType::SyncMessage);

        let result = handler.handle(&processed, &ctx).await;
        assert!(result.is_err());
        if let Err(HandlerError::ValidationFailed { reason }) = result {
            assert!(reason.contains("conversation_id"));
        } else {
            panic!("Expected ValidationFailed error");
        }
    }

    #[tokio::test]
    async fn test_sync_handler_invalid_target_author_aci() {
        let handler = SyncHandler::new();
        let (ctx, _rx) = make_context();

        let delete_sync = DeleteForMeSync {
            conversation_id: Uuid::new_v4().to_string(),
            target_author_aci: String::new(), // Invalid: empty
            target_sent_timestamp: 1704067200000,
            timestamp: 1704067201000,
        };

        let content = Content {
            content: Some(content::Content::DeleteForMeSync(delete_sync)),
            ephemeral_reply_to: None,
            timestamp: 1704067201000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed_message(plaintext, ContentType::SyncMessage);

        let result = handler.handle(&processed, &ctx).await;
        assert!(result.is_err());
        if let Err(HandlerError::ValidationFailed { reason }) = result {
            assert!(reason.contains("target_author_aci"));
        } else {
            panic!("Expected ValidationFailed error");
        }
    }

    #[tokio::test]
    async fn test_sync_handler_zero_timestamp() {
        let handler = SyncHandler::new();
        let (ctx, _rx) = make_context();

        let delete_sync = DeleteForMeSync {
            conversation_id: Uuid::new_v4().to_string(),
            target_author_aci: Uuid::new_v4().to_string(),
            target_sent_timestamp: 0, // Invalid: zero
            timestamp: 1704067201000,
        };

        let content = Content {
            content: Some(content::Content::DeleteForMeSync(delete_sync)),
            ephemeral_reply_to: None,
            timestamp: 1704067201000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed_message(plaintext, ContentType::SyncMessage);

        let result = handler.handle(&processed, &ctx).await;
        assert!(result.is_err());
        if let Err(HandlerError::ValidationFailed { reason }) = result {
            assert!(reason.contains("target_sent_timestamp"));
        } else {
            panic!("Expected ValidationFailed error");
        }
    }

    #[tokio::test]
    async fn test_sync_handler_ignores_non_delete_sync() {
        let handler = SyncHandler::new();
        let (ctx, _rx) = make_context();

        // Create a Content with typing message instead of delete sync
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
        let processed = make_processed_message(plaintext, ContentType::SyncMessage);

        let result = handler.handle(&processed, &ctx).await.unwrap();
        assert!(result.is_ignored());
    }

    #[test]
    fn test_content_types() {
        let handler = SyncHandler::new();
        assert_eq!(handler.content_types(), &[ContentType::SyncMessage]);
    }

    #[test]
    fn test_uuid_from_timestamp_deterministic() {
        let uuid1 = uuid_from_timestamp(1234567890);
        let uuid2 = uuid_from_timestamp(1234567890);
        assert_eq!(uuid1, uuid2);

        let uuid3 = uuid_from_timestamp(9876543210);
        assert_ne!(uuid1, uuid3);
    }
}
