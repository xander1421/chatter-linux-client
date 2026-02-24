//! Attachment content handler.
//!
//! Handle incoming standalone AttachmentMessage content. Most attachments
//! come embedded in DataMessage, but standalone AttachmentMessage is used for:
//! - Large file metadata sent separately
//! - Attachment re-sync across devices
//! - Forwarded attachments

use std::str::FromStr;

use prost::Message as ProstMessage;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::domain::{
    Attachment, AttachmentId, AttachmentStatus, ChannelId, FileMetadata, MessageId,
};
use crate::inbox::processor::{ContentType, ProcessedMessage};
use crate::proto::content_v1::{content, AttachmentMessage, AttachmentPointer, Content};

use super::{DomainEvent, HandlerContext, HandlerError, HandlerOutcome, HandlerResult, InboxHandler};

// ============================================================================
// UUID Helpers
// ============================================================================

/// Create a deterministic UUID from bytes using SHA256.
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
// AttachmentHandler
// ============================================================================

/// Handler for standalone AttachmentMessage.
///
/// Processes attachment pointers and emits AttachmentReady events for
/// download manager to fetch the encrypted blob from CDN.
///
/// # Stateless Pattern
///
/// AttachmentHandler has no fields and takes all context via parameters.
pub struct AttachmentHandler;

impl AttachmentHandler {
    /// Create a new attachment handler.
    pub fn new() -> Self {
        Self
    }

    /// Build a domain Attachment from AttachmentPointer proto.
    fn build_attachment(
        &self,
        pointer: &AttachmentPointer,
    ) -> Result<Attachment, HandlerError> {
        // Debug: log what we received
        tracing::info!(
            "AttachmentPointer received: cdn_key={}, file_name={}, content_type={}, \
             thumbnail_bytes={}, thumbnail_size={}x{}, width={}, height={}, waveform_len={}",
            pointer.cdn_key,
            pointer.file_name,
            pointer.content_type,
            pointer.thumbnail.len(),
            pointer.thumbnail_width,
            pointer.thumbnail_height,
            pointer.width,
            pointer.height,
            pointer.waveform.len()
        );

        // Validate required fields
        if pointer.cdn_key.is_empty() {
            return Err(HandlerError::validation_failed("missing cdn_key"));
        }

        // Generate deterministic attachment ID from cdn_key
        let attachment_id = AttachmentId::from_uuid(uuid_from_string(&pointer.cdn_key));

        // Build file metadata with sender-provided preview data
        let metadata = FileMetadata {
            name: pointer.file_name.clone(),
            mime_type: pointer.content_type.clone(),
            size: pointer.plaintext_size,
            sha256: pointer.digest.clone(),
            chunk_size: 0, // Not chunked for incoming attachments
            width: if pointer.width > 0 {
                Some(pointer.width)
            } else {
                None
            },
            height: if pointer.height > 0 {
                Some(pointer.height)
            } else {
                None
            },
            duration_seconds: if pointer.duration_ms > 0 {
                Some(pointer.duration_ms as f64 / 1000.0)
            } else {
                None
            },
            // Use sender-provided waveform (32 samples, 0-255)
            waveform_samples: if !pointer.waveform.is_empty() {
                Some(pointer.waveform.clone())
            } else {
                None
            },
            // Use sender-provided thumbnail for instant preview
            thumbnail: if !pointer.thumbnail.is_empty() {
                Some(pointer.thumbnail.clone())
            } else {
                None
            },
            thumbnail_width: if pointer.thumbnail_width > 0 {
                Some(pointer.thumbnail_width)
            } else {
                None
            },
            thumbnail_height: if pointer.thumbnail_height > 0 {
                Some(pointer.thumbnail_height)
            } else {
                None
            },
        };

        // Build attachment with pending status (needs download)
        // Content will be stored in database when downloaded via download_to_database()
        let mut attachment = Attachment::new(attachment_id, metadata);
        attachment.status = AttachmentStatus::Pending;
        // content_location remains NotDownloaded (set by Attachment::new)
        attachment.remote_url = Some(pointer.cdn_key.clone());
        attachment.encryption_key = if !pointer.encryption_key.is_empty() {
            Some(pointer.encryption_key.clone())
        } else {
            None
        };
        // Store digest for verification during decryption
        attachment.encryption_iv = if !pointer.digest.is_empty() {
            Some(pointer.digest.clone())
        } else {
            None
        };

        Ok(attachment)
    }

    /// Extract channel_id from AttachmentMessage.
    fn get_channel_id(&self, msg: &AttachmentMessage) -> Result<ChannelId, HandlerError> {
        if !msg.channel_id.is_empty() {
            ChannelId::from_str(&msg.channel_id).map_err(|_| {
                HandlerError::validation_failed(format!("invalid channel_id: {}", msg.channel_id))
            })
        } else {
            // Default to nil channel for unknown context
            Ok(ChannelId::from_uuid(Uuid::nil()))
        }
    }

    /// Get message ID from timestamp.
    fn get_message_id(&self, msg: &AttachmentMessage) -> MessageId {
        if msg.timestamp > 0 {
            MessageId::from_uuid(uuid_from_timestamp(msg.timestamp))
        } else {
            MessageId::new()
        }
    }
}

impl Default for AttachmentHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl InboxHandler for AttachmentHandler {
    async fn handle(&self, message: &ProcessedMessage, ctx: &HandlerContext) -> HandlerResult {
        // Decode Content proto from plaintext
        let content: Content = ProstMessage::decode(message.plaintext.as_slice())
            .map_err(|e| HandlerError::parse_failed(e.to_string()))?;

        // Extract AttachmentMessage from oneof
        let attachment_msg = match content.content {
            Some(content::Content::Attachment(am)) => am,
            _ => return Ok(HandlerOutcome::Ignored),
        };

        // Get attachment pointer (required)
        let pointer = attachment_msg
            .attachment
            .as_ref()
            .ok_or_else(|| HandlerError::validation_failed("missing attachment pointer"))?;

        // Build Attachment domain object
        let attachment = self.build_attachment(pointer)?;

        // Get message and channel context
        let message_id = self.get_message_id(&attachment_msg);
        let channel_id = self.get_channel_id(&attachment_msg)?;

        // Emit event
        ctx.emit(DomainEvent::AttachmentReady {
            attachment,
            message_id,
            channel_id,
        });

        Ok(HandlerOutcome::Processed)
    }

    fn content_types(&self) -> &[ContentType] {
        // Note: AttachmentMessage uses the same ContentType routing as DataMessage
        // since it's in the content oneof. The handler checks the variant.
        &[ContentType::DataMessage]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libsignal_protocol::{DeviceId, ProtocolAddress};
    use tokio::sync::broadcast;
    use crate::proto::mailbox_v1::EnvelopeType;

    fn make_context() -> HandlerContext {
        let (tx, _rx) = broadcast::channel(16);
        HandlerContext::new(crate::domain::UserId::new(), tx)
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
    async fn test_attachment_handler_ignores_non_attachment() {
        let handler = AttachmentHandler::new();
        let ctx = make_context();

        // Create a Content with typing message instead
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
    async fn test_attachment_handler_processes_attachment() {
        let handler = AttachmentHandler::new();
        let (tx, mut rx) = broadcast::channel(16);
        let ctx = HandlerContext::new(crate::domain::UserId::new(), tx);

        let channel_id = crate::domain::ChannelId::new();
        let pointer = AttachmentPointer {
            cdn_key: "test-cdn-key-12345".to_string(),
            encryption_key: vec![1, 2, 3, 4],
            digest: vec![5, 6, 7, 8],
            plaintext_size: 1024,
            content_type: "image/png".to_string(),
            file_name: "test.png".to_string(),
            width: 800,
            height: 600,
            ..Default::default()
        };

        let attachment_msg = AttachmentMessage {
            attachment: Some(pointer),
            caption: "Test image".to_string(),
            timestamp: 1704067200000,
            channel_id: channel_id.to_string(),
            author_name: "Alice".to_string(),
        };

        let content = Content {
            content: Some(content::Content::Attachment(attachment_msg)),
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
        if let DomainEvent::AttachmentReady {
            attachment,
            message_id: _,
            channel_id: ch_id,
        } = event
        {
            assert_eq!(attachment.metadata.name, "test.png");
            assert_eq!(attachment.metadata.mime_type, "image/png");
            assert_eq!(attachment.metadata.size, 1024);
            assert_eq!(attachment.metadata.width, Some(800));
            assert_eq!(attachment.metadata.height, Some(600));
            assert_eq!(attachment.status, AttachmentStatus::Pending);
            assert_eq!(ch_id, channel_id);
        } else {
            panic!("Expected AttachmentReady event");
        }
    }

    #[tokio::test]
    async fn test_attachment_handler_requires_cdn_key() {
        let handler = AttachmentHandler::new();
        let ctx = make_context();

        let pointer = AttachmentPointer {
            cdn_key: String::new(), // Missing cdn_key
            ..Default::default()
        };

        let attachment_msg = AttachmentMessage {
            attachment: Some(pointer),
            ..Default::default()
        };

        let content = Content {
            content: Some(content::Content::Attachment(attachment_msg)),
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
            assert!(reason.contains("cdn_key"));
        } else {
            panic!("Expected ValidationFailed error");
        }
    }

    #[tokio::test]
    async fn test_attachment_handler_requires_pointer() {
        let handler = AttachmentHandler::new();
        let ctx = make_context();

        let attachment_msg = AttachmentMessage {
            attachment: None, // Missing pointer
            ..Default::default()
        };

        let content = Content {
            content: Some(content::Content::Attachment(attachment_msg)),
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
            assert!(reason.contains("attachment pointer"));
        } else {
            panic!("Expected ValidationFailed error");
        }
    }

    #[test]
    fn test_content_types() {
        let handler = AttachmentHandler::new();
        assert_eq!(handler.content_types(), &[ContentType::DataMessage]);
    }

    #[tokio::test]
    async fn test_attachment_handler_parse_error() {
        let handler = AttachmentHandler::new();
        let ctx = make_context();

        // Invalid protobuf bytes
        let processed = make_processed_message(vec![0xFF, 0xFF, 0xFF]);

        let result = handler.handle(&processed, &ctx).await;
        assert!(result.is_err());
        assert!(matches!(result, Err(HandlerError::ParseFailed { .. })));
    }

    #[test]
    fn test_uuid_from_string_deterministic() {
        let uuid1 = uuid_from_string("test-key");
        let uuid2 = uuid_from_string("test-key");
        assert_eq!(uuid1, uuid2);

        let uuid3 = uuid_from_string("different-key");
        assert_ne!(uuid1, uuid3);
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
