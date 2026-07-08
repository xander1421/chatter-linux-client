//! Anonymous groups tables migration.
//!
//! Creates tables for NDSS'21 style anonymous groups where the server
//! has no knowledge of group existence, membership, or messages.

use super::Migration;

/// Anonymous groups tables migration (v020).
///
/// Creates the anonymous group-related tables:
/// - `anon_groups`: Our local view of groups we're in
/// - `anon_group_members`: Members of each group with their mailbox keys
/// - `anon_group_mailboxes`: Our mailbox keypair for each group
/// - `anon_group_pending`: Pending invites/actions awaiting response
pub struct V020AnonGroups;

impl Migration for V020AnonGroups {
    fn version(&self) -> u32 {
        20
    }

    fn description(&self) -> &'static str {
        "Anonymous groups - NDSS'21 style server-oblivious groups"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Anonymous groups (local-only, server has NO knowledge)
        CREATE TABLE anon_groups (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            my_role INTEGER NOT NULL DEFAULT 0,  -- 0=Member, 1=Admin, 2=Owner
            bootstrap_complete INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        -- Group members (our local view of membership)
        CREATE TABLE anon_group_members (
            id TEXT PRIMARY KEY,
            group_id TEXT NOT NULL,
            user_id TEXT NOT NULL,
            role INTEGER NOT NULL DEFAULT 0,  -- 0=Member, 1=Admin, 2=Owner
            mailbox_pk BLOB,                  -- 32 bytes Ed25519, NULL until JoinAck received
            joined_at TEXT NOT NULL,
            UNIQUE(group_id, user_id),
            FOREIGN KEY (group_id) REFERENCES anon_groups(id) ON DELETE CASCADE
        );
        CREATE INDEX idx_agm_group ON anon_group_members(group_id);
        CREATE INDEX idx_agm_user ON anon_group_members(user_id);

        -- Our mailbox for each group (reuses blind signature infrastructure)
        CREATE TABLE anon_group_mailboxes (
            id TEXT PRIMARY KEY,
            group_id TEXT NOT NULL UNIQUE,
            mailbox_public_key BLOB NOT NULL,  -- 32 bytes Ed25519
            mailbox_private_key BLOB NOT NULL, -- 64 bytes Ed25519 keypair
            token_id TEXT NOT NULL,
            created_at TEXT NOT NULL,
            expires_at TEXT NOT NULL,
            FOREIGN KEY (group_id) REFERENCES anon_groups(id) ON DELETE CASCADE
        );

        -- Pending actions (invites sent, waiting for ack)
        CREATE TABLE anon_group_pending (
            id TEXT PRIMARY KEY,
            group_id TEXT NOT NULL,
            user_id TEXT NOT NULL,
            action TEXT NOT NULL,              -- 'invite', 'kick'
            created_at TEXT NOT NULL,
            expires_at TEXT NOT NULL,
            retry_count INTEGER DEFAULT 0,
            UNIQUE(group_id, user_id, action),
            FOREIGN KEY (group_id) REFERENCES anon_groups(id) ON DELETE CASCADE
        );
        CREATE INDEX idx_agp_group ON anon_group_pending(group_id);
        CREATE INDEX idx_agp_expires ON anon_group_pending(expires_at);
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
            DROP INDEX IF EXISTS idx_agp_expires;
            DROP INDEX IF EXISTS idx_agp_group;
            DROP TABLE IF EXISTS anon_group_pending;
            DROP TABLE IF EXISTS anon_group_mailboxes;
            DROP INDEX IF EXISTS idx_agm_user;
            DROP INDEX IF EXISTS idx_agm_group;
            DROP TABLE IF EXISTS anon_group_members;
            DROP TABLE IF EXISTS anon_groups;
            "#,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_migration_version() {
        let migration = V020AnonGroups;
        assert_eq!(migration.version(), 20);
    }

    #[test]
    fn test_migration_description() {
        let migration = V020AnonGroups;
        assert!(migration.description().contains("Anonymous groups"));
    }

    #[test]
    fn test_migration_up_contains_tables() {
        let migration = V020AnonGroups;
        let up = migration.up();
        assert!(up.contains("CREATE TABLE anon_groups"));
        assert!(up.contains("CREATE TABLE anon_group_members"));
        assert!(up.contains("CREATE TABLE anon_group_mailboxes"));
        assert!(up.contains("CREATE TABLE anon_group_pending"));
        assert!(up.contains("UNIQUE(group_id, user_id)"));

        // Anon-group mailboxes are single-row-per-group (group_id UNIQUE): a
        // refresh does INSERT OR REPLACE, so the old row never lingers in the
        // DB. This is why the DM/group re-rotation guard (which fixes an
        // old-row-re-selected-every-cycle token drain caused by TWO coexisting
        // in-window rows) is NOT needed on refresh_expiring_group_mailboxes.
        // If this UNIQUE is ever dropped for a multi-row dual-poll overlap
        // design, that guard must be added — see lifecycle.rs refresh path.
        assert!(up.contains("group_id TEXT NOT NULL UNIQUE"));
    }

    #[test]
    fn test_migration_down() {
        let migration = V020AnonGroups;
        assert!(migration.down().is_some());
        let down = migration.down().unwrap();
        assert!(down.contains("DROP TABLE IF EXISTS anon_groups"));
        assert!(down.contains("DROP TABLE IF EXISTS anon_group_members"));
        assert!(down.contains("DROP TABLE IF EXISTS anon_group_mailboxes"));
        assert!(down.contains("DROP TABLE IF EXISTS anon_group_pending"));
    }
}
