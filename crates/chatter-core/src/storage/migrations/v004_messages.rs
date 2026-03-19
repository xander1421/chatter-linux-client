//! Message tables migration.
//!
//! Creates tables for messages and threads.

use super::Migration;

/// Message tables migration (v004).
///
/// Creates the message-related tables:
/// - `messages`: Chat messages with JSON content
/// - `threads`: Thread metadata for conversation branches
pub struct V004Messages;

impl Migration for V004Messages {
    fn version(&self) -> u32 {
        4
    }

    fn description(&self) -> &'static str {
        "Message tables - messages, threads"
    }

    fn up(&self) -> &'static str {
        // Empty vec serialized with bincode is 8 bytes (u64 length = 0)
        r#"
        -- Messages
        CREATE TABLE messages (
            id TEXT PRIMARY KEY,
            channel_id TEXT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
            sender_id TEXT,  -- NULL for sealed sender
            -- Content stored as JSON (MessageContent is complex enum)
            content_type INTEGER NOT NULL,  -- Discriminant for quick filtering
            content_json TEXT NOT NULL,     -- Full serialized MessageContent (kept for FTS5)
            thread_id TEXT,
            reply_to TEXT,
            -- Attachments stored as bincode BLOB
            attachments_blob BLOB NOT NULL DEFAULT X'0800000000000000',
            is_pinned INTEGER NOT NULL DEFAULT 0,
            is_edited INTEGER NOT NULL DEFAULT 0,
            is_deleted INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            edited_at TEXT,
            sent_at INTEGER NOT NULL DEFAULT 0,
            -- Reaction data as bincode BLOB (for local caching)
            reaction_counts_blob BLOB NOT NULL DEFAULT X'0800000000000000',
            current_user_reactions_blob BLOB NOT NULL DEFAULT X'0800000000000000',
            -- AI-related fields
            mentioned_integrations_blob BLOB NOT NULL DEFAULT X'0800000000000000',
            targets_ai_bot INTEGER NOT NULL DEFAULT 0
        );

        -- Indexes for common queries
        CREATE INDEX idx_messages_channel_created ON messages(channel_id, created_at DESC);
        CREATE INDEX idx_messages_thread ON messages(thread_id) WHERE thread_id IS NOT NULL;
        CREATE INDEX idx_messages_sender ON messages(sender_id) WHERE sender_id IS NOT NULL;
        CREATE INDEX idx_messages_pinned ON messages(channel_id) WHERE is_pinned = 1;

        -- Signal-style deduplication: unique on (sender_id, sent_at)
        CREATE UNIQUE INDEX idx_messages_sender_sent_at
            ON messages(sender_id, sent_at)
            WHERE sender_id IS NOT NULL AND sent_at > 0;

        -- Threads metadata (derived/cached)
        CREATE TABLE threads (
            root_message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
            channel_id TEXT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
            reply_count INTEGER NOT NULL DEFAULT 0,
            last_reply_at TEXT,
            participant_ids_blob BLOB NOT NULL DEFAULT X'0800000000000000'
        );
        CREATE INDEX idx_threads_channel ON threads(channel_id);
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
            DROP INDEX IF EXISTS idx_threads_channel;
            DROP TABLE IF EXISTS threads;
            DROP INDEX IF EXISTS idx_messages_pinned;
            DROP INDEX IF EXISTS idx_messages_sender;
            DROP INDEX IF EXISTS idx_messages_thread;
            DROP INDEX IF EXISTS idx_messages_channel_created;
            DROP TABLE IF EXISTS messages;
            "#,
        )
    }
}
