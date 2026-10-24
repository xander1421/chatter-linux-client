use super::Migration;

pub struct V131MeshArchivePackets;

impl Migration for V131MeshArchivePackets {
    fn version(&self) -> u32 { 131 }
    fn description(&self) -> &'static str { "Keyed durable public mesh packets" }
    fn up(&self) -> &'static str {
        r#"
        DELETE FROM mesh_store WHERE store_key = 'public-archive';
        CREATE TABLE mesh_archive_packets (
            packet_id BLOB PRIMARY KEY NOT NULL CHECK(length(packet_id) = 16),
            packet BLOB NOT NULL,
            inserted_order INTEGER NOT NULL UNIQUE
        );
        "#
    }
}
