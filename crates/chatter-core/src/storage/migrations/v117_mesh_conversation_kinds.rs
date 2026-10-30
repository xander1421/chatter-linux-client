//! Allow the public `#channel` and private-channel kinds in the mesh
//! conversation directory. V058's CHECK predates both, so every insert of
//! either kind failed and those conversations were never persisted. SQLite
//! cannot alter a CHECK, so the table is rebuilt with its rows kept.
use super::Migration;

pub struct V117MeshConversationKinds;

impl Migration for V117MeshConversationKinds {
    fn version(&self) -> u32 {
        117
    }
    fn description(&self) -> &'static str {
        "Mesh conversation kinds for public and private channels"
    }
    fn up(&self) -> &'static str {
        "CREATE TABLE mesh_conversations_v117 (
            conversation_id TEXT PRIMARY KEY NOT NULL,
            kind            TEXT NOT NULL CHECK (kind IN ('room', 'geohash', 'dm', 'channel', 'private-channel')),
            wire_id         TEXT,
            label           TEXT NOT NULL DEFAULT '',
            joined_at       TEXT NOT NULL,
            CHECK ((kind = 'room') = (wire_id IS NULL))
        );
        INSERT INTO mesh_conversations_v117 (conversation_id, kind, wire_id, label, joined_at)
            SELECT conversation_id, kind, wire_id, label, joined_at FROM mesh_conversations;
        DROP TABLE mesh_conversations;
        ALTER TABLE mesh_conversations_v117 RENAME TO mesh_conversations;"
    }
}
