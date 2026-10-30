use super::Migration;

pub struct V140ActivityActorNames;

impl Migration for V140ActivityActorNames {
    fn version(&self) -> u32 { 140 }
    fn description(&self) -> &'static str { "Resolve activity actor names when reading the feed" }
    fn up(&self) -> &'static str {
        "DROP TABLE activity_items;
         DROP TABLE activity_outages;
         CREATE TABLE activity_items (
             owner_id TEXT NOT NULL,
             id TEXT NOT NULL,
             kind TEXT NOT NULL,
             actor_id TEXT,
             actor_name TEXT NOT NULL DEFAULT '',
             detail TEXT NOT NULL DEFAULT '',
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
             outage_id TEXT NOT NULL,
             resolved_at_ms INTEGER
         );"
    }
}
