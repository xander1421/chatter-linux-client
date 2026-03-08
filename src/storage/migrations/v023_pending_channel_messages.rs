//! Migration v023: Pending channel messages table.
//!
//! Adds a table to queue channel messages during bootstrap when member
//! mailboxes aren't yet available. Messages are flushed when the member's
//! mailbox address is received.

use super::Migration;

pub struct V023PendingChannelMessages;

impl Migration for V023PendingChannelMessages {
    fn version(&self) -> u32 {
        23
    }

    fn description(&self) -> &'static str {
        "Add pending_channel_messages table for bootstrap queueing"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Pending channel messages during bootstrap
        -- Messages are queued here when member mailboxes aren't ready
        -- and flushed when GroupMailboxAddress is received
        CREATE TABLE IF NOT EXISTS pending_channel_messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            channel_id TEXT NOT NULL,
            member_id TEXT NOT NULL,
            plaintext BLOB NOT NULL,
            timestamp INTEGER NOT NULL,
            created_at TEXT NOT NULL
        );

        -- Index for fast lookup by channel + member
        CREATE INDEX IF NOT EXISTS idx_pending_channel_messages_channel_member
            ON pending_channel_messages(channel_id, member_id);

        -- Index for cleanup of old messages
        CREATE INDEX IF NOT EXISTS idx_pending_channel_messages_timestamp
            ON pending_channel_messages(timestamp);
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
            DROP INDEX IF EXISTS idx_pending_channel_messages_timestamp;
            DROP INDEX IF EXISTS idx_pending_channel_messages_channel_member;
            DROP TABLE IF EXISTS pending_channel_messages;
            "#,
        )
    }
}
