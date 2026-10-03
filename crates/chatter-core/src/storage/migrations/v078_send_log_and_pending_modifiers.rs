//! The message send log, and edits/deletes that arrived before their target.
//!
//! `send_log_payloads` / `send_log_recipients` replace the `message.dm_sent_log`
//! settings blob, which pointed at the stored message row and so resent its
//! CURRENT (edited) body and nothing that has no row: Signal-Desktop's MSL
//! shape (`sendLogPayloads` / `sendLogRecipients`), holding the exact
//! plaintext `Content` per send and one row per device it reached.
//! `pending_message_modifiers` holds an edit or delete-for-everyone until the
//! message it targets is stored (`message_modifier_repo`).
use super::Migration;
pub struct V078SendLogAndPendingModifiers;
impl Migration for V078SendLogAndPendingModifiers {
    fn version(&self) -> u32 { 78 }
    fn description(&self) -> &'static str { "Message send log and pending edit/delete modifiers" }
    fn up(&self) -> &'static str {
        r#"
        CREATE TABLE send_log_payloads (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            content BLOB NOT NULL,
            message_ts INTEGER,
            expires_at INTEGER NOT NULL
        );
        CREATE INDEX send_log_payloads_expires ON send_log_payloads(expires_at);
        CREATE TABLE send_log_recipients (
            payload_id INTEGER NOT NULL REFERENCES send_log_payloads(id) ON DELETE CASCADE,
            peer_id TEXT NOT NULL,
            device_id INTEGER NOT NULL,
            server_ts_ms INTEGER NOT NULL,
            PRIMARY KEY (peer_id, device_id, server_ts_ms)
        );
        CREATE INDEX send_log_recipients_payload ON send_log_recipients(payload_id);
        CREATE TABLE pending_message_modifiers (
            author_id TEXT NOT NULL,
            target_sent_at INTEGER NOT NULL,
            kind TEXT NOT NULL CHECK (kind IN ('edit', 'delete')),
            conversation_id TEXT,
            body TEXT,
            edit_ts INTEGER,
            received_at INTEGER NOT NULL,
            PRIMARY KEY (author_id, target_sent_at, kind)
        );
        CREATE INDEX pending_message_modifiers_received ON pending_message_modifiers(received_at);
        DELETE FROM settings WHERE key = 'message.dm_sent_log';
        "#
    }
}
