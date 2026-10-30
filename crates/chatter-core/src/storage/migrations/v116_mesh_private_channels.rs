//! Device-local private mesh channel state. SQLCipher protects the stored epoch key.
use super::Migration;

pub struct V116MeshPrivateChannels;

impl Migration for V116MeshPrivateChannels {
    fn version(&self) -> u32 {
        116
    }
    fn description(&self) -> &'static str {
        "Private mesh channels and QR admissions"
    }
    fn up(&self) -> &'static str {
        "CREATE TABLE mesh_private_channels (
            channel_id TEXT PRIMARY KEY,
            state BLOB NOT NULL
        );
        CREATE TABLE mesh_private_joins (
            channel_id TEXT PRIMARY KEY,
            invite BLOB NOT NULL
        );"
    }
}
