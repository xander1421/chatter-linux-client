//! Migration v040: Add missing fields for DM/Group message parity.
//!
//! Adds `is_pinned` to `anon_dm_messages` (Groups already have it since v039)
//! and `is_read` to `anon_group_messages` (DMs already have it since v025).

use super::Migration;

pub struct V040AnonMessageParity;

impl Migration for V040AnonMessageParity {
    fn version(&self) -> u32 {
        40
    }

    fn description(&self) -> &'static str {
        "Add is_pinned to anon_dm_messages and is_read to anon_group_messages"
    }

    fn up(&self) -> &'static str {
        r#"
        ALTER TABLE anon_dm_messages ADD COLUMN is_pinned INTEGER NOT NULL DEFAULT 0;
        ALTER TABLE anon_group_messages ADD COLUMN is_read INTEGER NOT NULL DEFAULT 0;
        CREATE INDEX IF NOT EXISTS idx_anon_dm_messages_pinned ON anon_dm_messages(conversation_id) WHERE is_pinned = 1;
        "#
    }

    fn down(&self) -> Option<&'static str> {
        None
    }
}
