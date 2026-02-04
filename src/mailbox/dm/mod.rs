//! DM-specific mailbox handling.
//!
//! Direct message mailbox operations for two-way anonymity.

pub mod bootstrap;
pub mod conversation;
pub mod two_way;

// Re-export key types
pub use conversation::{
    ConversationKey, DmConversation, DmConversationManager, DmMailbox, PeerMailbox,
};
