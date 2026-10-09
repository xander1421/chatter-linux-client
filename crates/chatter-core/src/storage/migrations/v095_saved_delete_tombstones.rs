//! Exact own Saved deletions may arrive before the snapshot.
use super::Migration;

pub struct V095SavedDeleteTombstones;

impl Migration for V095SavedDeleteTombstones {
    fn version(&self) -> u32 {
        95
    }
    fn description(&self) -> &'static str {
        "Scoped own Saved deletion tombstones"
    }
    fn up(&self) -> &'static str {
        "CREATE TABLE saved_delete_tombstones (
            owner_id TEXT NOT NULL,
            message_id TEXT NOT NULL,
            PRIMARY KEY (owner_id, message_id)
        );"
    }
    fn down(&self) -> Option<&'static str> {
        Some("DROP TABLE IF EXISTS saved_delete_tombstones;")
    }
}
