//! DM-specific mailbox handling.
//!
//! Direct message mailbox operations for two-way anonymity.

pub mod bootstrap;
pub mod conversation;
pub mod two_way;

// Re-export key types
pub use bootstrap::{
    BootstrapResponse, BootstrapResult, DEFAULT_MAILBOX_TTL_SECONDS, DmBootstrap,
    release_bootstrap_lock, try_acquire_bootstrap_lock,
};
pub use conversation::{
    ConversationKey, DmConversation, DmConversationManager, DmMailbox, PeerMailbox,
};
pub use two_way::{PollResult, ReceivedMessage, TwoWayAnonymity};
