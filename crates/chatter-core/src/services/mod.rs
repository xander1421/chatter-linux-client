//! Business logic / Use cases layer.
//!
//! Service layer orchestrating domain, storage, network, and crypto.

pub mod ai;
pub mod call_signaling;
pub mod conversation;
pub mod current_user;
pub mod auth_service;
pub mod link_device_service;
pub mod session_service;
pub mod account_service;
pub mod device_service;
pub mod message;
pub mod gateway;
pub mod channel;
pub mod user;
pub mod attachment;
#[cfg(feature = "media")]
pub mod call_worker;
#[cfg(feature = "media")]
pub mod call_notification_bridge;
pub mod call_verification;
pub mod notification_service;
pub mod notification_stream;
pub mod search_service;
pub mod semantic_search;
pub mod sync_service;
pub mod moderation_service;
pub mod ai_service;
pub mod local_ai_service;
pub mod ai_summarize;
pub mod draft_service;
pub mod pin_service;
pub mod outgoing_typing;
pub mod presence_service;
pub mod ephemeral_service;
pub mod embed_service;
// Removed - gif_service.rs file was deleted
pub mod quick_switcher;
pub mod search;
pub mod section_service;
pub mod signal;
pub mod channel_event_service;
pub mod dm_bootstrap_service;
pub mod pairwise;
pub mod sealed_sender;
pub mod fanout;
pub mod anon_group;
pub mod anon_dm;
pub(crate) mod anon_shared;
pub mod svr_backup_service;
#[cfg(feature = "jarvis")]
pub mod jarvis;
pub mod validation;

// Re-exports for DM bootstrap service
pub use dm_bootstrap_service::{DmBootstrapError, DmBootstrapService};

// Re-exports for channel event service
pub use channel_event_service::{ChannelEventError, ChannelEventService};

// Re-exports for current user provider
pub use current_user::{CurrentUserError, CurrentUserProvider};

// Re-exports for call verification
pub use call_verification::{CallVerification, CallVerificationData, VERIFICATION_EMOJI_COUNT};

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

pub mod signal_service;
pub mod mailbox_manager;
pub mod inbox_processor;
pub mod channel_bootstrap_service;

// Re-exports for user service
pub use user::{FriendResponseResult, UserError, UserService};

pub use signal_service::SignalService;
pub use mailbox_manager::{MailboxManager, MailboxError, ActiveMailbox};
pub use inbox_processor::InboxEventProcessor;
pub use gateway::{
    DmContent, MessageGateway, RouteResult,
    // Padding
    generate_padding, PADDING_GROUP_CONTROL, PADDING_MESSAGE, PADDING_LARGE,
    // DataMessage builders
    build_text_content, build_data_message_content, build_reply_content,
    build_data_message_content_with_quote,
    // Control message builders
    build_typing_content, build_typing_content_with_channel,
    build_read_receipt_content, build_read_receipt_content_with_channel,
    build_delivery_receipt_content, build_delivery_receipt_content_with_channel,
    build_reaction_content, build_reaction_content_with_channel,
    build_pin_content, build_pin_content_with_channel,
    build_edit_content, build_edit_content_with_channel,
    build_delete_for_me_content,
    build_saved_message_sync_content,
    build_delete_for_everyone_content, build_delete_for_everyone_content_with_channel,
    // DM call key exchange (ephemeral keys for PFS)
    build_call_key_offer, build_call_key_answer,
    // Group call key distribution
    build_group_call_key_content,
    // Call hangup
    build_call_hangup_content, CallHangupReason,
};
pub use message::{MessageService, MessageError, SendDmContentParams, SendDmMsgParams};
pub use message::core::{proto_to_domain_message, proto_to_domain_message_with_sender, proto_content_to_domain};
pub use attachment::{AttachmentService, AttachmentError, CdnUploadResult};
#[cfg(feature = "media")]
pub use call_worker::CallWorkerHandle;

// Re-exports for pairwise encryption service
pub use pairwise::{PairwiseService, PairwiseError};

// Re-exports for sealed sender service
pub use sealed_sender::{SealedSenderService, SealedSenderError};

// Re-exports for fanout utilities
pub use fanout::{fanout, fanout_simple, FanoutOptions, FanoutResult};

// Re-exports for anon group service
pub use anon_group::{AnonGroupError, AnonGroupService};

// Re-exports for anon DM service
pub use anon_dm::{AnonDmError, AnonDmService};

// Re-exports for Jarvis AI assistant
#[cfg(feature = "jarvis")]
pub use jarvis::{JarvisError, JarvisService, JarvisBackground, JarvisBackgroundBuilder};

// Re-exports for SVR backup service
pub use svr_backup_service::{SvrBackupError, SvrBackupService, RestoreResult};

// Re-exports for section service
pub use section_service::{SectionError, SectionService};
