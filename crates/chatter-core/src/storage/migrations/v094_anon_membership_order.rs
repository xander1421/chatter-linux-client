//! Authenticated anonymous-group structural ordering and durable removals.
use super::Migration;

pub struct V094AnonMembershipOrder;
impl Migration for V094AnonMembershipOrder {
    fn version(&self) -> u32 {
        94
    }
    fn description(&self) -> &'static str {
        "Anonymous group admission clocks and removal tombstones"
    }
    fn up(&self) -> &'static str {
        // Deliberately no group FK: deleting/reaccepting a group must not erase
        // authenticated removal history. Legacy joined_at is a local receipt
        // time, not authenticated evidence of an explicit invitation.
        "CREATE TABLE anon_group_member_order (
            group_id TEXT NOT NULL,
            user_id TEXT NOT NULL,
            admitted_at INTEGER NOT NULL DEFAULT 0,
            removed_at INTEGER NOT NULL DEFAULT 0,
            changed_at INTEGER NOT NULL DEFAULT 0,
            acknowledged_at INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY(group_id, user_id)
        );
        CREATE TABLE anon_group_order (
            group_id TEXT PRIMARY KEY,
            roster_at INTEGER NOT NULL DEFAULT 0,
            update_at INTEGER NOT NULL DEFAULT 0,
            legacy_locked INTEGER NOT NULL DEFAULT 0
        );
        -- There is no authenticated history for already deleted legacy rows.
        -- Retain today's members, but never reconstruct old membership from
        -- an unversioned legacy snapshot received after this upgrade.
        INSERT INTO anon_group_order(group_id,legacy_locked) SELECT id,1 FROM anon_groups;"
    }
}
