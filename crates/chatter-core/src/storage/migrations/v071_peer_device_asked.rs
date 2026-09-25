//! V071: When each peer device was last asked to answer the Shield bootstrap.
//!
//! "Already owed a copy" stood in for "already asked", so an ask that never
//! left (empty token batch) was not repeated until the owed copies aged out.
//! A row here is written only when a bootstrap MESSAGE or REQUEST actually
//! reached the device, and removed when the device answers.

use super::Migration;

pub struct V071PeerDeviceAsked;

impl Migration for V071PeerDeviceAsked {
    fn version(&self) -> u32 {
        71
    }

    fn description(&self) -> &'static str {
        "Record when a peer device was asked to bootstrap"
    }

    fn up(&self) -> &'static str {
        r#"
        CREATE TABLE dm_device_asked (
            conversation_id TEXT NOT NULL,
            peer_device_id INTEGER NOT NULL,
            asked_at TEXT NOT NULL,
            PRIMARY KEY (conversation_id, peer_device_id)
        );
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some("DROP TABLE IF EXISTS dm_device_asked;")
    }
}
