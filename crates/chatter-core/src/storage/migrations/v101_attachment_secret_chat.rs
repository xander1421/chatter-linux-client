//! Record on each attachment row whether it was created for a secret chat
//! (anonymous DM / group), so its downloads go over Tor by an explicit marker
//! rather than a best-effort index lookup.
use super::Migration;

pub struct V101AttachmentSecretChat;
impl Migration for V101AttachmentSecretChat {
    fn version(&self) -> u32 {
        101
    }
    fn description(&self) -> &'static str {
        "Attachment secret-chat origin marker"
    }
    fn up(&self) -> &'static str {
        // 1 = created for a secret chat, 0 = not. Every insert writes it; NULL
        // only on rows that predate this column, which routing resolves from
        // the messages carrying them (`AttachmentService::for_download`).
        "ALTER TABLE attachments ADD COLUMN secret_chat INTEGER;"
    }
}
