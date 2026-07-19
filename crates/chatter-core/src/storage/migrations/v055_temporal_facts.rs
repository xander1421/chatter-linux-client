//! V055: Temporal validity for Jarvis facts (Zep-style).
//!
//! A fact stops being flat "current truth": contradictions supersede the old
//! fact instead of overwriting it, preserving *when* something was true.
//! `valid_until IS NULL` = currently true; a superseded fact keeps its row
//! with `valid_until` set and `superseded_by` pointing at the replacement.

use super::Migration;

pub struct V055TemporalFacts;

impl Migration for V055TemporalFacts {
    fn version(&self) -> u32 {
        55
    }

    fn description(&self) -> &'static str {
        "Add temporal validity (valid_from/valid_until/superseded_by) to jarvis_facts"
    }

    fn up(&self) -> &'static str {
        r#"
        ALTER TABLE jarvis_facts ADD COLUMN valid_from TEXT;
        ALTER TABLE jarvis_facts ADD COLUMN valid_until TEXT;
        ALTER TABLE jarvis_facts ADD COLUMN superseded_by TEXT;

        -- Current-facts lookups filter on valid_until IS NULL
        CREATE INDEX idx_jarvis_facts_current
            ON jarvis_facts(subject_type, subject_id)
            WHERE valid_until IS NULL;
        "#
    }
}
