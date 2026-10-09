//! A received server DM this account deleted (for me, by expiry, with its
//! chat, or by its author for everyone), remembered by what identifies it
//! across copies: conversation, sender and `sent_at`. The row id is random per
//! receipt, so the `deleted_messages` id tombstone never matches a second
//! copy, which a resend after a DecryptionErrorMessage or a retry re-encrypts.
use super::Migration;

pub struct V099DmReceiveTombstones;
impl Migration for V099DmReceiveTombstones {
    fn version(&self) -> u32 {
        99
    }
    fn description(&self) -> &'static str {
        "Server DM receive tombstones by sender and sent_at"
    }
    fn up(&self) -> &'static str {
        // `deleted_at` is Unix seconds, for the retention bound
        // (`unified_message_repo` DM_RECEIVE_TOMBSTONE_SECONDS).
        "CREATE TABLE IF NOT EXISTS dm_receive_tombstones (
            owner_id TEXT NOT NULL,
            conversation_id TEXT NOT NULL,
            sender_id TEXT NOT NULL,
            sent_at INTEGER NOT NULL,
            deleted_at INTEGER NOT NULL,
            PRIMARY KEY (owner_id, conversation_id, sender_id, sent_at)
        );
        CREATE INDEX IF NOT EXISTS idx_dm_receive_tombstones_deleted_at
            ON dm_receive_tombstones (deleted_at);"
    }
}
