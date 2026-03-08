//! Migration v032: Jarvis knowledge graph storage.
//!
//! Creates tables for:
//! - jarvis_entities: Entity nodes (people, projects, topics, etc.)
//! - jarvis_relationships: Edges between entities
//! - jarvis_entity_mentions: Tracking entity mentions in context

use super::Migration;

pub struct V032JarvisGraph;

impl Migration for V032JarvisGraph {
    fn version(&self) -> u32 {
        32
    }

    fn description(&self) -> &'static str {
        "Create Jarvis knowledge graph tables"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Entity nodes in the knowledge graph
        CREATE TABLE jarvis_entities (
            id TEXT PRIMARY KEY,
            entity_type INTEGER NOT NULL,  -- 0=person, 1=project, 2=topic, 3=org, 4=tech
            name TEXT NOT NULL,
            normalized_name TEXT NOT NULL,
            aliases_json TEXT,
            properties_json TEXT,
            first_seen_at TEXT NOT NULL,
            last_seen_at TEXT NOT NULL,
            mention_count INTEGER DEFAULT 1,
            created_at TEXT NOT NULL
        );

        -- Index for name lookups (case-insensitive via normalized_name)
        CREATE INDEX idx_jarvis_entities_name ON jarvis_entities(normalized_name);

        -- Index for type filtering
        CREATE INDEX idx_jarvis_entities_type ON jarvis_entities(entity_type);

        -- Index for popularity sorting
        CREATE INDEX idx_jarvis_entities_mentions ON jarvis_entities(mention_count DESC);

        -- Relationship edges between entities
        CREATE TABLE jarvis_relationships (
            id TEXT PRIMARY KEY,
            from_entity_id TEXT NOT NULL REFERENCES jarvis_entities(id) ON DELETE CASCADE,
            to_entity_id TEXT NOT NULL REFERENCES jarvis_entities(id) ON DELETE CASCADE,
            relationship_type TEXT NOT NULL,  -- knows, works_on, interested_in, mentioned_with
            strength REAL DEFAULT 1.0,
            last_seen_at TEXT NOT NULL,
            context_count INTEGER DEFAULT 1,
            created_at TEXT NOT NULL,
            UNIQUE(from_entity_id, to_entity_id, relationship_type)
        );

        -- Index for graph traversal from source
        CREATE INDEX idx_jarvis_relationships_from ON jarvis_relationships(from_entity_id);

        -- Index for graph traversal to target
        CREATE INDEX idx_jarvis_relationships_to ON jarvis_relationships(to_entity_id);

        -- Index for relationship type queries
        CREATE INDEX idx_jarvis_relationships_type ON jarvis_relationships(relationship_type);

        -- Entity mention tracking for context association
        CREATE TABLE jarvis_entity_mentions (
            id TEXT PRIMARY KEY,
            entity_id TEXT NOT NULL REFERENCES jarvis_entities(id) ON DELETE CASCADE,
            context_type INTEGER NOT NULL,
            context_id TEXT NOT NULL,
            message_id TEXT,
            timestamp INTEGER NOT NULL
        );

        -- Index for entity mention lookups
        CREATE INDEX idx_jarvis_mentions_entity ON jarvis_entity_mentions(entity_id, timestamp DESC);

        -- Index for context mention lookups
        CREATE INDEX idx_jarvis_mentions_context ON jarvis_entity_mentions(context_type, context_id, timestamp DESC);
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
            DROP TABLE IF EXISTS jarvis_entity_mentions;
            DROP TABLE IF EXISTS jarvis_relationships;
            DROP TABLE IF EXISTS jarvis_entities;
            "#,
        )
    }
}
