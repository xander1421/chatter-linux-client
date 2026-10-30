use super::Migration;

pub struct V126ActivityOutageResolution;

impl Migration for V126ActivityOutageResolution {
    fn version(&self) -> u32 { 126 }
    fn description(&self) -> &'static str { "Keep a resolved outage so a reconnect flap reopens it" }
    fn up(&self) -> &'static str {
        "ALTER TABLE activity_outages ADD COLUMN resolved_at_ms INTEGER;"
    }
}
