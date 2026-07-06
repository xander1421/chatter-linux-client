//! DM conversation tables migration.
//!
//! Creates tables for DM mailbox state management.

use super::Migration;

/// DM conversation tables migration (v012).
///
/// Creates the DM-related tables:
/// - `dm_mailboxes`: Active DM mailboxes for receiving messages
/// - `dm_peer_mailboxes`: Peer mailbox addresses for sending messages
/// - `dm_conversation_keys`: Shared conversation keys (X25519 derived)
pub struct V012DmConversations;

impl Migration for V012DmConversations {
    fn version(&self) -> u32 {
        12
    }

    fn description(&self) -> &'static str {
        "DM conversation tables - mailboxes, peer mailboxes, conversation keys"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Active DM mailboxes (our mailboxes for receiving)
        CREATE TABLE dm_mailboxes (
            id TEXT PRIMARY KEY,
            conversation_id TEXT NOT NULL,
            mailbox_public_key BLOB NOT NULL,
            mailbox_private_key BLOB NOT NULL,
            signature BLOB NOT NULL,
            randomizer BLOB,
            token_id TEXT NOT NULL,
            created_at TEXT NOT NULL,
            expires_at TEXT NOT NULL,
            last_polled_at TEXT,
            FOREIGN KEY (token_id) REFERENCES mailbox_tokens(id)
        );
        CREATE INDEX idx_dm_mailboxes_conversation ON dm_mailboxes(conversation_id);
        CREATE INDEX idx_dm_mailboxes_expires ON dm_mailboxes(expires_at);

        -- Peer mailbox addresses (their mailboxes for sending)
        CREATE TABLE dm_peer_mailboxes (
            id TEXT PRIMARY KEY,
            conversation_id TEXT NOT NULL,
            peer_mailbox_public_key BLOB NOT NULL,
            received_at TEXT NOT NULL,
            expires_at TEXT,
            UNIQUE(conversation_id, peer_mailbox_public_key)
        );
        CREATE INDEX idx_dm_peer_mailboxes_conversation ON dm_peer_mailboxes(conversation_id);

        -- Shared conversation keys (for message encryption)
        CREATE TABLE dm_conversation_keys (
            id TEXT PRIMARY KEY,
            conversation_id TEXT NOT NULL,
            our_ephemeral_public BLOB NOT NULL,
            our_ephemeral_private BLOB NOT NULL,
            their_ephemeral_public BLOB,
            shared_secret BLOB,
            key_version INTEGER NOT NULL DEFAULT 1,
            created_at TEXT NOT NULL,
            established_at TEXT,
            UNIQUE(conversation_id, key_version)
        );
        CREATE INDEX idx_dm_conversation_keys_conversation ON dm_conversation_keys(conversation_id);
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
            DROP INDEX IF EXISTS idx_dm_conversation_keys_conversation;
            DROP TABLE IF EXISTS dm_conversation_keys;
            DROP INDEX IF EXISTS idx_dm_peer_mailboxes_conversation;
            DROP TABLE IF EXISTS dm_peer_mailboxes;
            DROP INDEX IF EXISTS idx_dm_mailboxes_expires;
            DROP INDEX IF EXISTS idx_dm_mailboxes_conversation;
            DROP TABLE IF EXISTS dm_mailboxes;
            "#,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_migration_version() {
        let migration = V012DmConversations;
        assert_eq!(migration.version(), 12);
    }

    #[test]
    fn test_migration_description() {
        let migration = V012DmConversations;
        assert!(migration.description().contains("DM conversation"));
    }

    #[test]
    fn test_migration_up_contains_tables() {
        let migration = V012DmConversations;
        let up = migration.up();
        assert!(up.contains("CREATE TABLE dm_mailboxes"));
        assert!(up.contains("CREATE TABLE dm_peer_mailboxes"));
        assert!(up.contains("CREATE TABLE dm_conversation_keys"));
    }

    #[test]
    fn test_migration_down() {
        let migration = V012DmConversations;
        assert!(migration.down().is_some());
        let down = migration.down().unwrap();
        assert!(down.contains("DROP TABLE IF EXISTS dm_mailboxes"));
        assert!(down.contains("DROP TABLE IF EXISTS dm_peer_mailboxes"));
        assert!(down.contains("DROP TABLE IF EXISTS dm_conversation_keys"));
    }
}
