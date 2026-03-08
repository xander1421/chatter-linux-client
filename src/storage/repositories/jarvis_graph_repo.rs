//! Jarvis knowledge graph repository.
//!
//! Data access for entity nodes, relationships, and graph queries.
//! Uses SQLite recursive CTEs for graph traversal.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use rusqlite::params;

use crate::domain::jarvis::{
    ContextType, EntityKnowledge, EntityType, JarvisContext, JarvisEntity, JarvisEntityId,
    JarvisEntityMention, JarvisRelationship, JarvisRelationshipId, SubjectType,
};
use crate::error::StorageError;
use crate::storage::Database;

/// Repository for Jarvis knowledge graph operations.
pub struct JarvisGraphRepository {
    db: Arc<Database>,
}

impl JarvisGraphRepository {
    /// Create a new repository with the given database.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    // ========================================================================
    // Entity CRUD
    // ========================================================================

    /// Insert or update an entity.
    ///
    /// If an entity with the same normalized name exists, updates it.
    pub fn upsert_entity(&self, entity: &JarvisEntity) -> Result<JarvisEntityId, StorageError> {
        let conn = self.db.connection()?;

        // Check for existing entity by normalized name
        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM jarvis_entities WHERE normalized_name = ?1",
                params![entity.normalized_name],
                |row| row.get(0),
            )
            .ok();

        if let Some(existing_id) = existing {
            // Update existing entity
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "UPDATE jarvis_entities SET
                    last_seen_at = ?1,
                    mention_count = mention_count + 1,
                    properties_json = COALESCE(?2, properties_json)
                 WHERE id = ?3",
                params![now, entity.properties_json, existing_id],
            )?;
            Ok(JarvisEntityId::from_string(existing_id))
        } else {
            // Insert new entity
            conn.execute(
                "INSERT INTO jarvis_entities (id, entity_type, name, normalized_name, aliases_json, properties_json, first_seen_at, last_seen_at, mention_count, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    entity.id.as_str(),
                    entity.entity_type.to_i32(),
                    entity.name,
                    entity.normalized_name,
                    entity.aliases_json,
                    entity.properties_json,
                    entity.first_seen_at.to_rfc3339(),
                    entity.last_seen_at.to_rfc3339(),
                    entity.mention_count,
                    entity.created_at.to_rfc3339(),
                ],
            )?;
            Ok(entity.id.clone())
        }
    }

    /// Get entity by ID.
    pub fn get_entity(&self, id: &JarvisEntityId) -> Result<Option<JarvisEntity>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, entity_type, name, normalized_name, aliases_json, properties_json, first_seen_at, last_seen_at, mention_count, created_at
             FROM jarvis_entities WHERE id = ?1",
        )?;

        match stmt.query_row(params![id.as_str()], row_to_entity) {
            Ok(entity) => Ok(Some(entity)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Get entity by normalized name.
    pub fn get_entity_by_name(&self, name: &str) -> Result<Option<JarvisEntity>, StorageError> {
        let normalized = name.trim().to_lowercase();
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, entity_type, name, normalized_name, aliases_json, properties_json, first_seen_at, last_seen_at, mention_count, created_at
             FROM jarvis_entities WHERE normalized_name = ?1",
        )?;

        match stmt.query_row(params![normalized], row_to_entity) {
            Ok(entity) => Ok(Some(entity)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Search entities by name (fuzzy match).
    pub fn search_entities(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<Vec<JarvisEntity>, StorageError> {
        let pattern = format!("%{}%", query.trim().to_lowercase());
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, entity_type, name, normalized_name, aliases_json, properties_json, first_seen_at, last_seen_at, mention_count, created_at
             FROM jarvis_entities
             WHERE normalized_name LIKE ?1 OR aliases_json LIKE ?1
             ORDER BY mention_count DESC
             LIMIT ?2",
        )?;

        let mut rows = stmt.query(params![pattern, limit])?;
        let mut entities = Vec::new();

        while let Some(row) = rows.next()? {
            entities.push(row_to_entity(row)?);
        }

        Ok(entities)
    }

    /// Get top entities by mention count.
    pub fn get_top_entities(&self, limit: u32) -> Result<Vec<JarvisEntity>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, entity_type, name, normalized_name, aliases_json, properties_json, first_seen_at, last_seen_at, mention_count, created_at
             FROM jarvis_entities
             ORDER BY mention_count DESC
             LIMIT ?1",
        )?;

        let mut rows = stmt.query(params![limit])?;
        let mut entities = Vec::new();

        while let Some(row) = rows.next()? {
            entities.push(row_to_entity(row)?);
        }

        Ok(entities)
    }

    /// Get entities by type.
    pub fn get_entities_by_type(
        &self,
        entity_type: EntityType,
        limit: u32,
    ) -> Result<Vec<JarvisEntity>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, entity_type, name, normalized_name, aliases_json, properties_json, first_seen_at, last_seen_at, mention_count, created_at
             FROM jarvis_entities
             WHERE entity_type = ?1
             ORDER BY mention_count DESC
             LIMIT ?2",
        )?;

        let mut rows = stmt.query(params![entity_type.to_i32(), limit])?;
        let mut entities = Vec::new();

        while let Some(row) = rows.next()? {
            entities.push(row_to_entity(row)?);
        }

        Ok(entities)
    }

    // ========================================================================
    // Relationship CRUD
    // ========================================================================

    /// Insert or update a relationship.
    pub fn upsert_relationship(
        &self,
        rel: &JarvisRelationship,
    ) -> Result<JarvisRelationshipId, StorageError> {
        let conn = self.db.connection()?;

        // Try to update existing relationship
        let now = Utc::now().to_rfc3339();
        let rows = conn.execute(
            "UPDATE jarvis_relationships SET
                strength = MIN(strength + 0.1, 1.0),
                last_seen_at = ?1,
                context_count = context_count + 1
             WHERE from_entity_id = ?2 AND to_entity_id = ?3 AND relationship_type = ?4",
            params![
                now,
                rel.from_entity_id.as_str(),
                rel.to_entity_id.as_str(),
                rel.relationship_type,
            ],
        )?;

        if rows > 0 {
            // Relationship was updated, get its ID
            let id: String = conn.query_row(
                "SELECT id FROM jarvis_relationships WHERE from_entity_id = ?1 AND to_entity_id = ?2 AND relationship_type = ?3",
                params![rel.from_entity_id.as_str(), rel.to_entity_id.as_str(), rel.relationship_type],
                |row| row.get(0),
            )?;
            Ok(JarvisRelationshipId::from_string(id))
        } else {
            // Insert new relationship
            conn.execute(
                "INSERT INTO jarvis_relationships (id, from_entity_id, to_entity_id, relationship_type, strength, last_seen_at, context_count, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    rel.id.as_str(),
                    rel.from_entity_id.as_str(),
                    rel.to_entity_id.as_str(),
                    rel.relationship_type,
                    rel.strength,
                    rel.last_seen_at.to_rfc3339(),
                    rel.context_count,
                    rel.created_at.to_rfc3339(),
                ],
            )?;
            Ok(rel.id.clone())
        }
    }

    /// Get relationships from an entity.
    pub fn get_relationships_from(
        &self,
        entity_id: &JarvisEntityId,
    ) -> Result<Vec<(JarvisRelationship, JarvisEntity)>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT r.id, r.from_entity_id, r.to_entity_id, r.relationship_type, r.strength, r.last_seen_at, r.context_count, r.created_at,
                    e.id, e.entity_type, e.name, e.normalized_name, e.aliases_json, e.properties_json, e.first_seen_at, e.last_seen_at, e.mention_count, e.created_at
             FROM jarvis_relationships r
             JOIN jarvis_entities e ON e.id = r.to_entity_id
             WHERE r.from_entity_id = ?1
             ORDER BY r.strength DESC",
        )?;

        let mut rows = stmt.query(params![entity_id.as_str()])?;
        let mut results = Vec::new();

        while let Some(row) = rows.next()? {
            let rel = row_to_relationship(row)?;
            let entity = row_to_entity_offset(row, 8)?;
            results.push((rel, entity));
        }

        Ok(results)
    }

    /// Get relationships to an entity.
    pub fn get_relationships_to(
        &self,
        entity_id: &JarvisEntityId,
    ) -> Result<Vec<(JarvisRelationship, JarvisEntity)>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT r.id, r.from_entity_id, r.to_entity_id, r.relationship_type, r.strength, r.last_seen_at, r.context_count, r.created_at,
                    e.id, e.entity_type, e.name, e.normalized_name, e.aliases_json, e.properties_json, e.first_seen_at, e.last_seen_at, e.mention_count, e.created_at
             FROM jarvis_relationships r
             JOIN jarvis_entities e ON e.id = r.from_entity_id
             WHERE r.to_entity_id = ?1
             ORDER BY r.strength DESC",
        )?;

        let mut rows = stmt.query(params![entity_id.as_str()])?;
        let mut results = Vec::new();

        while let Some(row) = rows.next()? {
            let rel = row_to_relationship(row)?;
            let entity = row_to_entity_offset(row, 8)?;
            results.push((rel, entity));
        }

        Ok(results)
    }

    /// Get connected entities using recursive CTE (graph traversal).
    ///
    /// Returns entities within `max_depth` hops of the starting entity.
    pub fn get_connected_entities(
        &self,
        entity_id: &JarvisEntityId,
        max_depth: u32,
    ) -> Result<Vec<(JarvisEntity, u32)>, StorageError> {
        let conn = self.db.read_connection()?;

        // Use recursive CTE to traverse the graph
        let mut stmt = conn.prepare(
            "WITH RECURSIVE connected(entity_id, depth) AS (
                SELECT ?1, 0
                UNION ALL
                SELECT CASE
                    WHEN r.from_entity_id = c.entity_id THEN r.to_entity_id
                    ELSE r.from_entity_id
                END, c.depth + 1
                FROM connected c
                JOIN jarvis_relationships r ON r.from_entity_id = c.entity_id OR r.to_entity_id = c.entity_id
                WHERE c.depth < ?2
            )
            SELECT DISTINCT e.id, e.entity_type, e.name, e.normalized_name, e.aliases_json, e.properties_json, e.first_seen_at, e.last_seen_at, e.mention_count, e.created_at, MIN(c.depth) as depth
            FROM connected c
            JOIN jarvis_entities e ON e.id = c.entity_id
            WHERE e.id != ?1
            GROUP BY e.id
            ORDER BY depth, e.mention_count DESC",
        )?;

        let mut rows = stmt.query(params![entity_id.as_str(), max_depth])?;
        let mut results = Vec::new();

        while let Some(row) = rows.next()? {
            let entity = row_to_entity(row)?;
            let depth: i32 = row.get(10)?;
            results.push((entity, depth as u32));
        }

        Ok(results)
    }

    // ========================================================================
    // Mention CRUD
    // ========================================================================

    /// Insert an entity mention.
    pub fn insert_mention(&self, mention: &JarvisEntityMention) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT INTO jarvis_entity_mentions (id, entity_id, context_type, context_id, message_id, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                mention.id,
                mention.entity_id.as_str(),
                mention.context_type.to_i32(),
                mention.context_id,
                mention.message_id,
                mention.timestamp as i64,
            ],
        )?;
        Ok(())
    }

    /// Get mentions of an entity.
    pub fn get_mentions(
        &self,
        entity_id: &JarvisEntityId,
        limit: u32,
    ) -> Result<Vec<JarvisEntityMention>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, context_type, context_id, message_id, timestamp
             FROM jarvis_entity_mentions
             WHERE entity_id = ?1
             ORDER BY timestamp DESC
             LIMIT ?2",
        )?;

        let mut rows = stmt.query(params![entity_id.as_str(), limit])?;
        let mut mentions = Vec::new();

        while let Some(row) = rows.next()? {
            mentions.push(row_to_mention(row)?);
        }

        Ok(mentions)
    }

    /// Get entities mentioned in a context.
    pub fn get_context_entities(
        &self,
        context: &JarvisContext,
        limit: u32,
    ) -> Result<Vec<JarvisEntity>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT DISTINCT e.id, e.entity_type, e.name, e.normalized_name, e.aliases_json, e.properties_json, e.first_seen_at, e.last_seen_at, e.mention_count, e.created_at
             FROM jarvis_entity_mentions m
             JOIN jarvis_entities e ON e.id = m.entity_id
             WHERE m.context_type = ?1 AND m.context_id = ?2
             ORDER BY m.timestamp DESC
             LIMIT ?3",
        )?;

        let mut rows = stmt.query(params![
            context.context_type.to_i32(),
            context.context_id,
            limit,
        ])?;
        let mut entities = Vec::new();

        while let Some(row) = rows.next()? {
            entities.push(row_to_entity(row)?);
        }

        Ok(entities)
    }

    // ========================================================================
    // High-level Knowledge Queries
    // ========================================================================

    /// Get comprehensive knowledge about an entity.
    pub fn get_entity_knowledge(
        &self,
        name: &str,
        memory_repo: &super::jarvis_memory_repo::JarvisMemoryRepository,
    ) -> Result<EntityKnowledge, StorageError> {
        let entity = self.get_entity_by_name(name)?;

        let (related, facts) = if let Some(ref ent) = entity {
            // Get relationships
            let from_rels = self.get_relationships_from(&ent.id)?;
            let to_rels = self.get_relationships_to(&ent.id)?;
            let mut related: Vec<(JarvisRelationship, JarvisEntity)> = from_rels;
            related.extend(to_rels);

            // Get facts about this entity
            let facts = memory_repo.get_facts_about(SubjectType::Topic, &ent.normalized_name)?;

            (related, facts)
        } else {
            (vec![], vec![])
        };

        Ok(EntityKnowledge {
            entity,
            related,
            facts,
        })
    }

    /// Get entities co-mentioned with given entities in a context.
    pub fn get_co_mentioned_entities(
        &self,
        entity_ids: &[JarvisEntityId],
        limit: u32,
    ) -> Result<Vec<JarvisEntity>, StorageError> {
        if entity_ids.is_empty() {
            return Ok(vec![]);
        }

        let conn = self.db.read_connection()?;

        // Build placeholders for IN clause
        let placeholders: Vec<String> = entity_ids.iter().enumerate().map(|(i, _)| format!("?{}", i + 1)).collect();
        let in_clause = placeholders.join(", ");

        let sql = format!(
            "SELECT DISTINCT e.id, e.entity_type, e.name, e.normalized_name, e.aliases_json, e.properties_json, e.first_seen_at, e.last_seen_at, e.mention_count, e.created_at
             FROM jarvis_entity_mentions m1
             JOIN jarvis_entity_mentions m2 ON m1.context_type = m2.context_type AND m1.context_id = m2.context_id
             JOIN jarvis_entities e ON e.id = m2.entity_id
             WHERE m1.entity_id IN ({}) AND m2.entity_id NOT IN ({})
             ORDER BY e.mention_count DESC
             LIMIT ?{}",
            in_clause,
            in_clause,
            entity_ids.len() * 2 + 1
        );

        let mut stmt = conn.prepare(&sql)?;

        // Build params: entity_ids twice (for both IN clauses) + limit
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = entity_ids
            .iter()
            .map(|id| Box::new(id.as_str().to_string()) as Box<dyn rusqlite::ToSql>)
            .collect();
        params_vec.extend(
            entity_ids
                .iter()
                .map(|id| Box::new(id.as_str().to_string()) as Box<dyn rusqlite::ToSql>),
        );
        params_vec.push(Box::new(limit));

        let params_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();
        let mut rows = stmt.query(params_refs.as_slice())?;
        let mut entities = Vec::new();

        while let Some(row) = rows.next()? {
            entities.push(row_to_entity(row)?);
        }

        Ok(entities)
    }

    /// Delete old mentions and prune unused entities.
    pub fn prune_old_data(&self, days_old: u32) -> Result<(u32, u32), StorageError> {
        let conn = self.db.connection()?;
        let cutoff = Utc::now() - chrono::Duration::days(days_old as i64);
        let cutoff_ns = cutoff.timestamp_nanos_opt().unwrap_or(0) as i64;

        // Delete old mentions
        let mentions_deleted = conn.execute(
            "DELETE FROM jarvis_entity_mentions WHERE timestamp < ?1",
            params![cutoff_ns],
        )?;

        // Delete entities with no mentions and no relationships
        let entities_deleted = conn.execute(
            "DELETE FROM jarvis_entities WHERE id NOT IN (
                SELECT DISTINCT entity_id FROM jarvis_entity_mentions
            ) AND id NOT IN (
                SELECT DISTINCT from_entity_id FROM jarvis_relationships
            ) AND id NOT IN (
                SELECT DISTINCT to_entity_id FROM jarvis_relationships
            )",
            [],
        )?;

        Ok((mentions_deleted as u32, entities_deleted as u32))
    }
}

// ============================================================================
// Row Converters
// ============================================================================

fn row_to_entity(row: &rusqlite::Row) -> Result<JarvisEntity, rusqlite::Error> {
    row_to_entity_offset(row, 0)
}

fn row_to_entity_offset(row: &rusqlite::Row, offset: usize) -> Result<JarvisEntity, rusqlite::Error> {
    let id: String = row.get(offset)?;
    let entity_type: i32 = row.get(offset + 1)?;
    let name: String = row.get(offset + 2)?;
    let normalized_name: String = row.get(offset + 3)?;
    let aliases_json: Option<String> = row.get(offset + 4)?;
    let properties_json: Option<String> = row.get(offset + 5)?;
    let first_seen_at_str: String = row.get(offset + 6)?;
    let last_seen_at_str: String = row.get(offset + 7)?;
    let mention_count: i32 = row.get(offset + 8)?;
    let created_at_str: String = row.get(offset + 9)?;

    Ok(JarvisEntity {
        id: JarvisEntityId::from_string(id),
        entity_type: EntityType::from_i32(entity_type),
        name,
        normalized_name,
        aliases_json,
        properties_json,
        first_seen_at: parse_datetime(&first_seen_at_str),
        last_seen_at: parse_datetime(&last_seen_at_str),
        mention_count: mention_count as u32,
        created_at: parse_datetime(&created_at_str),
    })
}

fn row_to_relationship(row: &rusqlite::Row) -> Result<JarvisRelationship, rusqlite::Error> {
    let id: String = row.get(0)?;
    let from_entity_id: String = row.get(1)?;
    let to_entity_id: String = row.get(2)?;
    let relationship_type: String = row.get(3)?;
    let strength: f64 = row.get(4)?;
    let last_seen_at_str: String = row.get(5)?;
    let context_count: i32 = row.get(6)?;
    let created_at_str: String = row.get(7)?;

    Ok(JarvisRelationship {
        id: JarvisRelationshipId::from_string(id),
        from_entity_id: JarvisEntityId::from_string(from_entity_id),
        to_entity_id: JarvisEntityId::from_string(to_entity_id),
        relationship_type,
        strength,
        last_seen_at: parse_datetime(&last_seen_at_str),
        context_count: context_count as u32,
        created_at: parse_datetime(&created_at_str),
    })
}

fn row_to_mention(row: &rusqlite::Row) -> Result<JarvisEntityMention, rusqlite::Error> {
    let id: String = row.get(0)?;
    let entity_id: String = row.get(1)?;
    let context_type: i32 = row.get(2)?;
    let context_id: String = row.get(3)?;
    let message_id: Option<String> = row.get(4)?;
    let timestamp: i64 = row.get(5)?;

    #[allow(clippy::cast_sign_loss)]
    Ok(JarvisEntityMention {
        id,
        entity_id: JarvisEntityId::from_string(entity_id),
        context_type: ContextType::from_i32(context_type),
        context_id,
        message_id,
        timestamp: timestamp as u64,
    })
}

fn parse_datetime(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

// Unit tests are in tests/integration_jarvis.rs to use tempfile for database creation
