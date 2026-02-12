//! Domain to Slint type conversions.
//!
//! Pure functions for converting domain types to Slint UI types.

use slint::{Image, ModelRc, SharedString, VecModel};

use chatter_linux::domain::attachment::Attachment;
use chatter_linux::domain::channel::{Channel, ChannelType};
use chatter_linux::domain::message::Message;
use chatter_linux::state::PendingAttachment;

use crate::bin_ui::audio_state;

// Import Slint-generated types from crate root (where slint::include_modules! is called)
use crate::{ChannelListItemData, DMListItemData, MessageAttachmentData, MessageItemData, PendingAttachmentData};

/// Maximum characters in a word before inserting a break point.
/// Slint's word-wrap only breaks at spaces, so we insert zero-width spaces
/// to allow long strings without spaces to wrap.
const MAX_WORD_LENGTH: usize = 50;

/// Insert zero-width spaces (U+200B) into long sequences without whitespace.
/// This allows Slint's word-wrap to break long strings that have no natural break points.
fn insert_word_breaks(text: &str) -> String {
    const ZWSP: char = '\u{200B}';  // Zero-width space

    let mut result = String::with_capacity(text.len() + text.len() / MAX_WORD_LENGTH);
    let mut word_len = 0;

    for ch in text.chars() {
        if ch.is_whitespace() {
            word_len = 0;
            result.push(ch);
        } else {
            word_len += 1;
            result.push(ch);
            // Insert zero-width space after MAX_WORD_LENGTH consecutive non-whitespace chars
            if word_len >= MAX_WORD_LENGTH {
                result.push(ZWSP);
                word_len = 0;
            }
        }
    }

    result
}

/// Convert a domain Channel to Slint ChannelListItemData.
pub fn channel_to_slint(channel: &Channel) -> ChannelListItemData {
    ChannelListItemData {
        channel_id: channel.id.to_string().into(),
        name: channel.name.clone().into(),
        channel_type: channel_type_to_string(channel.channel_type).into(),
        unread_count: channel.unread_count as i32,
    }
}

/// Convert a domain Channel (DM) to Slint DMListItemData.
pub fn dm_to_slint(channel: &Channel, other_user_name: &str, other_user_id: &str) -> DMListItemData {
    DMListItemData {
        user_id: other_user_id.to_string().into(),
        display_name: other_user_name.to_string().into(),
        avatar_url: SharedString::default(),
        initials: get_initials(other_user_name).into(),
        presence_status: "offline".into(),
        last_message: SharedString::default(),
        timestamp: SharedString::default(),
        unread_count: channel.unread_count as i32,
    }
}

/// Data needed to create attachment UI (can be sent across threads).
#[derive(Clone, Debug)]
pub struct AttachmentDisplayData {
    pub id: String,
    pub filename: String,
    pub size_display: String,
    pub mime_type: String,
    pub category: String,
    pub thumbnail_url: String,
    pub download_url: String,
    pub local_path: Option<String>,
    pub is_image: bool,
    pub is_video: bool,
    pub width: i32,
    pub height: i32,
    pub is_playing: bool,
    pub icon: String,
    pub icon_color: slint::Color,
}

impl AttachmentDisplayData {
    /// Convert to Slint MessageAttachmentData (must be called on UI thread).
    pub fn to_slint(&self) -> MessageAttachmentData {
        // Try to load thumbnail from local path if available
        let (has_thumbnail, thumbnail, img_width, img_height) = if let Some(ref path) = self.local_path {
            if self.is_image {
                let path_obj = std::path::Path::new(path);
                if !path_obj.exists() {
                    tracing::debug!("Attachment thumbnail path does not exist: {}", path);
                    (false, Image::default(), self.width, self.height)
                } else {
                    match Image::load_from_path(path_obj) {
                        Ok(img) => {
                            // Extract actual dimensions from loaded image
                            let size = img.size();
                            let w = size.width as i32;
                            let h = size.height as i32;
                            tracing::debug!(
                                "Loaded thumbnail for {}: {}x{}",
                                self.filename,
                                w,
                                h
                            );
                            (true, img, w, h)
                        }
                        Err(e) => {
                            tracing::warn!("Failed to load thumbnail for {}: {:?}", self.filename, e);
                            (false, Image::default(), self.width, self.height)
                        }
                    }
                }
            } else {
                (false, Image::default(), self.width, self.height)
            }
        } else {
            tracing::debug!("No local_path for attachment: {}", self.filename);
            (false, Image::default(), self.width, self.height)
        };

        MessageAttachmentData {
            id: self.id.clone().into(),
            filename: self.filename.clone().into(),
            size_display: self.size_display.clone().into(),
            mime_type: self.mime_type.clone().into(),
            category: self.category.clone().into(),
            thumbnail_url: self.thumbnail_url.clone().into(),
            download_url: self.download_url.clone().into(),
            is_image: self.is_image,
            is_video: self.is_video,
            width: img_width,
            height: img_height,
            has_thumbnail,
            thumbnail,
            is_playing: self.is_playing,
            icon: self.icon.clone().into(),
            icon_color: self.icon_color,
        }
    }
}

/// Convert a domain Attachment to thread-safe AttachmentDisplayData.
pub fn attachment_to_display_data(attachment: &Attachment) -> AttachmentDisplayData {
    let meta = &attachment.metadata;
    let category = mime_to_category(&meta.mime_type);
    let id_str = attachment.id.to_string();

    // Check if this attachment is currently playing audio
    let is_playing = meta.is_audio() && audio_state::is_attachment_playing(&id_str);

    // Get icon and color for category
    let (icon, icon_color) = category_to_icon_and_color(category);

    AttachmentDisplayData {
        id: id_str,
        filename: meta.name.clone(),
        size_display: format_file_size(meta.size),
        mime_type: meta.mime_type.clone(),
        category: category.to_string(),
        thumbnail_url: String::new(), // Populated by caller if available
        download_url: attachment.remote_url.clone().unwrap_or_default(),
        local_path: attachment.local_path.clone(),
        is_image: meta.is_image(),
        is_video: meta.is_video(),
        width: meta.width.unwrap_or(0) as i32,
        height: meta.height.unwrap_or(0) as i32,
        is_playing,
        icon: icon.to_string(),
        icon_color,
    }
}

/// Data needed to create message UI (can be sent across threads).
#[derive(Clone, Debug)]
pub struct MessageDisplayData {
    pub message_id: String,
    pub sender_id: String,
    pub sender_name: String,
    pub sender_initials: String,
    pub content: String,
    pub timestamp: String,
    pub timestamp_ms: i64,
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
    pub delivery_status: String,
}

impl MessageDisplayData {
    /// Convert to Slint MessageItemData (must be called on UI thread).
    pub fn to_slint(&self) -> MessageItemData {
        let slint_attachments: Vec<MessageAttachmentData> = self
            .attachments
            .iter()
            .map(|a| a.to_slint())
            .collect();

        MessageItemData {
            message_id: self.message_id.clone().into(),
            sender_id: self.sender_id.clone().into(),
            sender_name: self.sender_name.clone().into(),
            sender_initials: self.sender_initials.clone().into(),
            sender_avatar_url: SharedString::default(),
            content: self.content.clone().into(),
            timestamp: self.timestamp.clone().into(),
            timestamp_ms: self.timestamp_ms as i32,
            is_own: false, // Set by caller based on current user
            is_edited: self.is_edited,
            is_deleted: self.is_deleted,
            is_system: false,
            is_ai: self.is_ai,
            has_attachments: self.has_attachments,
            attachment_count: self.attachment_count,
            attachments: ModelRc::new(VecModel::from(slint_attachments)),
            has_reactions: self.has_reactions,
            reaction_summary: self.reaction_summary.clone().into(),
            reply_count: self.reply_count,
            reply_count_text: self.reply_count_text.clone().into(),
            is_thread_reply: self.is_thread_reply,
            show_sender: true,
            delivery_status: self.delivery_status.clone().into(),
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

    MessageDisplayData {
        message_id: msg.id.to_string(),
        sender_id: msg.sender_id.map(|id| id.to_string()).unwrap_or_default(),
        sender_name: sender_name.to_string(),
        sender_initials: get_initials(sender_name),
        content: insert_word_breaks(msg.as_text().unwrap_or("")),
        timestamp: msg.created_at.format("%H:%M").to_string(),
        timestamp_ms: msg.created_at.timestamp_millis(),
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
            .join(" "),
        reply_count,
        reply_count_text,
        is_thread_reply: msg.is_thread_reply(),
        delivery_status: String::new(), // Set by caller based on receipt status
    }
}

/// Convert ChannelType enum to string for Slint.
pub fn channel_type_to_string(ct: ChannelType) -> &'static str {
    match ct {
        ChannelType::Public => "public",
        ChannelType::Private => "private",
        ChannelType::DirectMessage => "dm",
        ChannelType::SavedMessages => "saved",
    }
}

/// Extract initials from a display name (first letter of each word, max 2).
pub fn get_initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|word| word.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase()
}

/// Convert a PendingAttachment to Slint PendingAttachmentData.
/// Note: This should be called from the UI thread for image loading to work.
pub fn pending_attachment_to_slint(attachment: &PendingAttachment) -> PendingAttachmentData {
    let is_image = attachment.mime_type.starts_with("image/");

    // Try to load thumbnail for images
    let (has_thumbnail, thumbnail) = if is_image && attachment.path.exists() {
        match Image::load_from_path(&attachment.path) {
            Ok(img) => (true, img),
            Err(e) => {
                tracing::warn!("Failed to load thumbnail for {}: {:?}", attachment.filename, e);
                (false, Image::default())
            }
        }
    } else {
        (false, Image::default())
    };

    PendingAttachmentData {
        id: attachment.id.to_string().into(),
        filename: attachment.filename.clone().into(),
        size_display: format_file_size(attachment.size).into(),
        category: mime_to_category(&attachment.mime_type).into(),
        is_ready: true, // Already uploaded
        has_thumbnail,
        thumbnail,
    }
}

/// Format file size in human-readable form.
pub fn format_file_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Map MIME type to category for UI display.
fn mime_to_category(mime_type: &str) -> &'static str {
    if mime_type.starts_with("image/") {
        "image"
    } else if mime_type.starts_with("video/") {
        "video"
    } else if mime_type.starts_with("audio/") {
        "audio"
    } else if mime_type.starts_with("text/") || mime_type == "application/json" {
        "code"
    } else if mime_type == "application/pdf"
        || mime_type.starts_with("application/msword")
        || mime_type.contains("document")
    {
        "document"
    } else if mime_type == "application/zip"
        || mime_type == "application/x-rar-compressed"
        || mime_type == "application/x-tar"
        || mime_type == "application/gzip"
    {
        "archive"
    } else {
        "other"
    }
}

/// Map category to icon emoji and background color.
fn category_to_icon_and_color(category: &str) -> (&'static str, slint::Color) {
    match category {
        "document" => ("\u{1F4C4}", slint::Color::from_argb_u8(255, 96, 165, 250)),  // #60a5fa
        "code" => ("\u{1F4DD}", slint::Color::from_argb_u8(255, 251, 191, 36)),       // #fbbf24
        "archive" => ("\u{1F4E6}", slint::Color::from_argb_u8(255, 251, 146, 60)),    // #fb923c
        "image" => ("\u{1F5BC}", slint::Color::from_argb_u8(255, 74, 222, 128)),      // #4ade80
        "video" => ("\u{1F3AC}", slint::Color::from_argb_u8(255, 248, 113, 113)),     // #f87171
        "audio" => ("\u{1F3B5}", slint::Color::from_argb_u8(255, 167, 139, 250)),     // #a78bfa
        _ => ("\u{1F4CE}", slint::Color::from_argb_u8(255, 148, 163, 184)),           // #94a3b8
    }
}

/// Format a count with singular/plural form.
fn format_count(count: i32, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {}", singular)
    } else {
        format!("{} {}", count, plural)
    }
}
