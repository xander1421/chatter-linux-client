//! Drop `attachments.local_path`. Every attachment row is made with it unset:
//! content lives in `content_location` + the file store, so nothing reads it.
use super::Migration;

pub struct V107DropAttachmentLocalPath;
impl Migration for V107DropAttachmentLocalPath {
    fn version(&self) -> u32 {
        107
    }
    fn description(&self) -> &'static str {
        "Drop the unused attachment local path"
    }
    fn up(&self) -> &'static str {
        "ALTER TABLE attachments DROP COLUMN local_path;"
    }
}
