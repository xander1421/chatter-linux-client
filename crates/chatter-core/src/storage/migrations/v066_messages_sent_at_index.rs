//! V066: index `messages(sent_at)`.
//!
//! Two receipt paths look messages up by send timestamp alone --
//! `mark_read_by_timestamp` and `update_delivery_status_by_timestamp_with_lookup`
//! -- because a self-synced read receipt carries no destination on the wire.
//! Neither could use `idx_messages_sender_sent_at`, whose leading columns are
//! `conversation_type, conversation_id, sender_id`, so both full-scanned the
//! message table once per timestamp in a receipt batch.

use super::Migration;

pub struct V066MessagesSentAtIndex;

impl Migration for V066MessagesSentAtIndex {
    fn version(&self) -> u32 {
        66
    }

    fn description(&self) -> &'static str {
        "Index messages(sent_at) for timestamp-keyed receipt lookups"
    }

    fn up(&self) -> &'static str {
        r#"
        CREATE INDEX IF NOT EXISTS idx_messages_sent_at
            ON messages(sent_at);
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some("DROP INDEX IF EXISTS idx_messages_sent_at;")
    }
}
