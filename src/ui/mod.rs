//! Slint UI layer.
//!
//! Rust to Slint UI bridge and adapters.

pub mod adapters;
pub mod bridge;
pub mod chat_data;
pub mod commands;
pub mod emoji_data;
pub mod file_data;
pub mod handlers;
pub mod picker_data;
pub mod sidebar_data;
pub mod widgets;

// Re-export bridge types for convenience
pub use bridge::UiBridge;

// Re-export chat data types
pub use chat_data::{
    extract_content_text, format_timestamp, initials_from_name, message_to_item_data,
    reaction_summary_string, ChatHeaderData, MessageItemData, PinnedBarData, ThreadPanelData,
};

// Re-export emoji data types
pub use emoji_data::{get_emoji_by_category, get_emoji_list, search_emoji, EmojiEntry};

// Re-export picker data types
pub use picker_data::{GifData, MentionSuggestionData};

// Re-export sidebar data types
pub use sidebar_data::{
    presence_color, ChannelListItemData, DMListItemData, UserProfileBarData,
};

// Re-export file data types
pub use file_data::{category_icon, AttachmentItemData};
