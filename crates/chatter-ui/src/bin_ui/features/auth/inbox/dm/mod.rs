//! DM-specific event handlers.
//!
//! Handles direct message events: received, typing, receipts, reactions, pins, edits, deletes.
//!
//! This module is organized into:
//! - `receive` - Message receiving and sidebar updates
//! - `download` - Attachment auto-download and thumbnail generation
//! - `actions` - Typing, receipts, reactions, pins, edits, deletes
//! - `friends` - Friend acceptance handling

mod actions;
mod download;
mod friends;
mod receive;

// DM action handlers now unified in unified_actions.rs
pub use download::auto_download_attachments;
pub use friends::handle_friend_accepted;
pub use receive::{handle_dm_received, handle_dm_unread_count};
