//! Channel and DM converters.

use slint::SharedString;

use chatter_core::client::ChatterClient;
use chatter_core::domain::channel::Channel;
use chatter_core::ui::sidebar_data::{DMListItemSendData, FriendRequestSendData};

use super::utils::{avatar_bytes_to_image, channel_type_to_string};

/// Convert a domain Channel to Slint ChannelListItemData.
///
/// Decodes `channel.icon` (raw image bytes) into a `slint::Image` at the
/// UI-thread boundary only — no image-decode or caching logic lives here.
pub fn channel_to_slint(channel: &Channel, _ui_ctx: &ChatterClient) -> crate::ChannelListItemData {
    let (avatar_source, has_avatar) = match channel.icon.as_deref() {
        Some(bytes) if !bytes.is_empty() => match avatar_bytes_to_image(bytes) {
            Some(img) => (img, true),
            None => (slint::Image::default(), false),
        },
        _ => (slint::Image::default(), false),
    };

    crate::ChannelListItemData {
        channel_id: channel.id.to_string().into(),
        name: channel.name.clone().into(),
        channel_type: channel_type_to_string(channel.channel_type).into(),
        unread_count: channel.unread_count as i32,
        last_message: SharedString::default(),
        timestamp: SharedString::default(),
        avatar_source,
        has_avatar,
        is_selected: false,
    }
}

pub trait DMListItemExt {
    fn to_slint(self) -> crate::DMListItemData;
}

impl DMListItemExt for DMListItemSendData {
    fn to_slint(self) -> crate::DMListItemData {
        let (avatar_source, has_avatar) = if let Some((w, h, pixels)) = self.avatar_decoded {
            let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                &pixels, w, h
            );
            (slint::Image::from_rgba8(buffer), true)
        } else if let Some(bytes) = &self.avatar_bytes {
            match avatar_bytes_to_image(bytes) {
                Some(img) => (img, true),
                None => (slint::Image::default(), false),
            }
        } else {
            (slint::Image::default(), false)
        };

        crate::DMListItemData {
            channel_id: self.channel_id.into(),
            user_id: self.user_id.into(),
            display_name: self.display_name.into(),
            avatar_source,
            has_avatar,
            initials: self.initials.into(),
            presence_status: self.presence_status.into(),
            last_message: self.last_message.into(),
            timestamp: self.timestamp.into(),
            unread_count: self.unread_count as i32,
            is_typing: self.is_typing,
            is_anon: self.is_anon,
            is_selected: false,
        }
    }
}


pub trait FriendDataExt {
    fn to_slint(self) -> crate::FriendData;
}

impl FriendDataExt for chatter_core::ui::sidebar_data::FriendDataSendData {
    fn to_slint(self) -> crate::FriendData {
        let (avatar_source, has_avatar) = if let Some((w, h, pixels)) = self.avatar_decoded {
            let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                &pixels, w, h,
            );
            (slint::Image::from_rgba8(buffer), true)
        } else if let Some(bytes) = &self.avatar_bytes {
            match avatar_bytes_to_image(bytes) {
                Some(img) => (img, true),
                None => (slint::Image::default(), false),
            }
        } else {
            (slint::Image::default(), false)
        };
        crate::FriendData {
            user_id: self.user_id.into(),
            display_name: self.display_name.into(),
            username: self.username.into(),
            presence_status: self.presence_status.into(),
            custom_status: self.custom_status.into(),
            is_anon: self.is_anon,
            avatar_source,
            has_avatar,
        }
    }
}

pub trait FriendRequestExt {
    fn to_slint(self) -> crate::FriendRequestData;
}

impl FriendRequestExt for FriendRequestSendData {
    fn to_slint(self) -> crate::FriendRequestData {
        let (avatar_source, has_avatar) = if let Some((w, h, pixels)) = self.avatar_decoded {
            let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                &pixels, w, h
            );
            (slint::Image::from_rgba8(buffer), true)
        } else if let Some(bytes) = &self.avatar_bytes {
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
            avatar_source,
            has_avatar,
            sent_at: self.sent_at.into(),
        }
    }
}

