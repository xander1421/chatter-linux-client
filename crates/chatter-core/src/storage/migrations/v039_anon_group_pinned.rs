//! Migration v039: Add is_pinned to anon_group_messages.
//!
//! Enables persisting pin state for anonymous group messages so that
//! pinned messages survive channel switches and app restarts.

use super::Migration;

pub struct V039AnonGroupPinned;

impl Migration for V039AnonGroupPinned {
    fn version(&self) -> u32 {
        39
    }

    fn description(&self) -> &'static str {
        "Add is_pinned to anon_group_messages"
    }

    fn up(&self) -> &'static str {
        r#"
        ALTER TABLE anon_group_messages ADD COLUMN is_pinned INTEGER NOT NULL DEFAULT 0;
        "#
    }

    fn down(&self) -> Option<&'static str> {
        None
    }
}
