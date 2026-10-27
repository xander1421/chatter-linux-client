use super::Migration;

pub struct V141UserBlockIntervals;

impl Migration for V141UserBlockIntervals {
    fn version(&self) -> u32 { 141 }
    fn description(&self) -> &'static str { "Record local user block intervals" }
    fn up(&self) -> &'static str {
        "CREATE TABLE user_block_intervals (
            owner_id TEXT NOT NULL,
            peer_id TEXT NOT NULL,
            started_at_ns INTEGER NOT NULL,
            ended_at_ns INTEGER,
            PRIMARY KEY (owner_id, peer_id, started_at_ns)
        );
        CREATE INDEX user_block_intervals_lookup
            ON user_block_intervals(owner_id, peer_id, started_at_ns, ended_at_ns);"
    }
}
