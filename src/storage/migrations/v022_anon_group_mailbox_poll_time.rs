//! Add last_polled_at column to anon_group_mailboxes.
//!
//! Enables poll time tracking for anonymous group mailboxes,
//! matching the functionality of server-managed group_mailboxes.

use super::Migration;

/// Add last_polled_at to anon_group_mailboxes migration (v022).
pub struct V022AnonGroupMailboxPollTime;

impl Migration for V022AnonGroupMailboxPollTime {
    fn version(&self) -> u32 {
        22
    }

    fn description(&self) -> &'static str {
        "Add last_polled_at to anon_group_mailboxes"
    }

    fn up(&self) -> &'static str {
        r#"
        ALTER TABLE anon_group_mailboxes ADD COLUMN last_polled_at TEXT;
        "#
    }

    fn down(&self) -> Option<&'static str> {
        // SQLite doesn't support DROP COLUMN in older versions,
        // but we can leave this as None since it's a non-destructive addition
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_migration_version() {
        let migration = V022AnonGroupMailboxPollTime;
        assert_eq!(migration.version(), 22);
    }

    #[test]
    fn test_migration_description() {
        let migration = V022AnonGroupMailboxPollTime;
        assert!(migration.description().contains("last_polled_at"));
    }
}
