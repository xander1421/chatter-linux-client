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

// Re-export all public handlers
pub use actions::{
    handle_dm_delete_for_everyone,
    handle_dm_delete_for_me,
    handle_dm_edit,
    handle_dm_pin,
    handle_dm_reaction,
    handle_dm_receipt,
    handle_dm_typing,
};
pub use download::auto_download_attachments;
pub use friends::handle_friend_accepted;
pub use receive::{handle_dm_received, handle_dm_unread_count};
