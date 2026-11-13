//! Business logic / Use cases layer.
//!
//! Service layer orchestrating domain, storage, network, and crypto.

pub mod account_service;
pub mod activity_service;
pub mod ai;
pub mod ai_service;
#[cfg(target_os = "android")]
mod android_aac;
pub mod anon_dm;
pub mod anon_group;
pub(crate) mod anon_shared;
pub mod appearance;
pub mod attachment;
pub mod auth_service;
#[cfg(feature = "media")]
pub mod call_history_recorder;
#[cfg(feature = "media")]
pub mod call_notification_bridge;
pub mod call_signaling;
pub mod call_verification;
#[cfg(feature = "media")]
pub mod call_worker;
pub mod channel;
pub mod channel_event_service;
pub mod channel_stream;
pub mod configuration_sync;
pub mod conversation_pins;
pub mod current_user;
pub mod device_service;
pub mod disappearing;
pub mod dm_bootstrap_service;
pub mod ephemeral_service;
pub mod gateway;
#[cfg(feature = "jarvis")]
pub mod jarvis;
pub mod key_recovery;
pub mod link_device_service;
#[cfg(feature = "local-ai")]
pub mod local_ai_service;
pub mod media_gallery;
#[cfg(feature = "mesh")]
pub mod mesh;
pub mod message;
pub mod message_backup_service;
pub mod notification_service;
pub mod notification_stream;
pub mod pairwise;
pub mod pin_service;
pub mod presence_service;
pub mod profile_visibility;
pub mod quick_switcher;
pub mod remote_ai_service;
pub mod search;
pub(crate) mod secret_avatar;
pub mod section_service;
#[cfg(feature = "local-ai")]
pub mod semantic_search;
pub mod server_records;
pub mod session_service;
pub mod signal;
pub mod svr_backup_service;
pub mod sync_service;
pub mod user;
pub mod validation;

// Re-exports for DM bootstrap service

// Re-exports for channel event service

// Re-export the current-user lookup error.

// Re-exports for call verification


// Re-exports for AI service

pub mod channel_bootstrap_service;
pub mod inbox_processor;
pub mod mailbox_manager;

// Re-exports for user service
pub use user::UserError;

pub use attachment::AttachmentService;
#[cfg(feature = "media")]
pub use call_worker::CallWorkerHandle;
pub use gateway::{
    build_data_message_content,
    build_delete_for_me_content,
    build_edit_content,
    build_pin_content,
    build_pin_content_with_channel,
    build_reply_content,
    // Control message builders
    build_typing_content,
    build_typing_content_with_channel,
};
pub use inbox_processor::InboxEventProcessor;
pub use message::{MessageError, MessageService};
pub use signal::SignalService;

// Re-exports for pairwise encryption service

// Re-exports for anon group service
pub use anon_group::AnonGroupError;

// Re-exports for anon DM service
pub use anon_dm::AnonDmError;

// Re-exports for Jarvis AI assistant

// Re-exports for SVR backup service

// Re-exports for section service

pub mod voice_recording;
