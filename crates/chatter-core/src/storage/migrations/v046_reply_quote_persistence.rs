//! Persist reply quote context in the messages table.
//!
//! Adds `reply_to_author_id` and `reply_to_text` columns so that quote
//! bubbles survive a channel reload instead of being transient.

use super::Migration;

/// Add reply quote persistence columns (v046).
pub struct V046ReplyQuotePersistence;

impl Migration for V046ReplyQuotePersistence {
    fn version(&self) -> u32 {
        46
    }

    fn description(&self) -> &'static str {
        "Add reply_to_author_id and reply_to_text columns to messages"
    }

    fn up(&self) -> &'static str {
        r#"
        ALTER TABLE messages ADD COLUMN reply_to_author_id TEXT;
        ALTER TABLE messages ADD COLUMN reply_to_text TEXT;
        "#
    }

    fn down(&self) -> Option<&'static str> {
        None
    }
}
