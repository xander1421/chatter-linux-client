//! Migration v016: Add sent_at for Signal-style deduplication.
//!
//! Adds `sent_at` column (sender's timestamp) and a unique index on
//! `(sender_id, sent_at)` for Signal-style message deduplication.

use super::Migration;

pub struct V016MessageSentAt;

impl Migration for V016MessageSentAt {
    fn version(&self) -> u32 {
        16
    }

    fn description(&self) -> &'static str {
        "Add sent_at column and unique index for Signal-style deduplication"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Add sender's timestamp for deduplication
        -- This is when the sender created the message (ms since epoch)
        ALTER TABLE messages ADD COLUMN sent_at INTEGER NOT NULL DEFAULT 0;

        -- Signal-style deduplication: unique on (sender_id, sent_at)
        -- This prevents duplicate messages from the same sender with the same timestamp
        CREATE UNIQUE INDEX idx_messages_sender_sent_at
            ON messages(sender_id, sent_at)
            WHERE sender_id IS NOT NULL AND sent_at > 0;
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
            DROP INDEX IF EXISTS idx_messages_sender_sent_at;
            ALTER TABLE messages DROP COLUMN sent_at;
            "#,
        )
    }
}
