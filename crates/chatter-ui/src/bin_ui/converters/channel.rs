//! Channel and DM converters.

use chatter_core::ui::initials_from_name;

use chatter_core::ui::sidebar_data::{ChatRowSendData, FriendRequestSendData};

use super::utils::avatar_bytes_to_image;

pub trait ChatRowExt {
    fn to_slint(self) -> crate::ChatRowData;
}

impl ChatRowExt for ChatRowSendData {
    /// UI thread only: builds the row's `slint::Image`.
    fn to_slint(self) -> crate::ChatRowData {
        let (avatar_source, has_avatar) = if let Some((w, h, pixels)) = self.avatar_decoded {
            (
                crate::bin_ui::slint_conversions::rgba_to_slint_image(&pixels, w, h),
                true,
            )
        } else if let Some(bytes) = &self.avatar_bytes {
            match avatar_bytes_to_image(bytes) {
                Some(img) => (img, true),
                None => (slint::Image::default(), false),
            }
        } else {
            (slint::Image::default(), false)
        };

        crate::ChatRowData {
            conversation_id: self.id.into(),
            conversation_type: super::conversation_kind(&self.kind),
            user_id: self.user_id.into(),
            display_name: self.display_name.into(),
            avatar_source,
            has_avatar,
            initials: self.initials.into(),
            last_message: self.last_message.into(),
            timestamp: self.timestamp.into(),
            presence_status: self.presence_status.into(),
            private_channel: self.private_channel,
            member_count: self.member_count as i32,
            unread_count: self.unread_count as i32,
            is_typing: self.is_typing,
            is_pinned: self.is_pinned,
            can_leave: self.can_leave,
            can_delete: self.can_delete,
            is_selected: false,
            arrival_serial: 0,
        }
    }
}

pub trait FriendDataExt {
    fn to_slint(self) -> crate::FriendData;
}

impl FriendDataExt for chatter_core::ui::sidebar_data::FriendDataSendData {
    fn to_slint(self) -> crate::FriendData {
        let (avatar_source, has_avatar) = if let Some((w, h, pixels)) = self.avatar_decoded {
            (
                crate::bin_ui::slint_conversions::rgba_to_slint_image(&pixels, w, h),
                true,
            )
        } else if let Some(bytes) = &self.avatar_bytes {
            match avatar_bytes_to_image(bytes) {
                Some(img) => (img, true),
                None => (slint::Image::default(), false),
            }
        } else {
            (slint::Image::default(), false)
        };
        crate::FriendData {
            initials: initials_from_name(&self.display_name).into(),
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
            (
                crate::bin_ui::slint_conversions::rgba_to_slint_image(&pixels, w, h),
                true,
            )
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
            note: "".into(),
        }
    }
}

/// Avatar bytes for a private-group invite candidate: anon-DM peers keep
/// theirs on the anon DM row, friends on their local profile.
pub fn invite_candidate_avatar(
    ctx: &chatter_core::client::ChatterClient,
    c: &chatter_core::client::AnonGroupInviteCandidate,
) -> Option<Vec<u8>> {
    if c.is_anonymous {
        ctx.anon_dm_peer_avatar(&c.user_id)
    } else {
        ctx.user_avatar(&c.user_id)
    }
}

/// Private-group picker row. UI thread only (decodes the avatar).
pub fn invite_candidate_to_slint(
    c: chatter_core::client::AnonGroupInviteCandidate,
    avatar: Option<Vec<u8>>,
) -> crate::PrivateGroupFriend {
    crate::PrivateGroupFriend {
        user_id: c.user_id.to_string().into(),
        display_name: c.display_name.into(),
        username: if c.is_anonymous {
            "(anonymous)".into()
        } else {
            "".into()
        },
        selected: false,
        role: "member".into(),
        avatar_source: avatar
            .as_deref()
            .and_then(avatar_bytes_to_image)
            .unwrap_or_default(),
        anonymous: c.is_anonymous,
    }
}

/// Thin presentation mapping: core member → UI row. UI thread only (decodes
/// the avatar into a `slint::Image`).
pub fn member_to_slint(
    m: &chatter_core::domain::channel::MemberWithProfile,
) -> crate::ChannelMemberData {
    crate::ChannelMemberData {
        user_id: m.membership.user_id.to_string().into(),
        display_name: m.display_name.as_str().into(),
        username: m.username.as_str().into(),
        role: m.membership.role.as_str().into(),
        joined_at: "".into(),
        avatar_source: m
            .avatar_bytes
            .as_deref()
            .and_then(avatar_bytes_to_image)
            .unwrap_or_default(),
        muted: m.membership.is_muted,
        can_ban: m.moderation.can_ban,
        can_mute: m.moderation.can_mute,
        can_make_owner: m.moderation.can_make_owner,
        unverified: m.unverified,
        // Blocking within a conversation is a private-group control.
        can_block: false,
        blocked: false,
    }
}

/// Thin presentation mapping: core ban → UI row. UI thread only.
pub fn banned_member_to_slint(
    b: &chatter_core::domain::channel::BannedMember,
) -> crate::BannedMemberData {
    crate::BannedMemberData {
        user_id: b.user_id.to_string().into(),
        display_name: b.display_name.as_str().into(),
        avatar_source: b
            .avatar_bytes
            .as_deref()
            .and_then(avatar_bytes_to_image)
            .unwrap_or_default(),
    }
}
