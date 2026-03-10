//! Migration v035: Add delivery_status to messages table.
//!
//! Adds read receipt tracking for channel messages (same as anon_dm/anon_group).

use super::Migration;

pub struct V035MessageDeliveryStatus;

impl Migration for V035MessageDeliveryStatus {
    fn version(&self) -> u32 {
        35
    }

    fn description(&self) -> &'static str {
        "Add delivery_status column to messages table"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Add delivery_status column to messages table
        -- Values: '', 'sent', 'delivered', 'read'
        ALTER TABLE messages ADD COLUMN delivery_status TEXT NOT NULL DEFAULT '';
        "#
    }

    fn down(&self) -> Option<&'static str> {
        // SQLite doesn't support DROP COLUMN, so no down migration
        None
    }
}
