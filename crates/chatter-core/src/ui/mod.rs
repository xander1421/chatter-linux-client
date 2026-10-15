//! Slint UI layer.
//!
//! Rust to Slint UI bridge and adapters.

pub mod call_history_data;
pub mod chat_data;
pub mod emoji_data;
pub mod file_data;
pub mod picker_history;
mod registration_avatar;
pub mod shared_content_data;
pub mod sidebar_data;

pub use call_history_data::{CallHistoryRowData, format_call_duration};
pub use chat_data::{format_timestamp, initials_from_name};
pub use registration_avatar::registration_avatar;
pub use shared_content_data::{SharedContentItemData, SharedTabData, shared_tabs};
