//! Raw data and ordering helpers consumed by the client presenter.

pub mod call_history_data;
pub mod chat_data;
pub mod display;
pub mod emoji_data;
mod emoji_table;
pub mod file_data;
pub mod format;
pub mod markdown;
pub mod message_row;
pub mod picker_history;
pub mod pin_preview;
pub mod shared_content_data;
pub mod sidebar_data;

pub use call_history_data::CallHistoryRowData;
pub use shared_content_data::{SharedContentItemData, SharedTabData, shared_tabs};
