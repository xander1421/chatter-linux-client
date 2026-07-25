//! V058: Durable directory of open mesh conversations.
//!
//! Mesh messages are already rows in `messages`, but the sidebar entry that
//! points at them was in memory only. The mapping from a conversation UUID back
//! to the peer or cell that produced it is one-way — UUID v5 over the wire
//! identifier — so it cannot be recovered from the stored messages.
//!
//! The visible consequence was a mesh DM whose history sat on disk with nothing
//! in the sidebar pointing at it until that peer happened to be heard from
//! again. That reads as lost data, so the directory is durable now.
//!
//! `wire_id` is the peer ID for a DM and the cell for a geohash channel, and is
//! NULL for the room (there is only one, and it is not something you join).

use super::Migration;

pub struct V058MeshConversations;

impl Migration for V058MeshConversations {
    fn version(&self) -> u32 {
        58
    }

    fn description(&self) -> &'static str {
        "Durable directory of open bitchat mesh conversations"
    }

    fn up(&self) -> &'static str {
        r#"
        CREATE TABLE IF NOT EXISTS mesh_conversations (
            conversation_id TEXT PRIMARY KEY NOT NULL,
            kind            TEXT NOT NULL CHECK (kind IN ('room', 'geohash', 'dm')),
            wire_id         TEXT,
            label           TEXT NOT NULL DEFAULT '',
            joined_at       TEXT NOT NULL,
            CHECK ((kind = 'room') = (wire_id IS NULL))
        );
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some("DROP TABLE IF EXISTS mesh_conversations;")
    }
}
