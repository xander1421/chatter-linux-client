//! V008: Settings table migration.
//!
//! Adds table for application settings with key-value storage.

use super::Migration;

/// V008: Create settings table.
pub struct V008Settings;

impl Migration for V008Settings {
    fn version(&self) -> u32 {
        8
    }

    fn description(&self) -> &'static str {
        "Create settings table"
    }

    fn up(&self) -> &'static str {
        r#"
        CREATE TABLE settings (
            key TEXT PRIMARY KEY,
            value_json TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some("DROP TABLE IF EXISTS settings;")
    }
}
