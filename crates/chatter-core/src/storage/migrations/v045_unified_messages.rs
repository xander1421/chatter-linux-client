//! Unified message pipeline migration.
//!
//! Drops the old `messages` table and all separate anon message/reaction tables,
//! then creates a single unified `messages` table with `conversation_id` and
//! `conversation_type` discriminant. Zero users = zero data to preserve.

use super::Migration;

/// Unified message pipeline migration (v045).
///
/// Consolidates three separate message tables into one:
/// - `messages` (channel messages) — DROPPED and recreated with conversation_id
/// - `anon_dm_messages` — DROPPED
/// - `anon_group_messages` — DROPPED
/// - `anon_dm_reactions` — DROPPED
/// - `anon_group_reactions` — DROPPED
pub struct V045UnifiedMessages;

impl Migration for V045UnifiedMessages {
    fn version(&self) -> u32 {
        45
    }

    fn description(&self) -> &'static str {
        "Unify message pipelines - DROP all message tables, CREATE unified messages"
    }

    fn up(&self) -> &'static str {
        r#"
        -- ============================================================
        -- DROP everything: zero users = zero data to preserve
        -- ============================================================

        -- Drop indexes first (SQLite drops them with the table, but be explicit)
        DROP INDEX IF EXISTS idx_messages_channel_created;
        DROP INDEX IF EXISTS idx_messages_thread;
        DROP INDEX IF EXISTS idx_messages_sender;
        DROP INDEX IF EXISTS idx_messages_pinned;
        DROP INDEX IF EXISTS idx_messages_sender_sent_at;
        DROP INDEX IF EXISTS idx_messages_conversation_type;

        -- Drop the old messages table (v004 schema + accumulated ALTERs)
        DROP TABLE IF EXISTS threads;
        DROP TABLE IF EXISTS messages;

        -- Drop separate anon message tables
        DROP TABLE IF EXISTS anon_dm_messages;
        DROP TABLE IF EXISTS anon_group_messages;

        -- Drop separate anon reaction tables
        DROP TABLE IF EXISTS anon_dm_reactions;
        DROP TABLE IF EXISTS anon_group_reactions;

        -- Drop old unified reactions table (v044 will recreate with correct FK)
        DROP TABLE IF EXISTS unified_reactions;
        DROP TABLE IF EXISTS reactions;

        -- ============================================================
        -- CREATE unified messages table
        -- ============================================================

        CREATE TABLE messages (
            id TEXT PRIMARY KEY,
            conversation_type TEXT NOT NULL DEFAULT 'channel',
            conversation_id TEXT NOT NULL,
            sender_id TEXT,
            device_id INTEGER,
            content_type INTEGER NOT NULL,
            content_json TEXT NOT NULL,
            encrypted_content BLOB,
            thread_id TEXT,
            reply_to TEXT,
            attachments_blob BLOB NOT NULL DEFAULT X'0800000000000000',
            is_pinned INTEGER NOT NULL DEFAULT 0,
            is_edited INTEGER NOT NULL DEFAULT 0,
            is_read INTEGER NOT NULL DEFAULT 0,
            is_deleted INTEGER NOT NULL DEFAULT 0,
            delivery_status TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            edited_at TEXT,
            sent_at INTEGER NOT NULL DEFAULT 0,
            reaction_counts_blob BLOB NOT NULL DEFAULT X'0800000000000000',
            current_user_reactions_blob BLOB NOT NULL DEFAULT X'0800000000000000',
            mentioned_integrations_blob BLOB NOT NULL DEFAULT X'0800000000000000',
            targets_ai_bot INTEGER NOT NULL DEFAULT 0
        );

        -- Primary query path: list messages in a conversation
        CREATE INDEX idx_messages_conv_type_id_sent
            ON messages(conversation_type, conversation_id, sent_at DESC);

        -- Thread queries
        CREATE INDEX idx_messages_thread
            ON messages(thread_id) WHERE thread_id IS NOT NULL;

        -- Sender lookups
        CREATE INDEX idx_messages_sender
            ON messages(sender_id) WHERE sender_id IS NOT NULL;

        -- Pinned messages per conversation
        CREATE INDEX idx_messages_pinned
            ON messages(conversation_id) WHERE is_pinned = 1;

        -- Signal dedup: unique on (sender_id, sent_at)
        CREATE UNIQUE INDEX idx_messages_sender_sent_at
            ON messages(sender_id, sent_at)
            WHERE sender_id IS NOT NULL AND sent_at > 0;

        -- Unread queries
        CREATE INDEX idx_messages_unread
            ON messages(conversation_id, is_read) WHERE is_read = 0;

        -- ============================================================
        -- Recreate threads table with conversation_id
        -- ============================================================

        CREATE TABLE threads (
            root_message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
            conversation_id TEXT NOT NULL,
            reply_count INTEGER NOT NULL DEFAULT 0,
            last_reply_at TEXT,
            participant_ids_blob BLOB NOT NULL DEFAULT X'0800000000000000'
        );
        CREATE INDEX idx_threads_conversation ON threads(conversation_id);

        -- ============================================================
        -- Unified reactions table (replaces v044 + old reactions)
        -- ============================================================

        CREATE TABLE unified_reactions (
            id TEXT PRIMARY KEY,
            message_id TEXT NOT NULL,
            user_id TEXT NOT NULL,
            emoji TEXT NOT NULL,
            created_at TEXT NOT NULL,
            UNIQUE(message_id, user_id, emoji),
            FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
        );

        CREATE INDEX idx_ur_message ON unified_reactions(message_id);
        CREATE INDEX idx_ur_user ON unified_reactions(user_id);
        "#
    }

    fn down(&self) -> Option<&'static str> {
        None
    }
}
