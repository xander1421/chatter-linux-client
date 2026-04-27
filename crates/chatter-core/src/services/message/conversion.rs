//! Proto message conversion utilities.
//!
//! Convert between proto messages and domain messages.

use chrono::Utc;

use crate::domain::ids::{AttachmentId, ChannelId, IntegrationId, MessageId};
use crate::domain::message::{ConversationType, Message, MessageContent, MixedContent, ReactionCount};
use crate::proto::chat_v1;
use crate::storage::repositories::channel_repo::ChannelRepository;
use crate::util::time::TimestampNanos;

use super::MessageError;

/// Derive the `ConversationType` for a given channel ID via
/// `Channel::conversation_type()`. Defaults to `Channel` when the row
/// isn't available yet (channel sync race). Use at RPC boundaries that
/// hand proto Messages to `proto_to_domain_message_with_sender` —
/// prevents every caller from repeating the same lookup boilerplate and
/// keeps the mapping logic in one place.
pub fn conversation_type_for_channel(
    repo: &ChannelRepository,
    channel_id: &ChannelId,
) -> ConversationType {
    match repo.get(channel_id) {
        Ok(Some(ch)) => ch.conversation_type(),
        Ok(None) => {
            tracing::debug!(
                channel_id = %channel_id,
                "conversation_type_for_channel: channel row missing, defaulting to Channel"
            );
            ConversationType::Channel
        }
        Err(e) => {
            tracing::warn!(
                channel_id = %channel_id,
                error = %e,
                "conversation_type_for_channel: repo error, defaulting to Channel"
            );
            ConversationType::Channel
        }
    }
}

/// Convert a proto Message to a domain Message.
///
/// `conversation_type` must be supplied by the caller — the proto does
/// not carry it, so the caller is responsible for deriving it from the
/// target channel (usually `Channel::conversation_type()` on the row
/// they're fetching into). Passing the wrong variant causes the message
/// to be written to the wrong index bucket in the sqlite `messages`
/// table (see domain/message.rs idx_messages_* indexes).
pub fn proto_to_domain_message(
    proto: &chat_v1::Message,
    sender_id: crate::domain::ids::UserId,
    conversation_type: ConversationType,
) -> Result<Message, MessageError> {
    proto_to_domain_message_with_sender(proto, Some(sender_id), conversation_type)
}

/// Convert a proto Message to a domain Message with optional sender.
///
/// See `proto_to_domain_message` for the `conversation_type` contract.
pub fn proto_to_domain_message_with_sender(
    proto: &chat_v1::Message,
    sender_id: Option<crate::domain::ids::UserId>,
    conversation_type: ConversationType,
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

    // Extract attachment IDs from content
    let attachments = extract_attachment_ids_from_content(proto.content.as_ref());

    // Convert reaction counts
    let reaction_counts: Vec<ReactionCount> = proto
        .reaction_counts
        .iter()
        .map(|rc| ReactionCount::new(rc.emoji.clone(), rc.count as u32))
        .collect();

    // Convert timestamp (proto uses nanos)
    let created_at = TimestampNanos::from_nanos(proto.timestamp).to_datetime().unwrap_or_else(Utc::now);
    let updated_at = if proto.updated_at > 0 {
        TimestampNanos::from_nanos(proto.updated_at).to_datetime().unwrap_or(created_at)
    } else {
        created_at
    };
    let edited_at = if proto.edited && proto.edited_at > 0 {
        TimestampNanos::from_nanos(proto.edited_at).to_datetime()
    } else {
        None
    };

    Ok(Message {
        id,
        conversation_id: channel_id,
        conversation_type,
        sender_id,
        device_id: if proto.author_device_id != 0 { Some(proto.author_device_id) } else { None },
        content,
        encrypted_content: None,
        thread_id,
        reply_to: None, // Not exposed in proto yet
        reply_to_author_id: None,
        reply_to_text: None,
        reply_to_thumbnail: None,
        reply_to_thumbnail_mime: None,
        attachments,
        is_pinned: proto.pinned,
        is_edited: proto.edited,
        is_deleted: false,
        is_read: false,
        sent_at: proto.timestamp,
        created_at,
        updated_at,
        edited_at,
        reaction_counts,
        current_user_reactions: proto.current_user_reactions.clone(),
        mentioned_integrations,
        targets_ai_bot: proto.targets_ai_bot,
        delivery_status: String::new(),
    })
}

/// Extracted quote context (author display name, preview text) from a
/// stored `content_json` blob on anonymous DM/group messages.
///
/// Lives in core because the JSON schema is part of the anonymous-message
/// storage contract — not presentation.
pub fn extract_quote_from_content_json(content_json: &str) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(content_json).ok()?;
    let quote = v.get("quote")?;
    // Prefer stored display name, fall back to UUID.
    let author_name = quote
        .get("author_name")
        .and_then(|n| n.as_str())
        .filter(|n| !n.is_empty())
        .or_else(|| quote.get("author_id").and_then(|n| n.as_str()))
        .unwrap_or_default()
        .to_string();
    let preview = quote.get("preview")?.as_str().unwrap_or_default().to_string();
    if author_name.is_empty() && preview.is_empty() {
        return None;
    }
    Some((author_name, preview))
}

/// Extract attachment IDs from proto message content.
fn extract_attachment_ids_from_content(
    proto_content: Option<&chat_v1::message::Content>,
) -> Vec<AttachmentId> {
    let Some(content) = proto_content else {
        return Vec::new();
    };

    let Some(ref content_type) = content.r#type else {
        return Vec::new();
    };

    match content_type {
        chat_v1::message::content::Type::File(file_meta) => {
            // Single file attachment
            if let Ok(id) = file_meta.id.parse() {
                vec![id]
            } else {
                Vec::new()
            }
        }
        chat_v1::message::content::Type::Mixed(mixed) => {
            // Multiple file attachments
            mixed
                .files
                .iter()
                .filter_map(|f| f.id.parse().ok())
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Convert proto content to domain MessageContent.
pub fn proto_content_to_domain(
    proto_content: Option<&chat_v1::message::Content>,
) -> Result<MessageContent, MessageError> {
    let content = proto_content.ok_or_else(|| {
        MessageError::InvalidContent("message has no content".to_string())
    })?;

    match &content.r#type {
        Some(chat_v1::message::content::Type::Text(text)) => {
            // Check for [GIF:url] markers
            if text.contains("[GIF:") {
                // We keep it as Text for now but we could potentially tag it.
                // The important part is that we handle it before word breaks.
                Ok(MessageContent::Text(text.clone()))
            } else {
                Ok(MessageContent::Text(text.clone()))
            }
        }
        Some(chat_v1::message::content::Type::Encrypted(_)) => {
            // Encrypted content - store as placeholder text for now
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
        Some(chat_v1::message::content::Type::Mixed(mixed)) => {
            // Text + attachments (how encrypted channel messages arrive post-refactor).
            let attachment_ids: Vec<AttachmentId> = mixed
                .files
                .iter()
                .filter_map(|f| f.id.parse().ok())
                .collect();
            let text = mixed.text.clone();
            let content = match (text.is_empty(), attachment_ids.is_empty()) {
                (false, false) => MixedContent::new(text, attachment_ids),
                (false, true) => MixedContent::text_only(text),
                (true, false) => MixedContent::attachments_only(attachment_ids),
                (true, true) => MixedContent::text_only(String::new()),
            };
            Ok(MessageContent::Mixed(content))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::ChannelId;

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
    fn test_nanos_to_datetime() {
        let nanos = 1706140800_000_000_000u64; // 2024-01-25 00:00:00 UTC in nanoseconds
        let dt = TimestampNanos::from_nanos(nanos).to_datetime().unwrap();
        assert_eq!(dt.timestamp_nanos_opt().unwrap(), nanos as i64);
    }

    #[test]
    fn test_proto_content_to_domain_encrypted() {
        use chat_v1::message::content::Type;
        use chat_v1::message::Content;

        let proto_content = Content {
            r#type: Some(Type::Encrypted(chat_v1::EncryptedData {
                version: 1,
                ciphertext: vec![1, 2, 3],
                nonce: vec![],
                auth_tag: vec![],
                signature: vec![],
                associated_data: vec![],
                device_id: String::new(),
            })),
        };

        let result = proto_content_to_domain(Some(&proto_content)).unwrap();
        match result {
            MessageContent::Text(text) => assert!(text.contains("Encrypted")),
            _ => panic!("Expected Text variant"),
        }
    }

    #[test]
    fn test_proto_content_to_domain_file() {
        use chat_v1::message::content::Type;
        use chat_v1::message::Content;

        let file_id = uuid::Uuid::new_v4().to_string();
        let proto_content = Content {
            r#type: Some(Type::File(chat_v1::FileMetadata {
                id: file_id.clone(),
                name: "test.pdf".to_string(),
                size: 1024,
                mime_type: "application/pdf".to_string(),
                sha256: vec![],
                chunk_size: 0,
            })),
        };

        let result = proto_content_to_domain(Some(&proto_content)).unwrap();
        match result {
            MessageContent::Mixed(mixed) => {
                assert_eq!(mixed.attachment_ids.len(), 1);
            }
            _ => panic!("Expected Mixed variant"),
        }
    }

    #[test]
    fn test_proto_content_to_domain_empty_type() {
        use chat_v1::message::Content;

        let proto_content = Content { r#type: None };

        let result = proto_content_to_domain(Some(&proto_content)).unwrap();
        match result {
            MessageContent::Text(text) => assert!(text.is_empty()),
            _ => panic!("Expected Text variant"),
        }
    }

    #[test]
    fn test_proto_content_to_domain_reaction() {
        use chat_v1::message::content::Type;
        use chat_v1::message::Content;

        // Reaction content should return placeholder
        let proto_content = Content {
            r#type: Some(Type::Reaction(chat_v1::ReactionUpdate {
                target_author_aci: "550e8400-e29b-41d4-a716-446655440000".to_string(),
                target_sent_timestamp: 1704067200_000_000_000,
                emoji: "👍".to_string(),
                add: true,
                proof_of_work: vec![],
            })),
        };

        let result = proto_content_to_domain(Some(&proto_content)).unwrap();
        match result {
            MessageContent::Text(text) => {
                assert_eq!(text, "[Special message]");
            }
            _ => panic!("Expected Text variant with placeholder"),
        }
    }

    #[test]
    fn test_file_id_parsing_error() {
        use chat_v1::message::content::Type;
        use chat_v1::message::Content;

        // Invalid file ID should return error
        let proto_content = Content {
            r#type: Some(Type::File(chat_v1::FileMetadata {
                id: "not-a-valid-uuid".to_string(),
                name: "test.pdf".to_string(),
                mime_type: "application/pdf".to_string(),
                size: 1024,
                sha256: vec![0xAB, 0xCD],
                chunk_size: 256,
            })),
        };

        let result = proto_content_to_domain(Some(&proto_content));
        assert!(result.is_err());
    }
}
