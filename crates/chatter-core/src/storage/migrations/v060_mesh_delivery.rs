//! V060: Durable delivery state for messages this device sent over the mesh.
//!
//! `messages.delivery_status` is not usable for this. That column carries server
//! semantics — "sent" there means a server accepted the message — and a mesh
//! message handed to a courier is genuinely sent while no server has ever seen
//! it. `MeshTimeline::record` deliberately stores it empty for that reason.
//!
//! The visible consequence of stopping there was that every mesh delivery state
//! was live-only: a message that was `waiting`, `carried`, or — worst — `not
//! delivered` came back after any reopen looking like an ordinary sent message,
//! because the converter turns an empty status on an own message into "sent".
//! That is the one thing the mesh surfaces must never do: claim an arrival
//! nobody confirmed.
//!
//! `state` holds [`MeshDelivery::as_str`](crate::services::mesh::MeshDelivery::as_str),
//! the mesh's own delivery vocabulary, which is disjoint from the server one on
//! purpose.

use super::Migration;

pub struct V060MeshDelivery;

impl Migration for V060MeshDelivery {
    fn version(&self) -> u32 {
        60
    }

    fn description(&self) -> &'static str {
        "Durable per-message mesh delivery state"
    }

    fn up(&self) -> &'static str {
        r#"
        CREATE TABLE IF NOT EXISTS mesh_message_delivery (
            message_id TEXT PRIMARY KEY NOT NULL,
            state      TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some("DROP TABLE IF EXISTS mesh_message_delivery;")
    }
}
