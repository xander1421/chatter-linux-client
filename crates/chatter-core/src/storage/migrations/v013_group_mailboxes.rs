//! Group mailbox tables migration.
//!
//! Creates tables for group mailbox state management.

use super::Migration;

/// Group mailbox tables migration (v013).
///
/// Creates the group mailbox-related tables:
/// - `group_mailboxes`: Our mailbox for each group we're in
/// - `group_member_mailboxes`: Peer mailboxes for group members
pub struct V013GroupMailboxes;

impl Migration for V013GroupMailboxes {
    fn version(&self) -> u32 {
        13
    }

    fn description(&self) -> &'static str {
        "Group mailbox tables - our mailboxes and member mailboxes"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Our mailbox for each group we're in
        CREATE TABLE group_mailboxes (
            id TEXT PRIMARY KEY,
            group_id TEXT NOT NULL,
            mailbox_public_key BLOB NOT NULL,
            mailbox_private_key BLOB NOT NULL,
            signature BLOB NOT NULL,
            randomizer BLOB,
            token_id TEXT NOT NULL,
            created_at TEXT NOT NULL,
            expires_at TEXT NOT NULL,
            last_polled_at TEXT,
            FOREIGN KEY (token_id) REFERENCES mailbox_tokens(id)
        );
        CREATE INDEX idx_group_mailboxes_group ON group_mailboxes(group_id);
        CREATE INDEX idx_group_mailboxes_expires ON group_mailboxes(expires_at);

        -- Peer mailboxes for group members
        CREATE TABLE group_member_mailboxes (
            id TEXT PRIMARY KEY,
            group_id TEXT NOT NULL,
            user_id TEXT NOT NULL,
            mailbox_public_key BLOB NOT NULL,
            received_at TEXT NOT NULL,
            expires_at TEXT,
            UNIQUE(group_id, user_id)
        );
        CREATE INDEX idx_group_member_mailboxes_group ON group_member_mailboxes(group_id);
        CREATE INDEX idx_group_member_mailboxes_user ON group_member_mailboxes(user_id);
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
            DROP INDEX IF EXISTS idx_group_member_mailboxes_user;
            DROP INDEX IF EXISTS idx_group_member_mailboxes_group;
            DROP TABLE IF EXISTS group_member_mailboxes;
            DROP INDEX IF EXISTS idx_group_mailboxes_expires;
            DROP INDEX IF EXISTS idx_group_mailboxes_group;
            DROP TABLE IF EXISTS group_mailboxes;
            "#,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_migration_version() {
        let migration = V013GroupMailboxes;
        assert_eq!(migration.version(), 13);
    }

    #[test]
    fn test_migration_description() {
        let migration = V013GroupMailboxes;
        assert!(migration.description().contains("Group mailbox"));
    }

    #[test]
    fn test_migration_up_contains_tables() {
        let migration = V013GroupMailboxes;
        let up = migration.up();
        assert!(up.contains("CREATE TABLE group_mailboxes"));
        assert!(up.contains("CREATE TABLE group_member_mailboxes"));
        assert!(up.contains("UNIQUE(group_id, user_id)"));
    }

    #[test]
    fn test_migration_down() {
        let migration = V013GroupMailboxes;
        assert!(migration.down().is_some());
        let down = migration.down().unwrap();
        assert!(down.contains("DROP TABLE IF EXISTS group_mailboxes"));
        assert!(down.contains("DROP TABLE IF EXISTS group_member_mailboxes"));
    }
}
