use super::Migration;

pub struct V123AttachmentCleanup;

impl Migration for V123AttachmentCleanup {
    fn version(&self) -> u32 { 123 }

    fn description(&self) -> &'static str { "Retain attachment cleanup work across crashes" }

    fn up(&self) -> &'static str {
        "CREATE TABLE attachment_cleanup_queue (attachment_id TEXT PRIMARY KEY);"
    }
}
