//! Migration v028: Add sent_at to anon_dm_messages.
//!
//! Adds sender's timestamp for consistent message ordering across devices.
//! Regular DMs use sent_at for ordering - anon DMs should too.

use super::Migration;

pub struct V028AnonDmSentAt;

impl Migration for V028AnonDmSentAt {
    fn version(&self) -> u32 {
        28
    }

    fn description(&self) -> &'static str {
        "Add sent_at to anon_dm_messages for consistent ordering"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Add sent_at column (sender's timestamp in nanoseconds)
        ALTER TABLE anon_dm_messages ADD COLUMN sent_at INTEGER NOT NULL DEFAULT 0;

        -- Populate sent_at from created_at for existing messages
        UPDATE anon_dm_messages
        SET sent_at = CAST(strftime('%s', created_at) AS INTEGER) * 1000000000;

        -- Create index for efficient ordering
        CREATE INDEX IF NOT EXISTS idx_anon_dm_messages_sent_at
        ON anon_dm_messages(conversation_id, sent_at DESC, sender_id DESC);
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(r#"
        DROP INDEX IF EXISTS idx_anon_dm_messages_sent_at;
        -- SQLite doesn't support DROP COLUMN, would need table rebuild
        "#)
    }
}
