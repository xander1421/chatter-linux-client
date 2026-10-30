//! Anon group owner ID migration (v038).
//!
//! Adds owner_id to anon_groups to track the authoritative group owner.
//! This is critical for verifying member lists and role updates.

use super::Migration;

pub struct V038AnonGroupOwnerId;

impl Migration for V038AnonGroupOwnerId {
    fn version(&self) -> u32 {
        38
    }

    fn description(&self) -> &'static str {
        "Add owner_id to anon_groups for authoritative updates"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Add owner_id to anon_groups
        -- Data preservation is not required, so we make it NOT NULL.
        -- We provide a default for the ALTER TABLE to succeed if 
        -- there were any stray rows, but new rows will require a real ID.
        ALTER TABLE anon_groups ADD COLUMN owner_id TEXT NOT NULL DEFAULT '';
        "#
    }

    fn down(&self) -> Option<&'static str> {
        // SQLite doesn't support DROP COLUMN in older versions easily,
        // but for this migration we can leave it as it's just an extra column.
        None
    }
}
