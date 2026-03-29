//! Message converters.

use slint::{ModelRc, SharedString, VecModel};

use chatter_core::domain::anon_dm::AnonDmMessage;
use chatter_core::domain::anon_group::AnonGroupMessage;
use chatter_core::domain::attachment::Attachment;
use chatter_core::domain::message::{Message, MessageContent};

use crate::MessageItemData;

use super::attachment::{attachment_to_display_data, AttachmentDisplayData};
use super::utils::{
    avatar_bytes_to_image, extract_quote_from_content_json, format_count, get_initials,
    insert_word_breaks, parse_reaction_summary_to_entries,
};

/// Data needed to create message UI (can be sent across threads).
#[derive(Clone, Debug)]
pub struct MessageDisplayData {
    pub message_id: String,
    pub sender_id: String,
    pub sender_name: String,
    pub sender_initials: String,
    pub content: String,
    pub timestamp: String,
    pub timestamp_ns: String,  // Nanosecond timestamp as string for event matching
    pub is_edited: bool,
    pub is_deleted: bool,
    pub is_ai: bool,
    pub has_attachments: bool,
    pub attachment_count: i32,
    pub attachments: Vec<AttachmentDisplayData>,
    pub has_reactions: bool,
    pub reaction_summary: String,
    pub reply_count: i32,
    pub reply_count_text: String,
    pub is_thread_reply: bool,
    // Quote / reply-to context
    pub is_reply: bool,
    pub reply_to_sender_name: String,
    pub reply_to_preview: String,
    pub delivery_status: String,
    /// Raw avatar bytes for anonymous group members.
    pub sender_avatar_bytes: Option<Vec<u8>>,
    /// Pre-decoded avatar pixels (RGBA, width, height).
    pub sender_avatar_decoded: Option<(Vec<u8>, u32, u32)>,
    // Call message fields
    pub is_pinned: bool,
    pub is_call: bool,
    pub call_message_type: String,  // "started", "ended", "missed" (raw from domain)
    pub call_initiator_id: String,  // User ID of who initiated the call
    pub call_duration: String,      // "1 min 30 s" or empty
}

impl MessageDisplayData {
    /// Pre-decode all attachment thumbnails (call this before invoke_from_event_loop).
    pub fn decode_attachment_thumbnails(&mut self) {
        for att in &mut self.attachments {
            att.decode_thumbnail();
        }

        // Also decode sender avatar if available
        if let Some(bytes) = &self.sender_avatar_bytes {
            if !bytes.is_empty() {
                self.sender_avatar_decoded = image::load_from_memory(bytes)
                    .ok()
                    .map(|img| {
                        let rgba = img.to_rgba8();
                        let (w, h) = rgba.dimensions();
                        (rgba.into_raw(), w, h)
                    });
            }
        }
    }

    /// Convert to Slint MessageItemData with current user context.
    ///
    /// The `current_user_id` is used to determine call direction (outgoing vs incoming).
    pub fn to_slint_with_user(&self, current_user_id: &str) -> MessageItemData {
        let slint_attachments: Vec<crate::MessageAttachmentData> = self
            .attachments
            .iter()
            .map(|a| a.to_slint())
            .collect();

        // Determine call type based on who initiated
        let call_type = if self.is_call {
            match self.call_message_type.as_str() {
                "missed" => "missed".to_string(),
                _ => {
                    // If we initiated, it's outgoing; otherwise incoming
                    if self.call_initiator_id == current_user_id {
                        "outgoing".to_string()
                    } else {
                        "incoming".to_string()
                    }
                }
            }
        } else {
            String::new()
        };

        // For call messages, is_own is based on who initiated
        let is_own = if self.is_call {
            self.call_initiator_id == current_user_id
        } else {
            let matches = self.sender_id == current_user_id;
            if !matches && !current_user_id.is_empty() && !self.sender_id.is_empty() {
                tracing::debug!(
                    sender_id = %self.sender_id,
                    current_user_id = %current_user_id,
                    message_id = %self.message_id,
                    "[IS_OWN] sender_id != current_user_id"
                );
            }
            matches
        };

        let (sender_avatar_source, has_avatar) = if let Some((ref pixels, w, h)) = self.sender_avatar_decoded {
            let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                pixels, w, h
            );
            (slint::Image::from_rgba8(buffer), true)
        } else if let Some(bytes) = &self.sender_avatar_bytes {
            match avatar_bytes_to_image(bytes) {
                Some(img) => (img, true),
                None => (slint::Image::default(), false),
            }
        } else {
            (slint::Image::default(), false)
        };

    MessageItemData {
        message_id: self.message_id.clone().into(),
        sender_id: self.sender_id.clone().into(),
        sender_name: self.sender_name.clone().into(),
        sender_initials: self.sender_initials.clone().into(),
        sender_avatar_url: SharedString::default(),
        sender_avatar_source,
        has_avatar,
        content: insert_word_breaks(self.content.trim()).into(),
        timestamp: self.timestamp.clone().into(),
        timestamp_ns: self.timestamp_ns.clone().into(),
        is_own,
        is_edited: self.is_edited,
        is_deleted: self.is_deleted,
        is_system: false,
        is_ai: self.is_ai,
        has_attachments: !slint_attachments.is_empty() || self.has_attachments,
        attachment_count: (slint_attachments.len() as i32).max(self.attachment_count),
        attachments: ModelRc::new(VecModel::from(slint_attachments)),
        has_reactions: self.has_reactions,
        reaction_summary: self.reaction_summary.clone().into(),
        reactions: ModelRc::new(VecModel::from(parse_reaction_summary_to_entries(&self.reaction_summary))),
        reply_count: self.reply_count,
        reply_count_text: self.reply_count_text.clone().into(),
        is_thread_reply: self.is_thread_reply,
        is_reply: self.is_reply,
        reply_to_sender_name: self.reply_to_sender_name.clone().into(),
        reply_to_preview: self.reply_to_preview.clone().into(),
        show_sender: true,
        // For own messages with empty status, default to "sent"
        delivery_status: if is_own && self.delivery_status.is_empty() {
            "sent".into()
        } else {
            self.delivery_status.clone().into()
        },
        is_pinned: self.is_pinned,
        is_call: self.is_call,
        call_type: call_type.into(),
        call_duration: self.call_duration.clone().into(),
        date_separator_text: Default::default(),
        is_continuation: false,
        show_date_separator: false,
        has_link_preview: false,
        link_preview: Default::default(),
    }
}
}

/// Convert a domain Message to thread-safe MessageDisplayData.
///
/// If `resolved_attachments` is provided, attachment details will be included.
/// If `reply_count` is provided, it will be used; otherwise defaults to 0.
pub fn message_to_display_data(
    msg: &Message,
    sender_name: &str,
    resolved_attachments: &[Attachment],
) -> MessageDisplayData {
    message_to_display_data_with_replies(msg, sender_name, resolved_attachments, None)
}

/// Convert a domain Message to thread-safe MessageDisplayData with reply count.
pub fn message_to_display_data_with_replies(
    msg: &Message,
    sender_name: &str,
    resolved_attachments: &[Attachment],
    reply_count: Option<i32>,
) -> MessageDisplayData {
    let attachment_data: Vec<AttachmentDisplayData> = resolved_attachments
        .iter()
        .map(attachment_to_display_data)
        .collect();

    let reply_count = reply_count.unwrap_or(0);
    let reply_count_text = format_count(reply_count, "reply", "replies");

    // Extract call information if this is a call message
    let (is_call, call_message_type, call_initiator_id, call_duration) =
        if let MessageContent::Call(call_msg) = &msg.content {
            use chatter_core::domain::message::CallMessageType;
            let msg_type = match call_msg.call_type {
                CallMessageType::Started => "started",
                CallMessageType::Ended => "ended",
                CallMessageType::Missed => "missed",
            };
            let duration = call_msg.duration_display().unwrap_or_default();
            (
                true,
                msg_type.to_string(),
                call_msg.initiator_id.to_string(),
                duration,
            )
        } else {
            (false, String::new(), String::new(), String::new())
        };

    MessageDisplayData {
        message_id: msg.id.to_string(),
        sender_id: msg.sender_id.map(|id| id.to_string()).unwrap_or_default(),
        sender_name: sender_name.to_string(),
        sender_initials: get_initials(sender_name),
        content: msg.as_text().unwrap_or("").to_string(),
        timestamp: {
            // Convert UTC to local time for display (consistent with sidebar)
            use chrono::Local;
            msg.created_at.with_timezone(&Local).format("%H:%M").to_string()
        },
        timestamp_ns: msg.sent_at.to_string(),
        is_edited: msg.is_edited,
        is_deleted: msg.is_deleted,
        is_ai: msg.targets_ai_bot,
        has_attachments: msg.has_attachments(),
        attachment_count: msg.attachments.len() as i32,
        attachments: attachment_data,
        has_reactions: !msg.reaction_counts.is_empty(),
        reaction_summary: msg
            .reaction_counts
            .iter()
            .map(|r| format!("{} {}", r.emoji, r.count))
            .collect::<Vec<_>>()
            .join("  "),
        sender_avatar_bytes: None,  // TODO: Add avatar support for anonymous groups
        sender_avatar_decoded: None,  // TODO: Add avatar support for anonymous groups
        reply_count,
        reply_count_text,
        is_thread_reply: msg.is_thread_reply(),
        is_reply: false,
        reply_to_sender_name: String::new(),
        reply_to_preview: String::new(),
        delivery_status: msg.delivery_status.clone(),
        is_pinned: msg.is_pinned,
        is_call,
        call_message_type,
        call_initiator_id,
        call_duration,
    }
}

// ============================================================================
// Shared trait for anonymous message conversion
// ============================================================================

use chatter_core::domain::message::ReactionCount;

/// Trait for anonymous message types that can be converted to display data.
///
/// Implemented by both `AnonDmMessage` and `AnonGroupMessage` to eliminate
/// the duplicated converter functions.
pub trait AnonMessageFields {
    fn message_id_str(&self) -> &str;
    fn sender_id(&self) -> &chatter_core::domain::UserId;
    fn content_json(&self) -> &str;
    fn is_deleted(&self) -> bool;
    fn is_pinned(&self) -> bool;
    fn reaction_counts(&self) -> &[ReactionCount];
    fn delivery_status(&self) -> &str;
    fn sent_at(&self) -> u64;
    fn created_at(&self) -> &chrono::DateTime<chrono::Utc>;
    fn is_edited(&self) -> bool;
    fn as_text(&self) -> Option<String>;
}

impl AnonMessageFields for AnonGroupMessage {
    fn message_id_str(&self) -> &str { self.id.as_str() }
    fn sender_id(&self) -> &chatter_core::domain::UserId { &self.sender_id }
    fn content_json(&self) -> &str { &self.content_json }
    fn is_deleted(&self) -> bool { self.is_deleted }
    fn is_pinned(&self) -> bool { self.is_pinned }
    fn reaction_counts(&self) -> &[ReactionCount] { &self.reaction_counts }
    fn delivery_status(&self) -> &str { &self.delivery_status }
    fn sent_at(&self) -> u64 { self.sent_at }
    fn created_at(&self) -> &chrono::DateTime<chrono::Utc> { &self.created_at }
    fn is_edited(&self) -> bool { self.is_edited }
    fn as_text(&self) -> Option<String> { AnonGroupMessage::as_text(self) }
}

impl AnonMessageFields for AnonDmMessage {
    fn message_id_str(&self) -> &str { self.id.as_str() }
    fn sender_id(&self) -> &chatter_core::domain::UserId { &self.sender_id }
    fn content_json(&self) -> &str { &self.content_json }
    fn is_deleted(&self) -> bool { self.is_deleted }
    fn is_pinned(&self) -> bool { self.is_pinned }
    fn reaction_counts(&self) -> &[ReactionCount] { &self.reaction_counts }
    fn delivery_status(&self) -> &str { &self.delivery_status }
    fn sent_at(&self) -> u64 { self.sent_at }
    fn created_at(&self) -> &chrono::DateTime<chrono::Utc> { &self.created_at }
    fn is_edited(&self) -> bool { self.is_edited }
    fn as_text(&self) -> Option<String> { AnonDmMessage::as_text(self) }
}

/// Convert any anonymous message (DM or Group) to MessageDisplayData.
pub fn anon_message_to_display_data(
    msg: &impl AnonMessageFields,
    sender_name: &str,
    sender_avatar: Option<Vec<u8>>,
    resolved_attachments: &[Attachment],
) -> MessageDisplayData {
    use chrono::Local;

    let content = msg.as_text().unwrap_or_default();
    let quote_data = extract_quote_from_content_json(msg.content_json());
    let attachment_data: Vec<AttachmentDisplayData> = resolved_attachments
        .iter()
        .map(attachment_to_display_data)
        .collect();

    MessageDisplayData {
        message_id: msg.message_id_str().to_string(),
        sender_id: msg.sender_id().to_string(),
        sender_name: sender_name.to_string(),
        sender_initials: get_initials(sender_name),
        content: content.to_string(),
        timestamp: msg.created_at().with_timezone(&Local).format("%H:%M").to_string(),
        timestamp_ns: msg.sent_at().to_string(),
        is_edited: msg.is_edited(),
        is_deleted: msg.is_deleted(),
        is_ai: false,
        has_attachments: !attachment_data.is_empty(),
        attachment_count: attachment_data.len() as i32,
        attachments: attachment_data,
        has_reactions: !msg.reaction_counts().is_empty(),
        reaction_summary: msg
            .reaction_counts()
            .iter()
            .map(|r| format!("{} {}", r.emoji, r.count))
            .collect::<Vec<_>>()
            .join("  "),
        reply_count: 0,
        reply_count_text: String::new(),
        is_thread_reply: false,
        is_reply: quote_data.is_some(),
        reply_to_sender_name: quote_data.as_ref().map(|q| q.0.clone()).unwrap_or_default(),
        reply_to_preview: quote_data.as_ref().map(|q| q.1.clone()).unwrap_or_default(),
        delivery_status: msg.delivery_status().to_string(),
        sender_avatar_bytes: sender_avatar,
        sender_avatar_decoded: None,
        is_pinned: msg.is_pinned(),
        is_call: false,
        call_message_type: String::new(),
        call_initiator_id: String::new(),
        call_duration: String::new(),
    }
}

/// Convert a unified Message (used for anon group) to MessageDisplayData.
pub fn anon_group_message_to_display_data(
    msg: &Message,
    sender_name: &str,
    sender_avatar: Option<Vec<u8>>,
    resolved_attachments: &[Attachment],
) -> MessageDisplayData {
    anon_dm_message_to_display_data(msg, sender_name, sender_avatar, resolved_attachments)
}

/// Convert a unified Message (used for anon DM) to MessageDisplayData.
pub fn anon_dm_message_to_display_data(
    msg: &Message,
    sender_name: &str,
    sender_avatar: Option<Vec<u8>>,
    resolved_attachments: &[Attachment],
) -> MessageDisplayData {
    use chrono::Local;

    let content = msg.as_text().unwrap_or_default().to_string();
    let attachment_data: Vec<AttachmentDisplayData> = resolved_attachments
        .iter()
        .map(attachment_to_display_data)
        .collect();

    let sender_id_str = msg.sender_id
        .map(|id| id.to_string())
        .unwrap_or_default();

    MessageDisplayData {
        message_id: msg.id.to_string(),
        sender_id: sender_id_str,
        sender_name: sender_name.to_string(),
        sender_initials: get_initials(sender_name),
        content: content.to_string(),
        timestamp: msg.created_at.with_timezone(&Local).format("%H:%M").to_string(),
        timestamp_ns: msg.sent_at.to_string(),
        is_edited: msg.is_edited,
        is_deleted: msg.is_deleted,
        is_ai: false,
        has_attachments: !attachment_data.is_empty(),
        attachment_count: attachment_data.len() as i32,
        attachments: attachment_data,
        has_reactions: !msg.reaction_counts.is_empty(),
        reaction_summary: msg
            .reaction_counts
            .iter()
            .map(|r| format!("{} {}", r.emoji, r.count))
            .collect::<Vec<_>>()
            .join("  "),
        reply_count: 0,
        reply_count_text: String::new(),
        is_thread_reply: false,
        is_reply: false,
        reply_to_sender_name: String::new(),
        reply_to_preview: String::new(),
        delivery_status: msg.delivery_status.clone(),
        sender_avatar_bytes: sender_avatar,
        sender_avatar_decoded: None,
        is_pinned: msg.is_pinned,
        is_call: false,
        call_message_type: String::new(),
        call_initiator_id: String::new(),
        call_duration: String::new(),
    }
}
