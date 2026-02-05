//! Business logic / Use cases layer.
//!
//! Service layer orchestrating domain, storage, network, and crypto.

pub mod auth_service;
pub mod session_service;
pub mod account_service;
pub mod device_service;
pub mod message_service;
pub mod message_gateway;
pub mod channel_service;
pub mod channel_template;
pub mod user_service;
pub mod status_service;
pub mod attachment_service;
pub mod call_service;
pub mod call_verification;
pub mod notification_service;
pub mod search_service;
pub mod semantic_search;
pub mod sync_service;
pub mod offline_queue;
pub mod moderation_service;
pub mod ai_service;
pub mod local_ai_service;
pub mod ai_summarize;
pub mod draft_service;
pub mod pin_service;
pub mod thread_service;
pub mod reaction_service;
pub mod typing_service;
pub mod read_receipt_service;
pub mod presence_service;
pub mod ephemeral_message;
pub mod ephemeral_service;
pub mod saved_message_service;
pub mod forward_service;
pub mod mention_service;
pub mod embed_service;
pub mod gif_service;

// Re-exports for typing service
pub use typing_service::{TypingError, TypingService};

// Re-exports for read receipt service
pub use read_receipt_service::{ReadReceiptError, ReadReceiptService};

// Re-export ReadState from aggregator for convenience
pub use crate::network::streaming::read_receipt_stream::ReadState;
