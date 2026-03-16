//! V007: Drafts table migration.
//!
//! Adds table for message drafts with per-channel/thread persistence.

use super::Migration;

/// V007: Create drafts table.
pub struct V007Drafts;

impl Migration for V007Drafts {
    fn version(&self) -> u32 {
        7
    }

    fn description(&self) -> &'static str {
        "Create drafts table"
    }

    fn up(&self) -> &'static str {
        // Empty vec serialized with bincode is 8 bytes (u64 length = 0)
        r#"
        CREATE TABLE drafts (
            id TEXT PRIMARY KEY,
            -- DraftKey fields (flattened, not JSON)
            channel_id TEXT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
            thread_id TEXT,  -- NULL for main channel drafts
            -- Content
            content TEXT NOT NULL DEFAULT '',
            reply_to TEXT,  -- MessageId being replied to
            -- Arrays stored as bincode BLOB
            attachments_blob BLOB NOT NULL DEFAULT X'0800000000000000',
            mentions_blob BLOB NOT NULL DEFAULT X'0800000000000000',
            -- Timestamps
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        -- Unique constraint: one draft per channel+thread combination
        -- Uses COALESCE to handle NULL thread_id (SQLite treats NULLs as distinct)
        CREATE UNIQUE INDEX idx_drafts_key ON drafts(channel_id, COALESCE(thread_id, ''));

        -- Index for listing all drafts (for sync/export)
        CREATE INDEX idx_drafts_updated ON drafts(updated_at DESC);
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some("DROP TABLE IF EXISTS drafts;")
    }
}
