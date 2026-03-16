//! Anonymous group messages table migration.
//!
//! Creates a table for storing messages in anonymous groups.
//! Separate from the main messages table since anon groups
//! don't have corresponding channel entries.

use super::Migration;

/// Anonymous group messages table migration (v021).
pub struct V021AnonGroupMessages;

impl Migration for V021AnonGroupMessages {
    fn version(&self) -> u32 {
        21
    }

    fn description(&self) -> &'static str {
        "Anonymous group messages storage"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Anonymous group messages (separate from main messages table)
        CREATE TABLE anon_group_messages (
            id TEXT PRIMARY KEY,
            group_id BLOB NOT NULL,
            sender_id BLOB NOT NULL,           -- 16 bytes UUID
            content_type INTEGER NOT NULL,     -- Discriminant for filtering
            content_json TEXT NOT NULL,        -- Serialized message content
            attachments_blob BLOB NOT NULL DEFAULT X'0800000000000000',
            is_deleted INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL,
            FOREIGN KEY (group_id) REFERENCES anon_groups(id) ON DELETE CASCADE
        );
        CREATE INDEX idx_agmsg_group ON anon_group_messages(group_id);
        CREATE INDEX idx_agmsg_created ON anon_group_messages(group_id, created_at DESC);
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
            DROP INDEX IF EXISTS idx_agmsg_created;
            DROP INDEX IF EXISTS idx_agmsg_group;
            DROP TABLE IF EXISTS anon_group_messages;
            "#,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_migration_version() {
        let migration = V021AnonGroupMessages;
        assert_eq!(migration.version(), 21);
    }

    #[test]
    fn test_migration_up_contains_table() {
        let migration = V021AnonGroupMessages;
        let up = migration.up();
        assert!(up.contains("CREATE TABLE anon_group_messages"));
        assert!(up.contains("FOREIGN KEY (group_id)"));
    }
}
