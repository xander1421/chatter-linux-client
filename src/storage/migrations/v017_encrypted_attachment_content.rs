//! V017: Add encrypted content storage to attachments.
//!
//! NOTE: These columns are now included in v005_attachments. This migration
//! is kept as a no-op for version number continuity.

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
        // No-op: encrypted_content and content_location are now in v005_attachments base schema
        "SELECT 1;"
    }

    fn down(&self) -> Option<&'static str> {
        None
    }
}
