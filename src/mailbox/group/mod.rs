//! Group-specific mailbox handling.
//!
//! Group mailbox operations for privacy-preserving group messaging.
//!
//! This module provides:
//! - [`GroupMailbox`] - Our mailbox for receiving messages in a group
//! - [`MemberMailbox`] - Other members' mailbox addresses for sending
//! - [`GroupMailboxState`] - Combined state for a group
//! - [`GroupMailboxManager`] - Database operations for group mailbox state
//! - [`GroupFanout`] - N-way message fanout to all group members
//! - [`FanoutResult`] - Per-member delivery result
//! - [`GroupFanoutResult`] - Complete fanout operation result
//! - [`GroupPollResult`] - Result of polling a group mailbox
//! - [`GroupCoordinator`] - Mailbox creation and member address exchange

pub mod coordination;
pub mod fanout;
pub mod member_mailbox;

// Re-export member mailbox types
pub use member_mailbox::{GroupMailbox, GroupMailboxManager, GroupMailboxState, MemberMailbox};

// Re-export fanout types
pub use fanout::{FanoutResult, GroupFanout, GroupFanoutResult, GroupPollResult};

// Re-export coordination types
pub use coordination::{GroupCoordinator, GroupMailboxSetupResult};
