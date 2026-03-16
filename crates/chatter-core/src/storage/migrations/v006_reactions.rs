//! V006: Reactions table migration.
//!
//! Adds table for message reactions with emoji data.

use super::Migration;

/// V006: Create reactions table.
pub struct V006Reactions;

impl Migration for V006Reactions {
    fn version(&self) -> u32 {
        6
    }

    fn description(&self) -> &'static str {
        "Create reactions table"
    }

    fn up(&self) -> &'static str {
        r#"
        CREATE TABLE reactions (
            id TEXT PRIMARY KEY,
            message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
            user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            -- Emoji stored as JSON (has optional custom_name, custom_url)
            emoji_json TEXT NOT NULL,
            created_at TEXT NOT NULL,
            -- Unique constraint: one reaction per user per message per emoji value
            UNIQUE(message_id, user_id, emoji_json)
        );

        -- Index for fetching all reactions on a message
        CREATE INDEX idx_reactions_message ON reactions(message_id);

        -- Index for fetching user's reactions (for "you reacted" display)
        CREATE INDEX idx_reactions_user ON reactions(user_id);
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some("DROP TABLE IF EXISTS reactions;")
    }
}
