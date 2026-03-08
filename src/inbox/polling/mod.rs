//! Polling handlers for different inbox types.
//!
//! This module contains the polling logic for:
//! - Device inbox (regular messages via Signal Protocol)
//! - DM mailboxes (two-way anonymous messaging)
//! - Group mailboxes (per-group ephemeral mailboxes for privacy)
//! - Anonymous DM mailboxes (stranger messaging)
//! - Friend requests (periodic polling for new requests)

pub mod anon_dm;
pub mod device;
pub mod dm;
pub mod friends;
pub mod group;

pub use anon_dm::{process_anon_dm_results, ProcessedAnonDmMessages};
pub use device::{dispatch_channel_control_content, process_device_message};
pub use dm::poll_dm_mailboxes;
pub use friends::{poll_friend_requests, FriendRequestPoller};
pub use group::poll_group_mailboxes;
