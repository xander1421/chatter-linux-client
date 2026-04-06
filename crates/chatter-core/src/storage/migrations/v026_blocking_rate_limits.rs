//! Migration v026: Blocking and rate limiting for anonymous messaging.
//!
//! Adds:
//! - `status` column to `anon_groups` for group-level blocking/muting
//! - `anon_group_blocked_users` table for per-user blocking within groups
//! - `anon_rate_limits` table for spam prevention

use super::Migration;

pub struct V026BlockingRateLimits;

impl Migration for V026BlockingRateLimits {
    fn version(&self) -> u32 {
        26
    }

    fn description(&self) -> &'static str {
        "Add blocking and rate limiting for anonymous messaging"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Add status column to anon_groups (0=Active, 1=Muted, 2=Blocked)
        ALTER TABLE anon_groups ADD COLUMN status INTEGER NOT NULL DEFAULT 0;

        -- Per-user blocking within anonymous groups
        CREATE TABLE anon_group_blocked_users (
            id TEXT PRIMARY KEY,
            group_id TEXT NOT NULL,
            user_id TEXT NOT NULL,
            blocked_at TEXT NOT NULL,
            reason TEXT,                        -- Optional reason for block
            UNIQUE(group_id, user_id),
            FOREIGN KEY (group_id) REFERENCES anon_groups(id) ON DELETE CASCADE
        );
        CREATE INDEX idx_agbu_group ON anon_group_blocked_users(group_id);
        CREATE INDEX idx_agbu_user ON anon_group_blocked_users(user_id);

        -- Per-user blocking in anonymous DMs (supplements conversation-level blocking)
        CREATE TABLE anon_dm_blocked_users (
            id TEXT PRIMARY KEY,
            user_id TEXT NOT NULL UNIQUE,
            blocked_at TEXT NOT NULL,
            reason TEXT                         -- Optional reason for block
        );
        CREATE INDEX idx_adbu_user ON anon_dm_blocked_users(user_id);

        -- Rate limiting for anonymous messaging
        -- Tracks message counts per entity (conversation/group) per time window
        CREATE TABLE anon_rate_limits (
            id TEXT PRIMARY KEY,
            entity_type TEXT NOT NULL,          -- 'dm' or 'group'
            entity_id TEXT NOT NULL,            -- conversation_id or group_id
            window_start TEXT NOT NULL,         -- Start of current time window
            message_count INTEGER NOT NULL DEFAULT 0,
            last_message_at TEXT NOT NULL,
            UNIQUE(entity_type, entity_id)
        );
        CREATE INDEX idx_arl_entity ON anon_rate_limits(entity_type, entity_id);
        CREATE INDEX idx_arl_window ON anon_rate_limits(window_start);
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
            DROP INDEX IF EXISTS idx_arl_window;
            DROP INDEX IF EXISTS idx_arl_entity;
            DROP TABLE IF EXISTS anon_rate_limits;

            DROP INDEX IF EXISTS idx_adbu_user;
            DROP TABLE IF EXISTS anon_dm_blocked_users;

            DROP INDEX IF EXISTS idx_agbu_user;
            DROP INDEX IF EXISTS idx_agbu_group;
            DROP TABLE IF EXISTS anon_group_blocked_users;

            -- SQLite doesn't support DROP COLUMN in older versions
            -- We recreate the table without the status column
            CREATE TABLE anon_groups_new (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                my_role INTEGER NOT NULL DEFAULT 0,
                bootstrap_complete INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            INSERT INTO anon_groups_new SELECT id, name, my_role, bootstrap_complete, created_at, updated_at FROM anon_groups;
            DROP TABLE anon_groups;
            ALTER TABLE anon_groups_new RENAME TO anon_groups;
            "#,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_migration_version() {
        let migration = V026BlockingRateLimits;
        assert_eq!(migration.version(), 26);
        assert!(migration.description().contains("blocking"));
    }

    #[test]
    fn test_migration_up() {
        let migration = V026BlockingRateLimits;
        let up = migration.up();
        assert!(up.contains("ALTER TABLE anon_groups ADD COLUMN status"));
        assert!(up.contains("CREATE TABLE anon_group_blocked_users"));
        assert!(up.contains("CREATE TABLE anon_dm_blocked_users"));
        assert!(up.contains("CREATE TABLE anon_rate_limits"));
    }

    #[test]
    fn test_migration_down() {
        let migration = V026BlockingRateLimits;
        assert!(migration.down().is_some());
        let down = migration.down().unwrap();
        assert!(down.contains("DROP TABLE IF EXISTS anon_rate_limits"));
        assert!(down.contains("DROP TABLE IF EXISTS anon_dm_blocked_users"));
        assert!(down.contains("DROP TABLE IF EXISTS anon_group_blocked_users"));
    }
}
