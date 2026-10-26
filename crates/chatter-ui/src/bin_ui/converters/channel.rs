//! Channel and DM converters.

use crate::bin_ui::converters::initials_from_name;

use chatter_core::ui::sidebar_data::{
    ChatRowLabel, ChatRowSendData, chat_row_accessible_label,
};

use super::utils::avatar_bytes_to_image;

/// Request row assembled for the Friends modal.
#[derive(Clone, Debug)]
pub struct FriendRequestSendData {
    pub user_id: String,
    pub display_name: String,
    pub username: String,
    pub avatar_bytes: Option<Vec<u8>>,
    pub secondary_text: String,
}

pub trait ChatRowExt {
    fn to_slint(self) -> crate::ChatRowData;
}

impl ChatRowExt for ChatRowSendData {
    /// UI thread only: builds the row's `slint::Image`.
    fn to_slint(self) -> crate::ChatRowData {
        let (avatar_source, has_avatar) = if let Some(bytes) = &self.avatar_bytes {
            match avatar_bytes_to_image(bytes) {
                Some(img) => (img, true),
                None => (slint::Image::default(), false),
            }
        } else {
            (slint::Image::default(), false)
        };

        let initials = initials_from_name(&self.display_name);
        let display_name = match self.label {
            ChatRowLabel::Plain => self.display_name,
            ChatRowLabel::IncomingSecretRequest => format!("\u{1F4E9} {}", self.display_name),
            ChatRowLabel::OutgoingSecretRequest => {
                format!("\u{1F4EE} {} (pending)", self.display_name)
            }
        };
        let accessible_label_text =
            chat_row_accessible_label(&display_name, self.unread_count, &self.last_message);
        crate::ChatRowData {
            conversation_id: self.id.into(),
            conversation_type: super::conversation_kind(&self.kind),
            user_id: self.user_id.into(),
            display_name: display_name.into(),
            accessible_label_text: accessible_label_text.into(),
            avatar_source,
            has_avatar,
            initials: initials.into(),
            last_message: self.last_message.into(),
            draft_preview: self.draft_preview.into(),
            timestamp: self
                .timestamp_at
                .map(super::sidebar_time)
                .unwrap_or_default()
                .into(),
            presence_status: self.presence_status.into(),
            private_channel: self.private_channel,
            member_count: self.member_count as i32,
            unread_count: self.unread_count as i32,
            mention_count: self.mention_count as i32,
            is_typing: self.is_typing,
            is_pinned: self.is_pinned,
            is_muted: self.is_muted,
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
        let (avatar_source, has_avatar) = if let Some(bytes) = &self.avatar_bytes {
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
            last_seen_label: "".into(),
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
        let (avatar_source, has_avatar) = if let Some(bytes) = &self.avatar_bytes {
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
            sent_at: self.secondary_text.into(),
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
        username: c.username.into(),
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
        invited: false,
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
