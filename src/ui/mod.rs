//! Slint UI layer.
//!
//! Rust to Slint UI bridge and adapters.

pub mod adapters;
pub mod bridge;
pub mod commands;
pub mod emoji_data;
pub mod handlers;
pub mod picker_data;
pub mod sidebar_data;
pub mod widgets;

// Re-export bridge types for convenience
pub use bridge::UiBridge;

// Re-export emoji data types
pub use emoji_data::{get_emoji_by_category, get_emoji_list, search_emoji, EmojiEntry};

// Re-export picker data types
pub use picker_data::{GifData, MentionSuggestionData};

// Re-export sidebar data types
pub use sidebar_data::{
    presence_color, ChannelListItemData, DMListItemData, UserProfileBarData,
};
