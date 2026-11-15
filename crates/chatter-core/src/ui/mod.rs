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

// Presentation rules owned by the services that produce the data. The UI
// calls these to label, order or validate what it shows, so the rule lives
// once in core instead of being copied into chatter-ui.
#[cfg(feature = "jarvis")]
pub use crate::services::jarvis::parse_slash_command;
pub use crate::services::gateway::shared::truncate_preview;
pub use crate::services::media_gallery::ordered_media;
#[cfg(feature = "mesh")]
pub use crate::services::mesh::mapping::wire_message_id;
pub use crate::services::message::{
    advance_delivery_status, display_delivery_status, retained_history_range,
};
pub use crate::services::notification_service::is_opaque_notification_key;
pub use crate::services::search::preview_fragments;
pub use crate::services::section_service::replace_section_conversation;
pub use crate::services::server_records::merge_pages;
pub use crate::services::session_service::revocable_count;
pub use crate::services::user::{friend_display_name_placeholder, is_lookup_handle};
pub use crate::services::validation::validate_registration_username;

/// Disappearing-timer presets, labels and input parsing for the timer menu.
pub mod disappearing {
    pub use crate::services::disappearing::{
        DEFAULT_ACTIVATION_SECONDS, DmTimer, TIMER_PRESETS, activation_minutes, timer_short_label,
    };
}
