//! DMs held back until a first-contact bootstrap completes get a table of their
//! own; the Sesame session tables go.
//!
//! `sesame_users` / `sesame_devices` / `sesame_sessions` (v065) were never
//! written by a production path: libsignal's `SessionRecord` already keeps the
//! archived states and promotes whichever one decrypts. The only live use was
//! `sesame_pending_messages`, whose foreign key into `sesame_users` cannot
//! outlive that table, so the queue moves to `pending_dm_messages`.
use super::Migration;
pub struct V077PendingDmMessages;
impl Migration for V077PendingDmMessages {
    fn version(&self) -> u32 {
        77
    }
    fn description(&self) -> &'static str {
        "Pending-DM queue replaces the unused Sesame tables"
    }
    fn up(&self) -> &'static str {
        r#"
        CREATE TABLE pending_dm_messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            peer_id TEXT NOT NULL,
            content BLOB NOT NULL,
            attachment_ids_blob BLOB,
            channel_id TEXT
        );
        CREATE INDEX pending_dm_messages_peer ON pending_dm_messages(peer_id);
        DROP INDEX IF EXISTS idx_sesame_pending_user;
        DROP TABLE IF EXISTS sesame_pending_messages;
        DROP INDEX IF EXISTS idx_sesame_sessions_user_device;
        DROP TABLE IF EXISTS sesame_sessions;
        DROP TABLE IF EXISTS sesame_devices;
        DROP TABLE IF EXISTS sesame_users;
        "#
    }
}
