//! Shared messaging utility functions.
//!
//! Pure helper functions used across `MessageService`, `AnonDmService`,
//! and `AnonGroupService`. These are free functions (not methods on a struct)
//! that accept data as parameters — no service or repo access.
//!
//! Phase 3 of messaging consolidation.

use crate::domain::attachment::{Attachment, FileCategory};
use crate::domain::message::Message;
use crate::domain::AttachmentId;
use crate::proto::content_v1::{AttachmentPointer, Quote, quote};

// =============================================================================
// Preview text
// =============================================================================

/// Maximum characters for a quote preview.
const QUOTE_PREVIEW_MAX_CHARS: usize = 100;
/// Characters to show before the ellipsis (3 chars for "...").
const QUOTE_PREVIEW_TRUNCATE_AT: usize = 97;

/// Truncate text for use as a quote preview.
///
/// If the text exceeds 100 characters, it's truncated to 97 characters
/// with "..." appended. This is the standard Signal-style quote preview.
pub fn truncate_preview(text: &str) -> String {
    if text.chars().count() > QUOTE_PREVIEW_MAX_CHARS {
        format!("{}...", text.chars().take(QUOTE_PREVIEW_TRUNCATE_AT).collect::<String>())
    } else {
        text.to_string()
    }
}

/// Build the reply-preview string for a message, falling back to an
/// attachment-type placeholder ("📷 Photo", "🎬 Video", etc.) when the
/// message has no text.
///
/// Empty return only when the message has neither text nor attachments.
/// Used by both the sender (to populate `Quote.text` before send) and the
/// UI quote-preview extraction path, so the receiver's reply bubble never
/// shows an empty preview for media-only replies.
pub fn message_reply_preview(msg: &Message, attachments: &[Attachment]) -> String {
    if let Some(text) = msg.as_text() {
        if !text.is_empty() {
            return truncate_preview(text);
        }
    }

    if attachments.len() == 1 {
        let (emoji, label) = match attachments[0].metadata.category() {
            FileCategory::Image => ("📷", "Photo"),
            FileCategory::Video => ("🎬", "Video"),
            FileCategory::Audio => ("🎵", "Voice message"),
            FileCategory::Document => ("📄", "Document"),
            FileCategory::Code => ("📝", "Code"),
            FileCategory::Archive => ("📦", "Archive"),
            FileCategory::Other => ("📎", "File"),
        };
        return format!("{} {}", emoji, label);
    }
    if attachments.len() > 1 {
        return format!("📎 {} attachments", attachments.len());
    }

    // Attachment IDs on the row but repo didn't resolve (e.g. peer hasn't
    // downloaded). Still give a non-empty hint.
    if !msg.attachments.is_empty() {
        return if msg.attachments.len() == 1 {
            "📎 Attachment".to_string()
        } else {
            format!("📎 {} attachments", msg.attachments.len())
        };
    }

    String::new()
}

// =============================================================================
// Quote construction
// =============================================================================

/// Build a Signal-style Quote proto from reply context.
///
/// The Quote uses `author_aci` (author's UUID string) and `id` (the original
/// message's timestamp) for identification — this is how Signal identifies
/// quoted messages without server-assigned message IDs.
pub fn build_quote(author_id: &str, timestamp: u64, preview_text: &str) -> Quote {
    Quote {
        id: timestamp,
        author_aci: author_id.to_string(),
        text: preview_text.to_string(),
        attachments: vec![],
        r#type: quote::Type::Normal as i32,
        author_aci_binary: vec![],
    }
}

// =============================================================================
// Attachment utilities
// =============================================================================

/// Extract `AttachmentId`s from a slice of `AttachmentPointer` protos.
///
/// Filters out pointers with empty `cdn_key` (not yet uploaded) and
/// derives deterministic IDs from the CDN keys.
pub fn extract_attachment_ids(pointers: &[AttachmentPointer]) -> Vec<AttachmentId> {
    pointers
        .iter()
        .filter_map(|ptr| {
            if ptr.cdn_key.is_empty() {
                None
            } else {
                Some(AttachmentId::from_cdn_key(&ptr.cdn_key))
            }
        })
        .collect()
}

// =============================================================================
// Content type classification
// =============================================================================

/// Content type constants for message storage.
///
/// These match the `content_type` column in the unified `messages` table.
pub const CONTENT_TYPE_EMPTY: i32 = 0;
pub const CONTENT_TYPE_TEXT: i32 = 1;
pub const CONTENT_TYPE_ATTACHMENT: i32 = 2;
pub const CONTENT_TYPE_MIXED: i32 = 3;

/// Determine the content type for a message based on its contents.
///
/// Returns one of the `CONTENT_TYPE_*` constants.
pub fn determine_content_type(has_text: bool, has_attachments: bool) -> i32 {
    match (has_text, has_attachments) {
        (true, true) => CONTENT_TYPE_MIXED,
        (true, false) => CONTENT_TYPE_TEXT,
        (false, true) => CONTENT_TYPE_ATTACHMENT,
        (false, false) => CONTENT_TYPE_EMPTY,
    }
}

/// Build the `content_json` string for message storage.
///
/// The JSON format is used by both anon DM and anon group message tables
/// to store structured message content alongside the raw proto.
pub fn build_content_json(text: &str, attachment_ids: &[AttachmentId]) -> String {
    let has_text = !text.is_empty();
    let has_attachments = !attachment_ids.is_empty();

    match (has_text, has_attachments) {
        (true, true) => serde_json::json!({
            "text": text,
            "attachments": attachment_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>()
        })
        .to_string(),
        (true, false) => serde_json::json!({ "text": text }).to_string(),
        (false, true) => serde_json::json!({
            "attachments": attachment_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>()
        })
        .to_string(),
        (false, false) => "{}".to_string(),
    }
}

/// Build `content_json` for a reply message with quote metadata.
///
/// Includes the quote's author, display name, timestamp, and preview text
/// for local display without needing to re-fetch the original message.
pub fn build_reply_content_json(
    text: &str,
    quote_author_id: &str,
    quote_author_name: &str,
    quote_timestamp: u64,
    quote_preview: &str,
) -> String {
    serde_json::json!({
        "text": text,
        "quote": {
            "author_id": quote_author_id,
            "author_name": quote_author_name,
            "timestamp": quote_timestamp,
            "preview": quote_preview,
        }
    })
    .to_string()
}

/// Serialize attachment IDs to a binary blob for DB storage.
///
/// Uses bincode serialization. Returns an empty-vec bincode encoding
/// (`[8, 0, 0, 0, 0, 0, 0, 0]`) on serialization failure.
pub fn serialize_attachment_ids(ids: &[AttachmentId]) -> Vec<u8> {
    crate::storage::serialization::db_serialize(&ids.to_vec())
        .unwrap_or_else(|_| vec![8, 0, 0, 0, 0, 0, 0, 0])
}

/// The empty-vec bincode encoding used when there are no attachments.
pub const EMPTY_ATTACHMENTS_BLOB: [u8; 8] = [8, 0, 0, 0, 0, 0, 0, 0];

// =============================================================================
// Error handling
// =============================================================================

/// Log a send error with consistent context.
///
/// Provides standardized error logging across all channel types.
/// Returns a user-facing error message suitable for UI display.
pub fn handle_send_error(error: &super::outgoing::SendError, context: &str) -> String {
    use super::outgoing::SendError;

    let detail = error.to_string();

    tracing::error!(
        context = context,
        error = %detail,
        "Failed to send message"
    );

    // Return user-facing message (don't expose internal error details)
    match error {
        SendError::NotLoggedIn |
        SendError::Message(crate::services::message::MessageError::NotLoggedIn) |
        SendError::AnonDm(crate::services::anon_dm::AnonDmError::NotLoggedIn) |
        SendError::AnonGroup(crate::services::anon_group::AnonGroupError::NotLoggedIn) => {
            "Not logged in".to_string()
        }
        SendError::UnsupportedAction => {
            "Action not supported for this channel type".to_string()
        }
        SendError::Message(crate::services::message::MessageError::EncryptionFailed(_)) => {
            "Encryption failed — try again".to_string()
        }
        SendError::AnonDm(crate::services::anon_dm::AnonDmError::ConversationBlocked(_)) |
        SendError::AnonGroup(crate::services::anon_group::AnonGroupError::GroupBlocked(_)) => {
            "Conversation is blocked".to_string()
        }
        SendError::AnonDm(crate::services::anon_dm::AnonDmError::RateLimitExceeded(n)) |
        SendError::AnonGroup(crate::services::anon_group::AnonGroupError::RateLimitExceeded(n)) => {
            format!("Slow down — {} messages per minute limit", n)
        }
        _ => format!("Failed to send: {}", detail),
    }
}
