//! DM conversation state and management.
//!
//! Per-conversation ephemeral mailbox and key state for two-way anonymity.

mod manager;
mod state;

pub use manager::DmConversationManager;
pub use state::{ConversationKey, DmConversation, DmMailbox, PeerMailbox};

#[cfg(test)]
mod tests;
