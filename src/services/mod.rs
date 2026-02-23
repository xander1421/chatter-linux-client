//! Business logic / Use cases layer.
//!
//! Service layer orchestrating domain, storage, network, and crypto.

pub mod auth_service;
pub mod session_service;
pub mod account_service;
pub mod device_service;
pub mod message_service;
pub mod message_gateway;
pub mod channel;
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

// Re-exports for reaction service
pub use reaction_service::{ReactionError, ReactionService};

// Re-export ReadState from aggregator for convenience
pub use crate::network::streaming::read_receipt_stream::ReadState;

// Re-export ReactionEvent from aggregator for convenience
pub use crate::network::streaming::reaction_stream::ReactionEvent;

// Re-exports for search service
pub use search_service::{CombinedSearchResults, SearchError, SearchService};

// Re-exports for moderation service
pub use moderation_service::{ModerationError, ModerationService, proto_moderation_action_to_domain};

// Re-exports for AI service
pub use ai_service::{AIError, AIService};

// Re-exports for AI summarization service
pub use ai_summarize::{AISummarizeService, SummarizeError};

// Re-exports for saved message service
pub use saved_message_service::{DecryptedSavedMessage, SavedMessageError, SavedMessageService};

// Re-export SavedMessageSource from crypto for convenience
pub use crate::crypto::SavedMessageSource;
pub mod signal_service;
pub mod mailbox_manager;

// Re-exports for user service
pub use user_service::{FriendResponseResult, UserError, UserService};

pub use signal_service::SignalService;
pub use mailbox_manager::{MailboxManager, MailboxError, ActiveMailbox};
pub use message_gateway::{
    DmContent, MessageGateway, RouteResult,
    build_text_content, build_data_message_content, build_typing_content, build_typing_content_with_channel,
    build_read_receipt_content, build_delivery_receipt_content,
    build_reaction_content, build_reaction_content_with_channel,
    build_pin_content, build_pin_content_with_channel,
    build_edit_content, build_edit_content_with_channel,
    build_delete_for_me_content,
    build_delete_for_everyone_content, build_delete_for_everyone_content_with_channel,
};
pub use message_service::MessageService;
pub use attachment_service::{AttachmentService, AttachmentError, UploadResult, CdnUploadResult};
pub use call_service::CallService;
