//! Initial database schema migration.
//!
//! Creates foundation tables for settings and sync state.

use super::Migration;

/// Initial schema migration (v001).
///
/// Creates the `settings` and `sync_state` tables that form the
/// foundation for app configuration and synchronization tracking.
pub struct V001Initial;

impl Migration for V001Initial {
    fn version(&self) -> u32 {
        1
    }

    fn description(&self) -> &'static str {
        "Initial schema - settings and sync_state tables"
    }

    fn up(&self) -> &'static str {
        r#"
        -- App configuration settings
        CREATE TABLE settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        -- Sync state tracking for each entity type
        CREATE TABLE sync_state (
            entity_type TEXT PRIMARY KEY,
            last_sync_at TEXT,
            last_sync_id TEXT,
            sync_token TEXT
        );
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
            DROP TABLE IF EXISTS sync_state;
            DROP TABLE IF EXISTS settings;
            "#,
        )
    }
}
