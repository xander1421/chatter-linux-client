//! Call room token tables migration.
//!
//! Creates tables for anonymous call room token management.
//! Uses the same blind signature infrastructure as mailbox tokens.

use super::Migration;

/// Call room token tables migration (v036).
///
/// Creates the call room token tables:
/// - `call_room_tokens`: Ready-to-use tokens for anonymous call rooms
/// - `call_room_pending_blinds`: Pending blinding operations
/// - `call_room_quota`: Daily quota tracking (separate from mailbox quota)
pub struct V036CallRoomTokens;

impl Migration for V036CallRoomTokens {
    fn version(&self) -> u32 {
        36
    }

    fn description(&self) -> &'static str {
        "Call room token tables for anonymous calls"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Pending blinding operations for call room tokens
        CREATE TABLE call_room_pending_blinds (
            id TEXT PRIMARY KEY,
            ephemeral_public_key BLOB NOT NULL,
            ephemeral_private_key BLOB NOT NULL,
            blinded_message BLOB NOT NULL,
            blinding_secret BLOB NOT NULL,
            message_randomizer BLOB,
            server_key_id TEXT NOT NULL,
            created_at TEXT NOT NULL,
            FOREIGN KEY (server_key_id) REFERENCES mailbox_server_keys(key_id)
        );

        -- Ready-to-use call room tokens
        CREATE TABLE call_room_tokens (
            id TEXT PRIMARY KEY,
            ephemeral_public_key BLOB NOT NULL UNIQUE,
            ephemeral_private_key BLOB NOT NULL,
            signature BLOB NOT NULL,
            message_randomizer BLOB,
            server_key_id TEXT NOT NULL,
            created_at TEXT NOT NULL,
            used_at TEXT,
            FOREIGN KEY (server_key_id) REFERENCES mailbox_server_keys(key_id)
        );
        CREATE INDEX idx_call_room_tokens_unused ON call_room_tokens(used_at) WHERE used_at IS NULL;

        -- Quota tracking for call room tokens (separate from mailbox quota)
        CREATE TABLE call_room_quota (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            tokens_used_today INTEGER NOT NULL DEFAULT 0,
            quota_resets_at INTEGER NOT NULL,
            last_updated TEXT NOT NULL
        );
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
            DROP TABLE IF EXISTS call_room_quota;
            DROP INDEX IF EXISTS idx_call_room_tokens_unused;
            DROP TABLE IF EXISTS call_room_tokens;
            DROP TABLE IF EXISTS call_room_pending_blinds;
            "#,
        )
    }
}
