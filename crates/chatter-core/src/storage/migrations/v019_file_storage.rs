//! Migration v019: Add file_path column for filesystem-based attachment storage.
//!
//! Adds a `file_path` column to the attachments table to store the path to
//! encrypted attachment files on the filesystem. This enables efficient handling
//! of large video files (4K/8K) without memory constraints from SQLite BLOBs.
//!
//! ## Architecture Change
//!
//! - **Before**: Large encrypted content stored in `encrypted_content` BLOB
//! - **After**: Large files stored in `~/.local/share/chatter/attachments/{id}.enc`
//!
//! The `content_location` column now supports three states:
//! - 0: NotDownloaded (needs CDN fetch)
//! - 1: InDatabase (legacy BLOB storage, still supported)
//! - 2: OnFilesystem (new file-based storage)

use super::Migration;

pub struct V019FileStorage;

impl Migration for V019FileStorage {
    fn version(&self) -> u32 {
        19
    }

    fn description(&self) -> &'static str {
        "Add file_path column for filesystem attachment storage"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Add file_path column for filesystem-based encrypted content storage
        ALTER TABLE attachments ADD COLUMN file_path TEXT;

        -- Index for quick lookups by file_path (only when populated)
        CREATE INDEX IF NOT EXISTS idx_attachments_file_path
            ON attachments(file_path) WHERE file_path IS NOT NULL;
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
            DROP INDEX IF EXISTS idx_attachments_file_path;
            ALTER TABLE attachments DROP COLUMN file_path;
            "#,
        )
    }
}
