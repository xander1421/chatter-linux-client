use super::Migration;

pub struct V129PendingDmRetry;

impl Migration for V129PendingDmRetry {
    fn version(&self) -> u32 { 129 }
    fn description(&self) -> &'static str { "Retain failed bootstrap content for manual retry" }
    fn up(&self) -> &'static str {
        "ALTER TABLE pending_dm_messages ADD COLUMN failed INTEGER NOT NULL DEFAULT 0;"
    }
}
