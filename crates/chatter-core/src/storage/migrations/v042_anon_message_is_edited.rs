//! Migration v042: Add is_edited column to anon_dm_messages and anon_group_messages.
//!
//! The "edited" indicator was only an in-memory flag from real-time events.
//! This migration persists it so the flag survives navigation / app restart.

use super::Migration;

pub struct V042AnonMessageIsEdited;

impl Migration for V042AnonMessageIsEdited {
    fn version(&self) -> u32 {
        42
    }

    fn description(&self) -> &'static str {
        "Add is_edited column to anon_dm_messages and anon_group_messages"
    }

    fn up(&self) -> &'static str {
        r#"
        ALTER TABLE anon_dm_messages ADD COLUMN is_edited INTEGER NOT NULL DEFAULT 0;
        ALTER TABLE anon_group_messages ADD COLUMN is_edited INTEGER NOT NULL DEFAULT 0;
        "#
    }

    fn down(&self) -> Option<&'static str> {
        None
    }
}
