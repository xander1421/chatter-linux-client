//! Shared messaging utility functions.
//!
//! Pure helper functions used across `MessageService`, `AnonDmService`,
//! and `AnonGroupService`. These are free functions (not methods on a struct)
//! that accept data as parameters — no service or repo access.
//!
//! Phase 3 of messaging consolidation.

use crate::domain::AttachmentId;
use crate::domain::attachment::{Attachment, FileCategory};
use crate::domain::message::Message;
use crate::proto::content_v1::{AttachmentPointer, Quote, QuotedAttachment, quote};

// =============================================================================
// Preview text
// =============================================================================

/// Maximum graphemes for a quote preview.
const QUOTE_PREVIEW_MAX_CHARS: usize = 100;
/// Truncate text for use as a quote preview.
///
/// If the text exceeds 100 graphemes, it's truncated to 97 graphemes
/// with "..." appended. This is the standard Signal-style quote preview.
pub fn truncate_preview(text: &str) -> String {
    crate::domain::message::truncate_preview_text(text, QUOTE_PREVIEW_MAX_CHARS)
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
        let att = &attachments[0];
        let cat = att.metadata.category();
        let name = att.metadata.name.trim();
        // Files with a real filename keep the filename in the preview.
        // True voice recordings (no filename) fall back to the generic label.
        if !name.is_empty() && cat != FileCategory::Image && cat != FileCategory::Video {
            let emoji = match cat {
                FileCategory::Audio => "🎵",
                FileCategory::Document => "📄",
                FileCategory::Code => "📝",
                FileCategory::Archive => "📦",
                _ => "📎",
            };
            return format!("{} {}", emoji, name);
        }
        if att.metadata.mime_type == "image/gif" {
            return "🎬 GIF".to_string();
        }
        let (emoji, label) = match cat {
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

/// Build a Signal-style Quote proto including one or more `QuotedAttachment`s.
///
/// Used for media replies so the receiver can render a thumbnail in the
/// reply bubble even if they haven't downloaded the original attachment.
pub fn build_quote_with_attachments(
    author_id: &str,
    timestamp: u64,
    preview_text: &str,
    attachments: Vec<QuotedAttachment>,
) -> Quote {
    Quote {
        id: timestamp,
        author_aci: author_id.to_string(),
        text: preview_text.to_string(),
        attachments,
        r#type: quote::Type::Normal as i32,
        author_aci_binary: vec![],
    }
}

/// Build `QuotedAttachment`s (Signal-style) from the quoted message's
/// attachments. Only image/video attachments with a sender thumbnail are
/// serialized — others are skipped to keep the quote payload small.
pub fn build_quoted_attachments(attachments: &[Attachment]) -> Vec<QuotedAttachment> {
    // Quote rides inside the encrypted DM, which is capped at MAX_CONTENT_SIZE
    // (64KB) after padding. The stored thumbnail is a 768px lossless WebP that
    // can run to hundreds of KB (gifs especially) — embedding it whole blew the
    // cap and the whole reply silently failed to send. Cap the embedded quote
    // thumbnail; over budget → skip it and let the text label carry the quote.
    const MAX_QUOTE_THUMB: usize = 12 * 1024;
    attachments
        .iter()
        .filter(|a| a.metadata.is_image() || a.metadata.is_video())
        .filter_map(|a| {
            let thumb = a.metadata.thumbnail.clone().unwrap_or_default();
            if thumb.is_empty() || thumb.len() > MAX_QUOTE_THUMB {
                return None;
            }
            Some(QuotedAttachment {
                content_type: a.metadata.mime_type.clone(),
                file_name: a.metadata.name.clone(),
                thumbnail: thumb,
            })
        })
        .take(1)
        .collect()
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

    if matches!(
        error,
        SendError::Message(crate::services::message::MessageError::ShieldNotReady)
    ) {
        return crate::services::message::core::SHIELD_WAITING_FOR_TOKENS.to_owned();
    }
    let detail = error.to_string();

    tracing::error!(
        context = context,
        error = %detail,
        "Failed to send message"
    );

    // Return user-facing message (don't expose internal error details)
    match error {
        SendError::BeforeSubmission(original) => handle_send_error(original, context),
        SendError::NotLoggedIn
        | SendError::Message(crate::services::message::MessageError::NotLoggedIn)
        | SendError::AnonDm(crate::services::anon_dm::AnonDmError::NotLoggedIn)
        | SendError::AnonGroup(crate::services::anon_group::AnonGroupError::NotLoggedIn) => {
            "Not logged in".to_string()
        }
        SendError::DeliveryUnconfirmed => SendError::DeliveryUnconfirmed.to_string(),
        SendError::SendInProgress => SendError::SendInProgress.to_string(),
        SendError::AttemptAlreadyConfirmed => SendError::AttemptAlreadyConfirmed.to_string(),
        SendError::UnsupportedAction => "Action not supported for this channel type".to_string(),
        SendError::Message(crate::services::message::MessageError::EncryptionFailed(_)) => {
            "Encryption failed — try again".to_string()
        }
        // NOT "try again". `ensure_session` refuses for as long as the change is
        // parked, so every retry fails identically; the only thing that clears
        // it is the user accepting the new safety number. The prompt to do that
        // is already open in this conversation, so point at it.
        SendError::Message(crate::services::message::MessageError::RecipientIdentityChanged {
            ..
        }) => "Their safety number changed — accept it above to send".to_string(),
        // Not "try again" either: the server could not prove this contact's
        // key, and retrying asks it the same question.
        SendError::Message(crate::services::message::MessageError::RecipientKeyUnverified {
            ..
        }) => "Can't verify this contact's security key — not sent".to_string(),
        SendError::AnonDm(crate::services::anon_dm::AnonDmError::ConversationBlocked(_))
        | SendError::AnonGroup(crate::services::anon_group::AnonGroupError::GroupBlocked(_)) => {
            "Conversation is blocked".to_string()
        }
        SendError::AnonDm(crate::services::anon_dm::AnonDmError::RateLimitExceeded(n))
        | SendError::AnonGroup(crate::services::anon_group::AnonGroupError::RateLimitExceeded(n)) =>
        {
            format!("Slow down — {} messages per minute limit", n)
        }
        e if e.is_refused() => "You can't send messages here".to_string(),
        _ => format!("Failed to send: {}", detail),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_preview_cuts_on_char_boundary() {
        // 'é' is 2 bytes; 96 ASCII bytes put one straddling byte 97, where a
        // byte slice `&text[..97]` panics.
        let text = format!("{}é{}", "a".repeat(96), "b".repeat(10));
        assert!(!text.is_char_boundary(97));
        let preview = truncate_preview(&text);
        assert_eq!(preview, format!("{}é...", "a".repeat(96)));
        assert_eq!(truncate_preview("héllo"), "héllo");
    }

    #[test]
    fn server_send_refusal_is_not_a_retryable_failure() {
        // A muted/banned member's send is refused by the server's
        // `send_message` check; say so instead of "Failed to send: ...".
        let err = super::super::outgoing::SendError::Message(
            crate::services::message::MessageError::Network(
                crate::error::NetworkError::PermissionDenied,
            ),
        );
        assert_eq!(
            handle_send_error(&err, "send"),
            "You can't send messages here"
        );
    }
}
