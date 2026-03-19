//! Service type re-exports for external consumers.
//!
//! Provides clean import paths for service types without exposing internal module structure.
//! Use `chatter_core::service_types::X` instead of `chatter_core::services::X`.

// Service types
pub use crate::services::attachment::AttachmentService;
pub use crate::services::channel::ChannelService;
pub use crate::services::message::MessageService;
pub use crate::services::user::{FriendRequestFilter, UserService};
pub use crate::services::signal::SignalService;
pub use crate::services::session_service::{Session, SessionService};
pub use crate::services::anon_dm::{AnonDmService, PendingAnonCall};
pub use crate::services::anon_group::AnonGroupService;
pub use crate::services::mailbox_manager::MailboxManager;
pub use crate::services::call_worker::CallWorkerHandle;
pub use crate::services::conversation::ConversationHandle;
pub use crate::services::jarvis::JarvisBackgroundBuilder;
pub use crate::services::pin_service::PinService;
pub use crate::services::call_verification::CallVerification;
pub use crate::services::ai_service::AIService;

// Mailbox types needed by call setup
pub use crate::mailbox::CallRoomToken;

// Storage types needed by tests
pub use crate::storage::Database;
pub use crate::storage::AttachmentRepository;

// Settings key constants
pub use crate::storage::settings_keys;

// Gateway builders and types
pub use crate::services::message_gateway::{
    MessageGateway,
    build_text_content, build_call_key_offer, build_call_key_answer,
    build_group_call_key_content, build_call_hangup_content, CallHangupReason,
    build_delivery_receipt_content, build_delivery_receipt_content_with_channel,
    build_read_receipt_content, build_read_receipt_content_with_channel,
};

/// Gateway module re-exports (for `use chatter_core::service_types::gateway;` pattern).
pub mod gateway {
    pub use crate::services::gateway::*;

    /// Shared utilities (preview truncation, error formatting).
    pub mod shared {
        pub use crate::services::gateway::shared::*;
    }
}
