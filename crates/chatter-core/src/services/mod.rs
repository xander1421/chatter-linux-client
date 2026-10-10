//! Business logic / Use cases layer.
//!
//! Service layer orchestrating domain, storage, network, and crypto.

pub mod account_service;
pub mod ai;
pub mod ai_service;
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
pub mod conversation;
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
pub mod local_ai_service;
#[cfg(feature = "mesh")]
pub mod mesh;
pub mod message;
pub mod message_backup_service;
pub mod notification_service;
pub mod notification_stream;
pub mod outgoing_typing;
pub mod pairwise;
pub mod pin_service;
pub mod presence_service;
pub mod profile_visibility;
pub mod quick_switcher;
pub mod search;
pub mod section_service;
pub mod semantic_search;
pub mod server_records;
pub mod session_service;
pub mod signal;
pub mod svr_backup_service;
pub mod sync_service;
pub mod user;
pub mod validation;

// Re-exports for DM bootstrap service
pub use dm_bootstrap_service::DmBootstrapService;

// Re-exports for channel event service
pub use channel_event_service::{ChannelEventError, ChannelEventService};

// Re-exports for current user provider
pub use current_user::{CurrentUserError, CurrentUserProvider};

// Re-exports for call verification
pub use call_verification::{CallVerification, CallVerificationData, VERIFICATION_EMOJI_COUNT};

// Re-export ReactionEvent from aggregator for convenience
pub use crate::network::streaming::reaction_stream::ReactionEvent;

// Re-exports for AI service
pub use ai_service::{AIError, AIService};

pub mod channel_bootstrap_service;
pub mod inbox_processor;
pub mod mailbox_manager;

// Re-exports for user service
pub use user::{FriendResponseResult, UserError, UserService};

pub use attachment::{AttachmentError, AttachmentService, CdnUploadResult};
#[cfg(feature = "media")]
pub use call_worker::CallWorkerHandle;
pub use gateway::{
    CallHangupReason,
    DmContent,
    MessageGateway,
    PADDING_GROUP_CONTROL,
    PADDING_LARGE,
    PADDING_MESSAGE,
    RouteResult,
    // Call hangup
    build_call_hangup_content,
    build_call_key_answer,
    // DM call key exchange (ephemeral keys for PFS)
    build_call_key_offer,
    build_data_message_content,
    build_data_message_content_with_quote,
    build_delete_for_me_content,
    build_delivery_receipt_content,
    build_delivery_receipt_content_with_channel,
    build_edit_content,
    build_edit_content_with_channel,
    // Group call key distribution
    build_group_call_signal_content,
    build_pin_content,
    build_pin_content_with_channel,
    build_reaction_content,
    build_reaction_content_with_channel,
    build_read_receipt_content,
    build_read_receipt_content_with_channel,
    build_reply_content,
    build_saved_message_sync_content,
    // DataMessage builders
    build_targeted_delete_content,
    build_text_content,
    // Control message builders
    build_typing_content,
    build_typing_content_with_channel,
    // Padding
    generate_padding,
};
pub use inbox_processor::InboxEventProcessor;
pub use mailbox_manager::{MailboxError, MailboxManager};
pub use message::core::{
    proto_content_to_domain, proto_to_domain_message, proto_to_domain_message_with_sender,
};
pub use message::{MessageError, MessageService, SendDmContentParams};
pub use signal::SignalService;

// Re-exports for pairwise encryption service
pub use pairwise::{PairwiseError, PairwiseService};

// Re-exports for anon group service
pub use anon_group::{AnonGroupError, AnonGroupService};

// Re-exports for anon DM service
pub use anon_dm::{AnonDmError, AnonDmService};

// Re-exports for Jarvis AI assistant
#[cfg(feature = "jarvis")]
pub use jarvis::{JarvisBackground, JarvisBackgroundBuilder, JarvisError, JarvisService};

// Re-exports for SVR backup service
pub use message_backup_service::{BackupStats, MessageBackupError, MessageBackupService};
pub use svr_backup_service::{RestoreResult, SvrBackupError, SvrBackupService};

// Re-exports for section service
pub use section_service::{SectionError, SectionService};

pub mod voice_recording;
