//! V017: Add encrypted content storage to attachments.
//!
//! Adds columns for storing encrypted attachment content directly in the database,
//! enabling secure in-memory-only decryption without touching the filesystem.

use super::Migration;

/// V017: Add encrypted_content BLOB and content_location to attachments.
pub struct V017EncryptedAttachmentContent;

impl Migration for V017EncryptedAttachmentContent {
    fn version(&self) -> u32 {
        17
    }

    fn description(&self) -> &'static str {
        "Add encrypted content storage to attachments"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Add encrypted content BLOB column for storing ciphertext
        -- This holds the full encrypted attachment for in-memory decryption
        ALTER TABLE attachments ADD COLUMN encrypted_content BLOB;

        -- Track where content is stored
        -- 0 = NotDownloaded (needs CDN fetch)
        -- 1 = InDatabase (encrypted_content populated)
        ALTER TABLE attachments ADD COLUMN content_location INTEGER NOT NULL DEFAULT 0;

        -- Index for finding attachments that need download
        CREATE INDEX IF NOT EXISTS idx_attachments_content_location
            ON attachments(content_location);
        "#
    }

    fn down(&self) -> Option<&'static str> {
        // SQLite doesn't support DROP COLUMN directly in older versions
        // We would need to recreate the table, which is destructive
        None
    }
}
