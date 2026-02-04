//! Group-specific mailbox handling.
//!
//! Group mailbox operations for privacy-preserving group messaging.

pub mod coordination;
pub mod fanout;
pub mod member_mailbox;

pub use member_mailbox::{GroupMailbox, GroupMailboxManager, GroupMailboxState, MemberMailbox};
