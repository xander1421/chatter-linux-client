use super::Migration;

pub struct V124ActivityCenter;

impl Migration for V124ActivityCenter {
    fn version(&self) -> u32 { 124 }
    fn description(&self) -> &'static str { "Persist account-scoped activity and outage state" }
    fn up(&self) -> &'static str {
        "CREATE TABLE activity_items (
            owner_id TEXT NOT NULL,
            id TEXT NOT NULL,
            kind TEXT NOT NULL,
            title TEXT NOT NULL,
            preview TEXT NOT NULL DEFAULT '',
            conversation_id TEXT,
            conversation_type TEXT,
            message_id TEXT,
            created_at_ms INTEGER NOT NULL,
            unread INTEGER NOT NULL DEFAULT 1,
            PRIMARY KEY (owner_id, id)
        );
        CREATE INDEX activity_items_recent ON activity_items(owner_id, created_at_ms DESC);
        CREATE TABLE activity_outages (
            owner_id TEXT PRIMARY KEY,
            outage_id TEXT NOT NULL
        );"
    }
}
