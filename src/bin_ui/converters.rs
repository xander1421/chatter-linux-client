//! Domain to Slint type conversions.
//!
//! Pure functions for converting domain types to Slint UI types.

use slint::SharedString;

use chatter_linux::domain::channel::{Channel, ChannelType};
use chatter_linux::domain::message::Message;

// Import Slint-generated types from crate root (where slint::include_modules! is called)
use crate::{ChannelListItemData, DMListItemData, MessageItemData};

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

/// Convert a domain Message to Slint MessageItemData.
pub fn message_to_slint(msg: &Message, sender_name: &str) -> MessageItemData {
    MessageItemData {
        message_id: msg.id.to_string().into(),
        sender_id: msg.sender_id.map(|id| id.to_string()).unwrap_or_default().into(),
        sender_name: sender_name.to_string().into(),
        sender_initials: get_initials(sender_name).into(),
        sender_avatar_url: SharedString::default(),
        content: msg.as_text().unwrap_or("").to_string().into(),
        timestamp: msg.created_at.format("%H:%M").to_string().into(),
        is_own: false, // Set by caller based on current user
        is_edited: msg.is_edited,
        is_deleted: msg.is_deleted,
        is_system: false,
        is_ai: msg.targets_ai_bot,
        has_attachments: msg.has_attachments(),
        attachment_count: msg.attachments.len() as i32,
        has_reactions: !msg.reaction_counts.is_empty(),
        reaction_summary: msg
            .reaction_counts
            .iter()
            .map(|r| format!("{} {}", r.emoji, r.count))
            .collect::<Vec<_>>()
            .join(" ")
            .into(),
        reply_count: 0,
        is_thread_reply: msg.is_thread_reply(),
        show_sender: true,
    }
}

/// Convert ChannelType enum to string for Slint.
pub fn channel_type_to_string(ct: ChannelType) -> &'static str {
    match ct {
        ChannelType::Public => "public",
        ChannelType::Private => "private",
        ChannelType::DirectMessage => "dm",
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
