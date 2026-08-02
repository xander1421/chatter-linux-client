//! Migration: per-section sort mode.
//!
//! Sections had no ordering preference at all — `get_section_conversations`
//! returned `ORDER BY added_at ASC`, so filing a conversation into a folder
//! silently swapped it from MRU to filing order. `sort_mode` makes the order a
//! choice, defaulting to `recent` so a folder matches the list it came from.

use super::Migration;

pub struct V062SectionSortMode;

impl Migration for V062SectionSortMode {
    fn version(&self) -> u32 {
        62
    }

    fn description(&self) -> &'static str {
        "Add sort_mode to custom_sections (recent | unread | alphabetical)"
    }

    fn up(&self) -> &'static str {
        r#"
        ALTER TABLE custom_sections ADD COLUMN sort_mode TEXT NOT NULL DEFAULT 'recent';
        "#
    }
}
