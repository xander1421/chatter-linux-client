//! Migration v033: Anonymous DM reactions and delivery status.
//!
//! Adds support for message reactions and read receipts in anonymous DMs.

use super::Migration;

pub struct V033AnonDmReactionsReceipts;

impl Migration for V033AnonDmReactionsReceipts {
    fn version(&self) -> u32 {
        33
    }

    fn description(&self) -> &'static str {
        "Anonymous message reactions and delivery status (relational)"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Add delivery_status column to anon_dm_messages and anon_group_messages
        ALTER TABLE anon_dm_messages ADD COLUMN delivery_status TEXT NOT NULL DEFAULT '';
        ALTER TABLE anon_group_messages ADD COLUMN delivery_status TEXT NOT NULL DEFAULT '';

        -- Anonymous DM reactions table
        CREATE TABLE IF NOT EXISTS anon_dm_reactions (
            id TEXT PRIMARY KEY,
            message_id TEXT NOT NULL,
            user_id TEXT NOT NULL,
            emoji TEXT NOT NULL,
            created_at TEXT NOT NULL,
            UNIQUE(message_id, user_id, emoji),
            FOREIGN KEY (message_id) REFERENCES anon_dm_messages(id) ON DELETE CASCADE
        );
        CREATE INDEX idx_anon_dm_reactions_message ON anon_dm_reactions(message_id);

        -- Anonymous group reactions table
        CREATE TABLE IF NOT EXISTS anon_group_reactions (
            id TEXT PRIMARY KEY,
            message_id TEXT NOT NULL,
            user_id TEXT NOT NULL,
            emoji TEXT NOT NULL,
            created_at TEXT NOT NULL,
            UNIQUE(message_id, user_id, emoji),
            FOREIGN KEY (message_id) REFERENCES anon_group_messages(id) ON DELETE CASCADE
        );
        CREATE INDEX idx_anon_group_reactions_message ON anon_group_reactions(message_id);
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(r#"
        DROP TABLE IF EXISTS anon_dm_reactions;
        DROP TABLE IF EXISTS anon_group_reactions;
        "#)
    }
}
