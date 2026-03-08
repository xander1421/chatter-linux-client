//! Migration v031: Jarvis AI memory storage.
//!
//! Creates tables for:
//! - jarvis_conversations: Conversation history with context
//! - jarvis_facts: Extracted facts with confidence decay
//! - jarvis_summaries: Compressed conversation summaries

use super::Migration;

pub struct V031JarvisMemory;

impl Migration for V031JarvisMemory {
    fn version(&self) -> u32 {
        31
    }

    fn description(&self) -> &'static str {
        "Create Jarvis AI memory tables"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Conversation memories with context association
        CREATE TABLE jarvis_conversations (
            id TEXT PRIMARY KEY,
            context_type INTEGER NOT NULL,  -- 0=channel, 1=dm, 2=anon_dm, 3=anon_group
            context_id TEXT NOT NULL,
            role INTEGER NOT NULL,          -- 0=user, 1=assistant, 2=system
            content TEXT NOT NULL,
            timestamp INTEGER NOT NULL,
            metadata_json TEXT,
            created_at TEXT NOT NULL
        );

        -- Index for context lookups (most common query pattern)
        CREATE INDEX idx_jarvis_conversations_context
            ON jarvis_conversations(context_type, context_id, timestamp DESC);

        -- Extracted facts with confidence scoring and decay
        CREATE TABLE jarvis_facts (
            id TEXT PRIMARY KEY,
            subject_type INTEGER NOT NULL,  -- 0=user, 1=topic, 2=project, 3=preference
            subject_id TEXT NOT NULL,
            predicate TEXT NOT NULL,
            object TEXT NOT NULL,
            confidence REAL DEFAULT 1.0,
            source_context_id TEXT,
            last_reinforced_at TEXT NOT NULL,
            decay_rate REAL DEFAULT 0.01,
            created_at TEXT NOT NULL
        );

        -- Index for subject lookups
        CREATE INDEX idx_jarvis_facts_subject ON jarvis_facts(subject_type, subject_id);

        -- Index for finding facts by predicate (e.g., all "works_on" facts)
        CREATE INDEX idx_jarvis_facts_predicate ON jarvis_facts(predicate);

        -- Compressed conversation summaries
        CREATE TABLE jarvis_summaries (
            id TEXT PRIMARY KEY,
            context_type INTEGER NOT NULL,
            context_id TEXT NOT NULL,
            summary TEXT NOT NULL,
            message_range_start INTEGER NOT NULL,
            message_range_end INTEGER NOT NULL,
            message_count INTEGER NOT NULL,
            created_at TEXT NOT NULL
        );

        -- Index for context lookups
        CREATE INDEX idx_jarvis_summaries_context
            ON jarvis_summaries(context_type, context_id, message_range_end DESC);
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
            DROP TABLE IF EXISTS jarvis_summaries;
            DROP TABLE IF EXISTS jarvis_facts;
            DROP TABLE IF EXISTS jarvis_conversations;
            "#,
        )
    }
}
