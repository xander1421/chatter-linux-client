//! V083: the nickname each mesh sender last announced.
//!
//! Mesh messages are stored with a one-way hash of the peer ID as their sender
//! (`services/mesh/mapping.rs`), so once the peer drops out of range nothing on
//! disk could name who wrote a message, and history rendered it as "Unknown
//! peer". One row per sender: the self-declared nickname from the last message
//! it sent us, and the peer ID that collision suffixes are cut from.

use super::Migration;

pub struct V083MeshSenderNames;

impl Migration for V083MeshSenderNames {
    fn version(&self) -> u32 {
        83
    }

    fn description(&self) -> &'static str {
        "Last announced nickname per mesh sender"
    }

    fn up(&self) -> &'static str {
        r#"
        CREATE TABLE mesh_sender_names (
            sender_id TEXT PRIMARY KEY,
            peer_id TEXT NOT NULL,
            nickname TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE INDEX idx_mesh_sender_names_nickname ON mesh_sender_names(nickname);
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some("DROP TABLE mesh_sender_names;")
    }
}
