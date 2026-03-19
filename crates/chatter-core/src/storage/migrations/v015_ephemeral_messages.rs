//! Ephemeral messages migration.
//!
//! Creates table for tracking ephemeral (self-destructing) message state.

use super::Migration;

/// Ephemeral messages migration (v015).
///
/// Creates the ephemeral_messages table for tracking TTL and expiration state.
/// This is a separate table from messages to enable efficient cleanup queries
/// without scanning all messages.
pub struct V015EphemeralMessages;

impl Migration for V015EphemeralMessages {
    fn version(&self) -> u32 {
        15
    }

    fn description(&self) -> &'static str {
        "Ephemeral messages - TTL tracking table"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Ephemeral message tracking
        CREATE TABLE ephemeral_messages (
            message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
            ttl_seconds INTEGER NOT NULL,
            expires_at TEXT NOT NULL,
            read_at TEXT,
            countdown_started INTEGER NOT NULL DEFAULT 0
        );

        -- Index for cleanup queries (find expired messages)
        CREATE INDEX idx_ephemeral_expires ON ephemeral_messages(expires_at)
            WHERE countdown_started = 1;

        -- Index for unread ephemeral messages (pending countdown)
        CREATE INDEX idx_ephemeral_pending ON ephemeral_messages(message_id)
            WHERE countdown_started = 0;
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
            DROP INDEX IF EXISTS idx_ephemeral_pending;
            DROP INDEX IF EXISTS idx_ephemeral_expires;
            DROP TABLE IF EXISTS ephemeral_messages;
            "#,
        )
    }
}
