//! Domain to Slint type conversions.
//!
//! Pure functions for converting domain types to Slint UI types.
//!
//! Note: Some Slint types contain `slint::Image` which is not `Send`,
//! so we provide Send-safe intermediate types for use across thread boundaries.

use slint::{Image, ModelRc, SharedString, VecModel};

use chatter_linux::domain::anon_dm::AnonDmMessage;
use chatter_linux::domain::anon_group::AnonGroupMessage;
use chatter_linux::domain::attachment::{Attachment, AttachmentStatus};
use chatter_linux::domain::channel::{Channel, ChannelType};
use chatter_linux::domain::ids::AttachmentId;
use chatter_linux::domain::message::{Message, MessageContent};
use chatter_linux::proto::content_v1::AttachmentPointer;
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
        last_message: SharedString::default(),
        timestamp: SharedString::default(),
    }
}

/// Send-safe intermediate struct for DMListItemData.
///
/// `slint::Image` is not Send, so we use this to pass data across thread boundaries
/// and convert to `DMListItemData` inside the Slint event loop.
#[derive(Clone, Debug)]
pub struct DMListItemSendData {
    pub channel_id: String,
    pub user_id: String,
    pub display_name: String,
    pub avatar_url: String,
    /// Raw avatar bytes (will be decoded on UI thread if not pre-decoded)
    pub avatar_bytes: Option<Vec<u8>>,
    /// Pre-decoded RGBA pixels (width, height, data) - use this to avoid blocking UI thread
    pub avatar_decoded: Option<(u32, u32, Vec<u8>)>,
    pub initials: String,
    pub presence_status: String,
    pub last_message: String,
    pub timestamp: String,
    pub unread_count: i32,
    pub is_typing: bool,
}

impl DMListItemSendData {
    /// Pre-decode avatar bytes to RGBA. Call this BEFORE invoke_from_event_loop
    /// to avoid blocking the UI thread with image decoding.
    pub fn pre_decode_avatar(&mut self) {
        if self.avatar_decoded.is_some() {
            return; // Already decoded
        }
        if let Some(bytes) = &self.avatar_bytes {
            if let Ok(img) = image::load_from_memory(bytes) {
                let rgba = img.to_rgba8();
                let (w, h) = rgba.dimensions();
                self.avatar_decoded = Some((w, h, rgba.into_raw()));
            }
        }
    }

    /// Convert to Slint DMListItemData. Must be called inside Slint event loop.
    /// For best performance, call pre_decode_avatar() before entering the event loop.
    pub fn to_slint(self) -> DMListItemData {
        let (avatar_source, has_avatar) = if let Some((w, h, pixels)) = self.avatar_decoded {
            // Fast path: use pre-decoded pixels
            let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                &pixels, w, h
            );
            (slint::Image::from_rgba8(buffer), true)
        } else if let Some(bytes) = &self.avatar_bytes {
            // Slow path: decode on UI thread (avoid if possible)
            match avatar_bytes_to_image(bytes) {
                Some(img) => (img, true),
                None => (slint::Image::default(), false),
            }
        } else {
            (slint::Image::default(), false)
        };

        DMListItemData {
            channel_id: self.channel_id.into(),
            user_id: self.user_id.into(),
            display_name: self.display_name.into(),
            avatar_url: self.avatar_url.into(),
            avatar_source,
            has_avatar,
            initials: self.initials.into(),
            presence_status: self.presence_status.into(),
            last_message: self.last_message.into(),
            timestamp: self.timestamp.into(),
            unread_count: self.unread_count,
            is_typing: self.is_typing,
        }
    }
}

/// Convert a domain Channel (DM) to Send-safe DMListItemSendData.
pub fn dm_to_send_data(channel: &Channel, other_user_name: &str, other_user_id: &str) -> DMListItemSendData {
    DMListItemSendData {
        channel_id: channel.id.to_string(),
        user_id: other_user_id.to_string(),
        display_name: other_user_name.to_string(),
        avatar_url: String::new(),
        avatar_bytes: None, // Regular DMs don't use local avatar bytes
        avatar_decoded: None,
        initials: get_initials(other_user_name),
        presence_status: "offline".to_string(),
        last_message: String::new(),
        timestamp: String::new(),
        unread_count: channel.unread_count as i32,
        is_typing: false,
    }
}

/// Convert a domain Channel (DM) to Slint DMListItemData.
/// Note: This creates a struct containing slint::Image which is not Send.
/// Use `dm_to_send_data` when you need to pass data across thread boundaries.
pub fn dm_to_slint(channel: &Channel, other_user_name: &str, other_user_id: &str) -> DMListItemData {
    DMListItemData {
        channel_id: channel.id.to_string().into(),
        user_id: other_user_id.to_string().into(),
        display_name: other_user_name.to_string().into(),
        avatar_url: SharedString::default(),
        avatar_source: slint::Image::default(),
        has_avatar: false,
        initials: get_initials(other_user_name).into(),
        presence_status: "offline".into(),
        last_message: SharedString::default(),
        timestamp: SharedString::default(),
        unread_count: channel.unread_count as i32,
        is_typing: false,
    }
}

/// Send-safe intermediate struct for FriendRequestData.
#[derive(Clone, Debug)]
pub struct FriendRequestSendData {
    pub user_id: String,
    pub display_name: String,
    pub username: String,
    pub avatar_url: String,
    pub avatar_bytes: Option<Vec<u8>>,
    /// Pre-decoded RGBA pixels (width, height, data) - use this to avoid blocking UI thread
    pub avatar_decoded: Option<(u32, u32, Vec<u8>)>,
    pub sent_at: String,
}

impl FriendRequestSendData {
    /// Pre-decode avatar bytes to RGBA. Call this BEFORE invoke_from_event_loop
    /// to avoid blocking the UI thread with image decoding.
    pub fn pre_decode_avatar(&mut self) {
        if self.avatar_decoded.is_some() {
            return; // Already decoded
        }
        if let Some(bytes) = &self.avatar_bytes {
            if let Ok(img) = image::load_from_memory(bytes) {
                let rgba = img.to_rgba8();
                let (w, h) = rgba.dimensions();
                self.avatar_decoded = Some((w, h, rgba.into_raw()));
            }
        }
    }

    /// Convert to Slint FriendRequestData. Must be called inside Slint event loop.
    /// For best performance, call pre_decode_avatar() before entering the event loop.
    pub fn to_slint(self) -> crate::FriendRequestData {
        let (avatar_source, has_avatar) = if let Some((w, h, pixels)) = self.avatar_decoded {
            // Fast path: use pre-decoded pixels
            let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                &pixels, w, h
            );
            (slint::Image::from_rgba8(buffer), true)
        } else if let Some(bytes) = &self.avatar_bytes {
            // Slow path: decode on UI thread (avoid if possible)
            match avatar_bytes_to_image(bytes) {
                Some(img) => (img, true),
                None => (slint::Image::default(), false),
            }
        } else {
            (slint::Image::default(), false)
        };

        crate::FriendRequestData {
            user_id: self.user_id.into(),
            display_name: self.display_name.into(),
            username: self.username.into(),
            avatar_url: self.avatar_url.into(),
            avatar_source,
            has_avatar,
            sent_at: self.sent_at.into(),
        }
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
    /// Audio waveform samples (32 values, 0-100) for visualization.
    pub waveform: Vec<i32>,
    /// Formatted duration string (e.g., "1:23").
    pub duration_display: String,
    /// Transfer progress fields
    pub is_uploading: bool,
    pub is_downloading: bool,
    pub progress: f32,
    /// Thumbnail JPEG bytes from sender (for instant preview without download).
    pub thumbnail_bytes: Vec<u8>,
    /// Pre-decoded thumbnail pixels (RGBA, width, height) - decode before UI thread.
    pub thumbnail_pixels: Option<(Vec<u8>, u32, u32)>,
}

impl AttachmentDisplayData {
    /// Pre-decode thumbnail bytes to RGBA pixels (call this before invoke_from_event_loop).
    pub fn decode_thumbnail(&mut self) {
        if self.thumbnail_pixels.is_some() {
            return; // Already decoded
        }
        if !self.thumbnail_bytes.is_empty() {
            self.thumbnail_pixels = image::load_from_memory(&self.thumbnail_bytes)
                .ok()
                .map(|img| {
                    let rgba = img.to_rgba8();
                    let (w, h) = rgba.dimensions();
                    (rgba.into_raw(), w, h)
                });
        }
    }

    /// Convert to Slint MessageAttachmentData (must be called on UI thread).
    pub fn to_slint(&self) -> MessageAttachmentData {
        // Priority 0: Use pre-decoded pixels (fastest - no decoding on UI thread)
        // Priority 1: Try to load thumbnail from sender-provided bytes (instant preview)
        // Priority 2: Load from local path after download
        let (has_thumbnail, thumbnail, img_width, img_height) = if let Some((ref pixels, w, h)) = self.thumbnail_pixels {
            // Use pre-decoded pixels (fast - just wrap in Slint buffer)
            let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                pixels,
                w,
                h,
            );
            let slint_img = Image::from_rgba8(buffer);
            (true, slint_img, w as i32, h as i32)
        } else if !self.thumbnail_bytes.is_empty() {
            // Fallback: decode on UI thread (should be avoided - call decode_thumbnail() first)
            tracing::warn!(
                "Decoding thumbnail on UI thread for {} - consider calling decode_thumbnail() earlier",
                self.filename
            );
            match image::load_from_memory(&self.thumbnail_bytes) {
                Ok(img) => {
                    let rgba = img.to_rgba8();
                    let (w, h) = rgba.dimensions();
                    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                        rgba.as_raw(),
                        w,
                        h,
                    );
                    let slint_img = Image::from_rgba8(buffer);
                    (true, slint_img, w as i32, h as i32)
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to decode thumbnail bytes for {}: {:?}",
                        self.filename,
                        e
                    );
                    (false, Image::default(), self.width, self.height)
                }
            }
        } else if let Some(ref path) = self.local_path {
            // Load from local file after download
            if self.is_image || self.is_video {
                let path_obj = std::path::Path::new(path);
                if !path_obj.exists() {
                    tracing::info!("Attachment thumbnail path does not exist: {}", path);
                    (false, Image::default(), self.width, self.height)
                } else {
                    // For videos, we need to extract a thumbnail if it hasn't been done yet
                    // But Image::load_from_path only works for images.
                    // If it's a video and we are here, it means we don't have thumbnail_bytes.
                    if self.is_video {
                        tracing::info!("Skipping local video thumbnail loading for {} (not implemented)", self.filename);
                        (false, Image::default(), self.width, self.height)
                    } else {
                        match Image::load_from_path(path_obj) {
                            Ok(img) => {
                                // Extract actual dimensions from loaded image
                                let size = img.size();
                                let w = size.width as i32;
                                let h = size.height as i32;
                                tracing::info!(
                                    "Loaded thumbnail from local path for {}: {}x{}",
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
                }
            } else {
                (false, Image::default(), self.width, self.height)
            }
        } else {
            tracing::info!("No thumbnail source available for {}", self.filename);
            (false, Image::default(), self.width, self.height)
        };

        // Convert waveform to Slint model
        let waveform_model: ModelRc<i32> = ModelRc::new(VecModel::from(self.waveform.clone()));

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
            waveform: waveform_model,
            duration_display: self.duration_display.clone().into(),
            is_uploading: self.is_uploading,
            is_downloading: self.is_downloading,
            progress: self.progress,
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

    // Get waveform samples (convert u8 to i32 for Slint)
    let waveform: Vec<i32> = meta
        .waveform_samples
        .as_ref()
        .map(|samples| {
            let converted: Vec<i32> = samples.iter().map(|&s| s as i32).collect();
            if meta.is_audio() {
                let max = converted.iter().max().copied().unwrap_or(0);
                tracing::debug!(
                    "attachment_to_display_data: waveform has {} samples, max={}, first_5={:?}",
                    converted.len(), max, &converted[..5.min(converted.len())]
                );
            }
            converted
        })
        .unwrap_or_else(|| {
            if meta.is_audio() {
                tracing::warn!("attachment_to_display_data: no waveform_samples for audio, using flat default");
            }
            vec![10; 32]
        }); // Default flat waveform

    // Format duration for audio
    let duration_display = meta
        .duration_seconds
        .map(|secs| {
            let total_secs = secs as u64;
            let minutes = total_secs / 60;
            let seconds = total_secs % 60;
            format!("{}:{:02}", minutes, seconds)
        })
        .unwrap_or_default();

    // Determine transfer status from attachment status
    let is_uploading = attachment.status == AttachmentStatus::Uploading;
    let is_downloading = attachment.status == AttachmentStatus::Downloading;

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
        waveform,
        duration_display,
        is_uploading,
        is_downloading,
        progress: 0.0, // Will be updated by progress callback
        // Use stored thumbnail from sender (for instant preview)
        thumbnail_bytes: {
            let bytes = meta.thumbnail.clone().unwrap_or_default();
            if meta.is_image() || meta.is_video() {
                tracing::debug!(
                    "attachment_to_display_data: {} thumbnail_bytes={}, waveform_samples={:?}",
                    meta.name,
                    bytes.len(),
                    meta.waveform_samples.as_ref().map(|w| w.len())
                );
            }
            bytes
        },
        thumbnail_pixels: None, // Pre-decode with decode_thumbnail() before UI thread
    }
}

/// Convert an AttachmentPointer (from proto) to thread-safe AttachmentDisplayData.
///
/// Used for displaying attachments immediately upon receive, before download completes.
/// Shows filename, size, type info while download happens in background.
pub fn attachment_pointer_to_display_data(pointer: &AttachmentPointer) -> AttachmentDisplayData {
    let category = mime_to_category(&pointer.content_type);
    let is_image = pointer.content_type.starts_with("image/");
    let is_video = pointer.content_type.starts_with("video/");
    let _is_audio = pointer.content_type.starts_with("audio/");

    // Get icon and color for category
    let (icon, icon_color) = category_to_icon_and_color(category);

    // Use waveform from pointer if available (sender-side generated)
    let waveform: Vec<i32> = if !pointer.waveform.is_empty() {
        let converted: Vec<i32> = pointer.waveform.iter().map(|&s| s as i32).collect();
        let max = converted.iter().max().copied().unwrap_or(0);
        tracing::debug!(
            "attachment_pointer_to_display_data: waveform has {} samples, max={}, first_5={:?}",
            converted.len(), max, &converted[..5.min(converted.len())]
        );
        converted
    } else {
        tracing::debug!("attachment_pointer_to_display_data: no waveform in pointer, using flat default");
        vec![10; 32] // Default flat waveform
    };

    // Format duration from pointer (milliseconds)
    let duration_display = if pointer.duration_ms > 0 {
        let total_secs = pointer.duration_ms / 1000;
        let minutes = total_secs / 60;
        let seconds = total_secs % 60;
        format!("{}:{:02}", minutes, seconds)
    } else {
        String::new()
    };

    // Check if we have sender-provided thumbnail (instant preview)
    let has_thumbnail = !pointer.thumbnail.is_empty();

    // No download needed if we have thumbnail OR it's audio with waveform
    // Video still needs download since we can't play without full file
    let needs_download = is_video || (is_image && !has_thumbnail);

    // Use deterministic UUID from cdn_key for consistent ID matching
    // This ensures the ID matches what's stored in the attachment repository
    let attachment_id = AttachmentId::from_cdn_key(&pointer.cdn_key).to_string();

    AttachmentDisplayData {
        id: attachment_id,
        filename: pointer.file_name.clone(),
        size_display: format_file_size(pointer.plaintext_size),
        mime_type: pointer.content_type.clone(),
        category: category.to_string(),
        thumbnail_url: String::new(),
        download_url: pointer.cdn_key.clone(), // CDN key for download
        local_path: None, // Not downloaded yet
        is_image,
        is_video,
        width: pointer.width as i32,
        height: pointer.height as i32,
        is_playing: false,
        icon: icon.to_string(),
        icon_color,
        waveform,
        duration_display,
        is_uploading: false,
        is_downloading: needs_download,
        progress: 0.0,
        thumbnail_bytes: pointer.thumbnail.clone(),
        thumbnail_pixels: None, // Pre-decode with decode_thumbnail() before UI thread
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
    pub delivery_status: String,
    // Call message fields
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
    }

    /// Convert to Slint MessageItemData with current user context.
    ///
    /// The `current_user_id` is used to determine call direction (outgoing vs incoming).
    pub fn to_slint_with_user(&self, current_user_id: &str) -> MessageItemData {
        let slint_attachments: Vec<MessageAttachmentData> = self
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

    MessageItemData {
        message_id: self.message_id.clone().into(),
        sender_id: self.sender_id.clone().into(),
        sender_name: self.sender_name.clone().into(),
        sender_initials: self.sender_initials.clone().into(),
        sender_avatar_url: SharedString::default(),
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
        reply_count: self.reply_count,
        reply_count_text: self.reply_count_text.clone().into(),
        is_thread_reply: self.is_thread_reply,
        show_sender: true,
        // For own messages with empty status, default to "sent"
        delivery_status: if is_own && self.delivery_status.is_empty() {
            "sent".into()
        } else {
            self.delivery_status.clone().into()
        },
        is_pinned: false,
        is_call: self.is_call,
        call_type: call_type.into(),
        call_duration: self.call_duration.clone().into(),
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
            use chatter_linux::domain::message::CallMessageType;
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
            .join(" "),
        reply_count,
        reply_count_text,
        is_thread_reply: msg.is_thread_reply(),
        delivery_status: msg.delivery_status.clone(),
        is_call,
        call_message_type,
        call_initiator_id,
        call_duration,
    }
}

/// Convert a domain Message to MessageDisplayData with pre-converted attachments.
///
/// Use this when you have already converted attachments (e.g., using pointer fallback).
pub fn message_to_display_data_with_attachments(
    msg: &Message,
    sender_name: &str,
    attachment_data: Vec<AttachmentDisplayData>,
) -> MessageDisplayData {
    let reply_count_text = format_count(0, "reply", "replies");

    // Extract call information if this is a call message
    let (is_call, call_message_type, call_initiator_id, call_duration) =
        if let MessageContent::Call(call_msg) = &msg.content {
            use chatter_linux::domain::message::CallMessageType;
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
        has_attachments: !attachment_data.is_empty(),
        attachment_count: attachment_data.len() as i32,
        attachments: attachment_data,
        has_reactions: !msg.reaction_counts.is_empty(),
        reaction_summary: msg
            .reaction_counts
            .iter()
            .map(|r| format!("{} {}", r.emoji, r.count))
            .collect::<Vec<_>>()
            .join(" "),
        reply_count: 0,
        reply_count_text,
        is_thread_reply: msg.is_thread_reply(),
        delivery_status: msg.delivery_status.clone(),
        is_call,
        call_message_type,
        call_initiator_id,
        call_duration,
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

/// Convert avatar bytes to a Slint Image.
///
/// Returns None if the bytes are empty or can't be decoded.
pub fn avatar_bytes_to_image(bytes: &[u8]) -> Option<slint::Image> {
    if bytes.is_empty() {
        return None;
    }
    match image::load_from_memory(bytes) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let (w, h) = rgba.dimensions();
            let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                rgba.as_raw(),
                w,
                h,
            );
            Some(slint::Image::from_rgba8(buffer))
        }
        Err(e) => {
            tracing::warn!("Failed to decode avatar image: {:?}", e);
            None
        }
    }
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
pub fn mime_to_category(mime_type: &str) -> &'static str {
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

/// Convert an AnonGroupMessage to MessageDisplayData.
///
/// Used for displaying messages in private groups.
pub fn anon_group_message_to_display_data(
    msg: &AnonGroupMessage,
    sender_name: &str,
    resolved_attachments: &[Attachment],
) -> MessageDisplayData {
    use chrono::Local;

    let content = msg.as_text().unwrap_or_default();
    let attachment_data: Vec<AttachmentDisplayData> = resolved_attachments
        .iter()
        .map(attachment_to_display_data)
        .collect();

    MessageDisplayData {
        message_id: msg.id.as_str().to_string(),
        sender_id: msg.sender_id.to_string(),
        sender_name: sender_name.to_string(),
        sender_initials: get_initials(sender_name),
        content: content.to_string(),
        timestamp: msg.created_at.with_timezone(&Local).format("%H:%M").to_string(),
        // Use sent_at for ordering (sender's canonical timestamp), not created_at (local storage time)
        timestamp_ns: msg.sent_at.to_string(),
        is_edited: false,
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
            .join(" "),
        reply_count: 0,
        reply_count_text: String::new(),
        is_thread_reply: false,
        delivery_status: msg.delivery_status.clone(),
        is_call: false,
        call_message_type: String::new(),
        call_initiator_id: String::new(),
        call_duration: String::new(),
    }
}

/// Convert an AnonDmMessage to MessageDisplayData.
///
/// Used for displaying messages in anonymous DM conversations.
pub fn anon_dm_message_to_display_data(
    msg: &AnonDmMessage,
    sender_name: &str,
    resolved_attachments: &[Attachment],
) -> MessageDisplayData {
    use chrono::Local;

    let content = msg.as_text().unwrap_or_default();
    let attachment_data: Vec<AttachmentDisplayData> = resolved_attachments
        .iter()
        .map(attachment_to_display_data)
        .collect();

    MessageDisplayData {
        message_id: msg.id.as_str().to_string(),
        sender_id: msg.sender_id.to_string(),
        sender_name: sender_name.to_string(),
        sender_initials: get_initials(sender_name),
        content: content.to_string(),
        timestamp: msg.created_at.with_timezone(&Local).format("%H:%M").to_string(),
        // Use sent_at for ordering (sender's canonical timestamp), not created_at (local storage time)
        timestamp_ns: msg.sent_at.to_string(),
        is_edited: false,
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
            .join(" "),
        reply_count: 0,
        reply_count_text: String::new(),
        is_thread_reply: false,
        delivery_status: msg.delivery_status.clone(),
        is_call: false,
        call_message_type: String::new(),
        call_initiator_id: String::new(),
        call_duration: String::new(),
    }
}
