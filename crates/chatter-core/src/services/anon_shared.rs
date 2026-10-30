//! Shared construction helpers for anonymous messaging (DM and group).
//!
//! Both `AnonDmService` and `AnonGroupService` persist messages through
//! identical logic: derive `MessageContent` from wire text/attachments (or
//! a `(content_type, content_json)` pair), then stamp the common `Message`
//! fields (id, sender, timestamps, attachments, reply metadata). These
//! helpers live here so the two services can't drift — notably so the
//! `{"text":"..."}` JSON envelope gets unwrapped on every path (the prior
//! anon-DM-only bug where raw JSON was stored as `MessageContent::Text`).

use crate::domain::ids::{ChannelId, MessageId};
use crate::domain::message::{ConversationType, Message, MessageContent, MixedContent};
use crate::domain::{AttachmentId, UserId};
use crate::inbox::handlers::base_anon::QuoteContext;
use crate::util::time::TimestampNanos;

/// Build a `MessageContent` from plain text plus attachment IDs.
///
/// Shape:
/// - text + attachments → `Mixed { text, attachments }`
/// - attachments only  → `Mixed { text: None, attachments }`
/// - text only / empty → `Text(text)`
pub(crate) fn message_content_from_text_and_attachments(
    text: &str,
    attachment_ids: &[AttachmentId],
) -> MessageContent {
    match (!text.is_empty(), !attachment_ids.is_empty()) {
        (true, true) => {
            MessageContent::Mixed(MixedContent::new(text.to_string(), attachment_ids.to_vec()))
        }
        (false, true) => {
            MessageContent::Mixed(MixedContent::attachments_only(attachment_ids.to_vec()))
        }
        _ => MessageContent::Text(text.to_string()),
    }
}

/// Convert a wire-format `(content_type, content_json)` pair to `MessageContent`.
///
/// Unwraps the `{"text": "..."}` JSON envelope used by inbound anon data
/// messages so the body is stored as real `MessageContent::Text` /
/// `MessageContent::Mixed`, not as an opaque JSON string. Prevents the
/// "raw JSON bubble on reload" bug.
pub(crate) fn message_content_from_type_and_json(
    content_type: i32,
    content_json: &str,
) -> MessageContent {
    match content_type {
        // Text
        1 => {
            let text = serde_json::from_str::<serde_json::Value>(content_json)
                .ok()
                .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(String::from))
                .unwrap_or_default();
            MessageContent::Text(text)
        }
        // Mixed (text + attachments) or reply with quote metadata
        3 => {
            let text = serde_json::from_str::<serde_json::Value>(content_json)
                .ok()
                .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(String::from));
            MessageContent::Mixed(MixedContent {
                text,
                attachment_ids: Vec::new(),
            })
        }
        _ => MessageContent::Text(String::new()),
    }
}

/// Build a unified `Message` row for an anonymous DM or group message.
///
/// This is the shared construction path so both protocols persist identical
/// shape (including reply metadata from `QuoteContext`). Callers insert via
/// their own `message_repo`.
pub(crate) fn build_anon_message(
    conversation_id: ChannelId,
    conversation_type: ConversationType,
    sender_id: UserId,
    content: MessageContent,
    attachment_ids: Vec<AttachmentId>,
    sent_at: u64,
    quote: Option<QuoteContext>,
) -> Message {
    let created_at = TimestampNanos::from_nanos(sent_at)
        .to_datetime()
        .unwrap_or_else(chrono::Utc::now);

    let (reply_to, reply_to_author_id, reply_to_text, reply_to_thumbnail, reply_to_thumbnail_mime) =
        match quote {
            Some(q) => (
                q.reply_to,
                q.reply_to_author_id,
                q.reply_to_text,
                q.reply_to_thumbnail,
                q.reply_to_thumbnail_mime,
            ),
            None => (None, None, None, None, None),
        };

    Message {
        id: MessageId::from_secret_sender_timestamp(&conversation_id, &sender_id, sent_at),
        conversation_id,
        conversation_type,
        sender_id: Some(sender_id),
        device_id: None,
        content,
        encrypted_content: None,
        thread_id: None,
        reply_to,
        reply_to_author_id,
        reply_to_text,
        reply_to_thumbnail,
        reply_to_thumbnail_mime,
        attachments: attachment_ids,
        is_pinned: false,
        is_edited: false,
        is_deleted: false,
        is_read: false,
        sent_at,
        created_at,
        updated_at: created_at,
        edited_at: None,
        reaction_counts: Vec::new(),
        current_user_reactions: Vec::new(),
        mentioned_integrations: Vec::new(),
        targets_ai_bot: false,
        delivery_status: String::new(),
        failure_reason: None,
    }
}
