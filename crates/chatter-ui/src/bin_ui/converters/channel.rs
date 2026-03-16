//! Channel and DM converters.

use slint::SharedString;

use chatter_linux::domain::channel::Channel;

use super::utils::{avatar_bytes_to_image, channel_type_to_string, get_initials};
use crate::{ChannelListItemData, DMListItemData};

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
