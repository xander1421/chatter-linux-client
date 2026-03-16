//! Mailbox token tables migration.
//!
//! Creates tables for blind signature token management.

use super::Migration;

/// Mailbox token tables migration (v011).
///
/// Creates the mailbox-related tables:
/// - `mailbox_server_keys`: Server blind signature public keys (for key rotation)
/// - `mailbox_pending_blinds`: Pending blinding operations (awaiting server signature)
/// - `mailbox_tokens`: Ready-to-use tokens with unblinded signatures
/// - `mailbox_quota`: Daily quota tracking (single row)
pub struct V011MailboxTokens;

impl Migration for V011MailboxTokens {
    fn version(&self) -> u32 {
        11
    }

    fn description(&self) -> &'static str {
        "Mailbox token tables - server keys, pending blinds, tokens, quota"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Server blind signature public keys (for key rotation)
        CREATE TABLE mailbox_server_keys (
            key_id TEXT PRIMARY KEY,
            public_key_der BLOB NOT NULL,
            valid_until INTEGER NOT NULL,
            created_at TEXT NOT NULL
        );

        -- Pending blinding operations (awaiting server signature)
        CREATE TABLE mailbox_pending_blinds (
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

        -- Ready-to-use tokens
        CREATE TABLE mailbox_tokens (
            id TEXT PRIMARY KEY,
            ephemeral_public_key BLOB NOT NULL UNIQUE,
            ephemeral_private_key BLOB NOT NULL,
            signature BLOB NOT NULL,
            message_randomizer BLOB,
            server_key_id TEXT NOT NULL,
            created_at TEXT NOT NULL,
            expires_at TEXT,
            used_at TEXT,
            FOREIGN KEY (server_key_id) REFERENCES mailbox_server_keys(key_id)
        );
        CREATE INDEX idx_mailbox_tokens_unused ON mailbox_tokens(used_at) WHERE used_at IS NULL;

        -- Quota tracking (single row table)
        CREATE TABLE mailbox_quota (
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
            DROP TABLE IF EXISTS mailbox_quota;
            DROP INDEX IF EXISTS idx_mailbox_tokens_unused;
            DROP TABLE IF EXISTS mailbox_tokens;
            DROP TABLE IF EXISTS mailbox_pending_blinds;
            DROP TABLE IF EXISTS mailbox_server_keys;
            "#,
        )
    }
}
