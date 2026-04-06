//! Migration v025: Anonymous DM tables.
//!
//! Creates tables for client-side managed anonymous DM conversations.
//! These conversations are invisible to the server (NDSS'21 style).

use super::Migration;

pub struct V025AnonDm;

impl Migration for V025AnonDm {
    fn version(&self) -> u32 {
        25
    }

    fn description(&self) -> &'static str {
        "Anonymous DM tables for client-side managed conversations"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Anonymous DM conversations (client-side managed)
        CREATE TABLE IF NOT EXISTS anon_dm_conversations (
            id TEXT PRIMARY KEY,
            peer_id TEXT NOT NULL,
            initiated_by_us INTEGER NOT NULL DEFAULT 0,
            status INTEGER NOT NULL DEFAULT 0,
            peer_mailbox_pk BLOB,
            peer_profile_key BLOB,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_anon_dm_conversations_peer_id ON anon_dm_conversations(peer_id);
        CREATE INDEX IF NOT EXISTS idx_anon_dm_conversations_status ON anon_dm_conversations(status);

        -- Anonymous DM mailboxes (our ephemeral mailbox per conversation)
        CREATE TABLE IF NOT EXISTS anon_dm_mailboxes (
            id TEXT PRIMARY KEY,
            conversation_id TEXT NOT NULL,
            public_key BLOB NOT NULL,
            private_key BLOB NOT NULL,
            token_id TEXT NOT NULL,
            created_at TEXT NOT NULL,
            expires_at TEXT NOT NULL,
            FOREIGN KEY (conversation_id) REFERENCES anon_dm_conversations(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_anon_dm_mailboxes_conversation ON anon_dm_mailboxes(conversation_id);
        CREATE INDEX IF NOT EXISTS idx_anon_dm_mailboxes_expires ON anon_dm_mailboxes(expires_at);

        -- Anonymous DM messages
        CREATE TABLE IF NOT EXISTS anon_dm_messages (
            id TEXT PRIMARY KEY,
            conversation_id TEXT NOT NULL,
            sender_id TEXT NOT NULL,
            content_type INTEGER NOT NULL,
            content_json TEXT NOT NULL,
            attachments_blob BLOB,
            is_deleted INTEGER NOT NULL DEFAULT 0,
            is_read INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL,
            FOREIGN KEY (conversation_id) REFERENCES anon_dm_conversations(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_anon_dm_messages_conversation ON anon_dm_messages(conversation_id);
        CREATE INDEX IF NOT EXISTS idx_anon_dm_messages_created ON anon_dm_messages(conversation_id, created_at);
        CREATE INDEX IF NOT EXISTS idx_anon_dm_messages_unread ON anon_dm_messages(conversation_id, is_read) WHERE is_read = 0;
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(r#"
        DROP TABLE IF EXISTS anon_dm_messages;
        DROP TABLE IF EXISTS anon_dm_mailboxes;
        DROP TABLE IF EXISTS anon_dm_conversations;
        "#)
    }
}
