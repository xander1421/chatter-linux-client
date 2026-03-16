//! Migration v027: Add peer_display_name to anon_dm_conversations.
//!
//! Adds a column to store the peer's display name for anonymous DMs.
//! This is revealed upon acceptance and needed for UI display.

use super::Migration;

pub struct V027AnonDmPeerDisplayName;

impl Migration for V027AnonDmPeerDisplayName {
    fn version(&self) -> u32 {
        27
    }

    fn description(&self) -> &'static str {
        "Add peer_display_name to anonymous DM conversations"
    }

    fn up(&self) -> &'static str {
        r#"
        ALTER TABLE anon_dm_conversations ADD COLUMN peer_display_name TEXT;
        "#
    }

    fn down(&self) -> Option<&'static str> {
        // SQLite doesn't support DROP COLUMN in older versions,
        // but we can just leave the column if rolling back
        None
    }
}
