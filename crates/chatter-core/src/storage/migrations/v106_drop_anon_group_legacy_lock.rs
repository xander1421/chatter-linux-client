//! Drop `anon_group_order.legacy_locked`. Only v094's upgrade backfill ever set
//! it, for groups that predated authenticated admission clocks; a group created
//! since always has ordering, so nothing reads the column any more.
use super::Migration;

pub struct V106DropAnonGroupLegacyLock;
impl Migration for V106DropAnonGroupLegacyLock {
    fn version(&self) -> u32 {
        106
    }
    fn description(&self) -> &'static str {
        "Drop the anonymous-group legacy ordering lock"
    }
    fn up(&self) -> &'static str {
        "ALTER TABLE anon_group_order DROP COLUMN legacy_locked;"
    }
}
