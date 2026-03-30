//! Migration: custom sidebar sections.

use super::Migration;

pub struct V048CustomSections;

impl Migration for V048CustomSections {
    fn version(&self) -> u32 {
        48
    }

    fn description(&self) -> &'static str {
        "Add custom_sections and section_memberships tables for sidebar organization"
    }

    fn up(&self) -> &'static str {
        r#"
        CREATE TABLE custom_sections (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            icon_emoji TEXT NOT NULL DEFAULT '📁',
            sort_order INTEGER NOT NULL DEFAULT 0,
            is_expanded INTEGER NOT NULL DEFAULT 1,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE section_memberships (
            section_id TEXT NOT NULL REFERENCES custom_sections(id) ON DELETE CASCADE,
            conversation_id TEXT NOT NULL,
            conversation_type TEXT NOT NULL,
            added_at TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (section_id, conversation_id)
        );
        "#
    }
}
