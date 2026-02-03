//! V005: Attachments table migration.
//!
//! Adds tables for file attachments and thumbnail cache.

use super::Migration;

/// V005: Create attachments and thumbnails tables.
pub struct V005Attachments;

impl Migration for V005Attachments {
    fn version(&self) -> u32 {
        5
    }

    fn description(&self) -> &'static str {
        "Create attachments and thumbnails tables"
    }

    fn up(&self) -> &'static str {
        r#"
        CREATE TABLE attachments (
            id TEXT PRIMARY KEY,
            -- File metadata (stored as JSON for nested structure)
            metadata_json TEXT NOT NULL,
            -- Status enum as integer: 0=Pending, 1=Uploading, 2=Processing, 3=Ready, 4=Failed, 5=Expired
            status INTEGER NOT NULL DEFAULT 0,
            -- File paths
            local_path TEXT,
            remote_url TEXT,
            -- Encryption data (BLOB for binary)
            encryption_key BLOB,
            encryption_iv BLOB,
            -- Timestamps
            created_at TEXT NOT NULL
        );

        -- Index for listing attachments by status (pending uploads, etc.)
        CREATE INDEX idx_attachments_status ON attachments(status);

        -- Thumbnails cache table
        CREATE TABLE thumbnails (
            attachment_id TEXT PRIMARY KEY REFERENCES attachments(id) ON DELETE CASCADE,
            local_path TEXT NOT NULL,
            width INTEGER NOT NULL,
            height INTEGER NOT NULL,
            created_at TEXT NOT NULL
        );
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
            DROP TABLE IF EXISTS thumbnails;
            DROP TABLE IF EXISTS attachments;
            "#,
        )
    }
}
