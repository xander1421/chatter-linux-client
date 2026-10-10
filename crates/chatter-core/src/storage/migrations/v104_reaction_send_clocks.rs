//! The reaction send-clock table, created by a migration like every other
//! table. It used to be created on first use inside the allocator
//! (`reaction_clock.rs`), which the storage SQL contract cannot see.
//! Local allocator state: deliberately retained across deletion of targets,
//! never a projection, never reset by snapshot/import.
use super::Migration;

pub struct V104ReactionSendClocks;
impl Migration for V104ReactionSendClocks {
    fn version(&self) -> u32 {
        104
    }
    fn description(&self) -> &'static str {
        "Reaction send-clock allocator table"
    }
    fn up(&self) -> &'static str {
        "CREATE TABLE IF NOT EXISTS reaction_send_clocks(
            actor_id TEXT PRIMARY KEY,
            action_ts INTEGER NOT NULL CHECK(action_ts>0)
        );"
    }
}
