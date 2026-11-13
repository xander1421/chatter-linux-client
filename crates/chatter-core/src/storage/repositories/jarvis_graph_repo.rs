//! Jarvis knowledge graph repository.
//!
//! Data access for entity nodes, relationships, and graph queries.
//! Uses SQLite recursive CTEs for graph traversal.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use rusqlite::params;

use crate::domain::jarvis::{EntityType, JarvisEntity, JarvisEntityId};
#[cfg(feature = "testing")]
use crate::domain::jarvis::{JarvisRelationship, JarvisRelationshipId};
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

    /// Get the total number of entities in the knowledge graph.
    pub fn count_entities(&self) -> Result<u32, StorageError> {
        let conn = self.db.read_connection()?;
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM jarvis_entities", [], |row| row.get(0))?;
        Ok(count as u32)
    }

    #[cfg(feature = "testing")]
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

    #[cfg(feature = "testing")]
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

    #[cfg(feature = "testing")]
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

    #[cfg(feature = "testing")]
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

    #[cfg(feature = "testing")]
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

    #[cfg(feature = "testing")]
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

    #[cfg(feature = "testing")]
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

    #[cfg(feature = "testing")]
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

    // ========================================================================
    // High-level Knowledge Queries
    // ========================================================================

}

// ============================================================================
// Row Converters
// ============================================================================

fn row_to_entity(row: &rusqlite::Row) -> Result<JarvisEntity, rusqlite::Error> {
    row_to_entity_offset(row, 0)
}

fn row_to_entity_offset(
    row: &rusqlite::Row,
    offset: usize,
) -> Result<JarvisEntity, rusqlite::Error> {
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

#[cfg(feature = "testing")]
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

fn parse_datetime(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

// Unit tests are in tests/integration_jarvis.rs to use tempfile for database creation
