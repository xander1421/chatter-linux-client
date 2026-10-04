//! Slint UI layer.
//!
//! Rust to Slint UI bridge and adapters.

pub mod call_history_data;
pub mod chat_data;
pub mod emoji_data;
pub mod file_data;
pub mod recent_emoji;
pub mod shared_content_data;
pub mod sidebar_data;

pub use call_history_data::{format_call_duration, CallHistoryRowData};
pub use chat_data::{format_timestamp, initials_from_name};
pub use shared_content_data::{shared_tabs, SharedContentItemData, SharedTabData};
