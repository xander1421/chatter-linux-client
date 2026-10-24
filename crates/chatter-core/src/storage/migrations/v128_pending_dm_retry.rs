use super::Migration;

pub struct V128PendingDmRetry;

impl Migration for V128PendingDmRetry {
    fn version(&self) -> u32 { 128 }
    fn description(&self) -> &'static str { "Retain failed bootstrap content for manual retry" }
    fn up(&self) -> &'static str {
        "ALTER TABLE pending_dm_messages ADD COLUMN failed INTEGER NOT NULL DEFAULT 0;"
    }
}
