//! V065: bring the Sesame tables into the migration system.
//!
//! `SqliteSesameStore::init_tables` created `sesame_users`, `sesame_devices`,
//! `sesame_sessions` and `sesame_pending_messages` with runtime
//! `CREATE TABLE IF NOT EXISTS` — the only four tables in the client that lived
//! outside `storage/migrations/`. Three problems, all of which this fixes by
//! moving the DDL here unchanged:
//!
//! 1. No version row. Nothing recorded that these tables existed, so nothing
//!    could ever alter them: a column added to `sesame_sessions` in a later
//!    build would need its own ad-hoc `ALTER` guarded by an ad-hoc probe.
//!
//! 2. `IF NOT EXISTS` silently accepts a divergent shape. A database written by
//!    an older build keeps that build's columns, the newer `CREATE` is a no-op,
//!    and every statement written against the new shape fails at runtime with no
//!    indication of why.
//!
//! 3. `sesame_sessions.session_record` holds serialized Double Ratchet state.
//!    Losing or diverging on that table is losing sessions, not losing a cache.
//!
//! The DDL is a verbatim move. `app/infrastructure.rs` no longer calls
//! `init_tables` (and no longer swallows its failure into a `warn!`); these
//! tables now come up with every other one, before anything can use them.

use super::Migration;

pub struct V065SesameTables;

impl Migration for V065SesameTables {
    fn version(&self) -> u32 {
        65
    }

    fn description(&self) -> &'static str {
        "Move the Sesame session tables into the migration system"
    }

    fn up(&self) -> &'static str {
        r#"
        CREATE TABLE IF NOT EXISTS sesame_users (
            user_id TEXT PRIMARY KEY,
            stale INTEGER NOT NULL DEFAULT 0,
            last_directory_check TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS sesame_devices (
            user_id TEXT NOT NULL,
            device_id INTEGER NOT NULL,
            stale INTEGER NOT NULL DEFAULT 0,
            stale_timestamp TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (user_id, device_id),
            FOREIGN KEY (user_id) REFERENCES sesame_users(user_id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS sesame_sessions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id TEXT NOT NULL,
            device_id INTEGER NOT NULL,
            session_id BLOB NOT NULL,
            session_record BLOB NOT NULL,
            is_active INTEGER NOT NULL DEFAULT 0,
            position INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL,
            last_send_at TEXT,
            last_recv_at TEXT,
            send_count INTEGER NOT NULL DEFAULT 0,
            recv_count INTEGER NOT NULL DEFAULT 0,
            FOREIGN KEY (user_id, device_id) REFERENCES sesame_devices(user_id, device_id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_sesame_sessions_user_device
            ON sesame_sessions(user_id, device_id, is_active);

        CREATE TABLE IF NOT EXISTS sesame_pending_messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id TEXT NOT NULL,
            content BLOB NOT NULL,
            timestamp TEXT NOT NULL,
            attempts INTEGER NOT NULL DEFAULT 0,
            attachment_ids_blob BLOB,
            channel_id TEXT,
            FOREIGN KEY (user_id) REFERENCES sesame_users(user_id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_sesame_pending_user
            ON sesame_pending_messages(user_id);
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
            DROP INDEX IF EXISTS idx_sesame_pending_user;
            DROP TABLE IF EXISTS sesame_pending_messages;
            DROP INDEX IF EXISTS idx_sesame_sessions_user_device;
            DROP TABLE IF EXISTS sesame_sessions;
            DROP TABLE IF EXISTS sesame_devices;
            DROP TABLE IF EXISTS sesame_users;
            "#,
        )
    }
}
