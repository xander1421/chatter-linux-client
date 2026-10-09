//! Durable exact own Saved deletion intent, independent of snapshot recovery.
use super::Migration;

pub struct V096SavedDeleteOutbox;
impl Migration for V096SavedDeleteOutbox {
    fn version(&self) -> u32 {
        96
    }
    fn description(&self) -> &'static str {
        "Durable own Saved deletion outbox"
    }
    fn up(&self) -> &'static str {
        "ALTER TABLE saved_delete_tombstones ADD COLUMN sent_at INTEGER;
        CREATE INDEX saved_delete_tombstones_timestamp ON saved_delete_tombstones(owner_id, sent_at);
        CREATE TABLE saved_delete_outbox (
            owner_id TEXT NOT NULL,
            message_id TEXT NOT NULL,
            sent_at INTEGER NOT NULL CHECK(sent_at > 0),
            generation INTEGER NOT NULL DEFAULT 1,
            attempts INTEGER NOT NULL DEFAULT 0,
            next_attempt_at INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY(owner_id, message_id)
        );
        CREATE INDEX saved_delete_outbox_due ON saved_delete_outbox(owner_id, next_attempt_at, message_id);"
    }
}
