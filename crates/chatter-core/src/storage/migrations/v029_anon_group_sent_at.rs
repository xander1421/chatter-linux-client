//! Migration v029: Add sent_at to anon_group_messages.
//!
//! Adds sender's timestamp for consistent message ordering across devices.
//! Matches the same pattern as anon DMs and regular messages.

use super::Migration;

pub struct V029AnonGroupSentAt;

impl Migration for V029AnonGroupSentAt {
    fn version(&self) -> u32 {
        29
    }

    fn description(&self) -> &'static str {
        "Add sent_at to anon_group_messages for consistent ordering"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Add sent_at column (sender's timestamp in nanoseconds)
        ALTER TABLE anon_group_messages ADD COLUMN sent_at INTEGER NOT NULL DEFAULT 0;

        -- Populate sent_at from created_at for existing messages
        UPDATE anon_group_messages
        SET sent_at = CAST(strftime('%s', created_at) AS INTEGER) * 1000000000;

        -- Create index for efficient ordering and lookup
        CREATE INDEX IF NOT EXISTS idx_anon_group_messages_sent_at
        ON anon_group_messages(group_id, sent_at DESC, sender_id DESC);
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
        DROP INDEX IF EXISTS idx_anon_group_messages_sent_at;
        "#,
        )
    }
}
