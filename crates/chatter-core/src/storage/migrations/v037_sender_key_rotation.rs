//! V037: Sender key rotation tracking.
//!
//! Adds metadata table for tracking sender key age and message count
//! to support automatic key rotation per Signal best practices
//! (rotate every 24 hours OR every 100 messages).

use super::Migration;

/// V037: Create sender key metadata table for rotation tracking.
pub struct V037SenderKeyRotation;

impl Migration for V037SenderKeyRotation {
    fn version(&self) -> u32 {
        37
    }

    fn description(&self) -> &'static str {
        "Add sender key rotation tracking metadata"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Track sender key usage for rotation decisions
        -- Signal best practice: rotate every 24 hours OR 100 messages
        CREATE TABLE sender_key_metadata (
            -- Channel ID (distribution_id in Signal terms)
            channel_id TEXT PRIMARY KEY,
            -- When the current sender key was created
            created_at TEXT NOT NULL,
            -- Number of messages encrypted with this key
            message_count INTEGER NOT NULL DEFAULT 0,
            -- Last time a message was sent (for staleness detection)
            last_used_at TEXT
        );

        CREATE INDEX idx_sender_key_metadata_created_at ON sender_key_metadata(created_at);
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
            DROP INDEX IF EXISTS idx_sender_key_metadata_created_at;
            DROP TABLE IF EXISTS sender_key_metadata;
            "#,
        )
    }
}
