//! Custom section repository.
//!
//! Data access for custom sidebar sections and their conversation memberships.

use crate::domain::ids::SectionId;
use crate::domain::message::ConversationType;
use crate::error::StorageError;
use crate::storage::Database;
use chrono::{DateTime, Utc};
use rusqlite::params;
use std::sync::Arc;

/// A custom sidebar section.
#[derive(Debug, Clone)]
pub struct Section {
    pub id: SectionId,
    pub name: String,
    pub icon_emoji: String,
    pub sort_order: i32,
    pub is_expanded: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A conversation belonging to a section.
#[derive(Debug, Clone)]
pub struct SectionMembership {
    pub conversation_id: String,
    pub conversation_type: ConversationType,
}

/// Custom section data repository.
pub struct SectionRepository {
    db: Arc<Database>,
}

impl SectionRepository {
    /// Create a new SectionRepository.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    // ========================================================================
    // Section CRUD
    // ========================================================================

    /// Create a new custom section.
    pub fn create_section(&self, name: &str, icon_emoji: &str) -> Result<Section, StorageError> {
        let conn = self.db.connection()?;
        let id = SectionId::new();
        let now = Utc::now();

        // Get next sort_order
        let max_order: i32 = conn
            .query_row(
                "SELECT COALESCE(MAX(sort_order), -1) FROM custom_sections",
                [],
                |row| row.get(0),
            )
            .unwrap_or(-1);

        let sort_order = max_order + 1;

        conn.execute(
            "INSERT INTO custom_sections (id, name, icon_emoji, sort_order, is_expanded, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6)",
            params![
                id.to_string(),
                name,
                icon_emoji,
                sort_order,
                now.to_rfc3339(),
                now.to_rfc3339(),
            ],
        )?;

        Ok(Section {
            id,
            name: name.to_string(),
            icon_emoji: icon_emoji.to_string(),
            sort_order,
            is_expanded: true,
            created_at: now,
            updated_at: now,
        })
    }

    /// Update a section's name, icon, and/or sort order.
    pub fn update_section(
        &self,
        id: &SectionId,
        name: &str,
        icon_emoji: &str,
        sort_order: i32,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now();

        let rows = conn.execute(
            "UPDATE custom_sections SET name = ?1, icon_emoji = ?2, sort_order = ?3, updated_at = ?4
             WHERE id = ?5",
            params![
                name,
                icon_emoji,
                sort_order,
                now.to_rfc3339(),
                id.to_string(),
            ],
        )?;

        if rows == 0 {
            return Err(StorageError::NotFound(format!("Section {} not found", id)));
        }

        Ok(())
    }

    /// Delete a section. CASCADE deletes memberships.
    pub fn delete_section(&self, id: &SectionId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;

        let rows = conn.execute(
            "DELETE FROM custom_sections WHERE id = ?1",
            params![id.to_string()],
        )?;

        if rows == 0 {
            return Err(StorageError::NotFound(format!("Section {} not found", id)));
        }

        Ok(())
    }

    /// List all sections ordered by sort_order.
    pub fn list_sections(&self) -> Result<Vec<Section>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, icon_emoji, sort_order, is_expanded, created_at, updated_at
             FROM custom_sections
             ORDER BY sort_order ASC",
        )?;

        let mut rows = stmt.query([])?;
        let mut sections = Vec::new();
        while let Some(row) = rows.next()? {
            sections.push(row_to_section(row)?);
        }
        Ok(sections)
    }

    /// Toggle the expanded/collapsed state of a section.
    pub fn toggle_section_expanded(
        &self,
        id: &SectionId,
        expanded: bool,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now();

        let rows = conn.execute(
            "UPDATE custom_sections SET is_expanded = ?1, updated_at = ?2 WHERE id = ?3",
            params![expanded as i32, now.to_rfc3339(), id.to_string()],
        )?;

        if rows == 0 {
            return Err(StorageError::NotFound(format!("Section {} not found", id)));
        }

        Ok(())
    }

    // ========================================================================
    // Membership operations
    // ========================================================================

    /// Add a conversation to a section.
    pub fn add_to_section(
        &self,
        section_id: &SectionId,
        conversation_id: &str,
        conversation_type: &ConversationType,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now();

        conn.execute(
            "INSERT OR REPLACE INTO section_memberships (section_id, conversation_id, conversation_type, added_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                section_id.to_string(),
                conversation_id,
                conversation_type.as_str(),
                now.to_rfc3339(),
            ],
        )?;

        Ok(())
    }

    /// Remove a conversation from a section.
    pub fn remove_from_section(
        &self,
        section_id: &SectionId,
        conversation_id: &str,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;

        conn.execute(
            "DELETE FROM section_memberships WHERE section_id = ?1 AND conversation_id = ?2",
            params![section_id.to_string(), conversation_id],
        )?;

        Ok(())
    }

    /// Get all conversations in a section.
    pub fn get_section_conversations(
        &self,
        section_id: &SectionId,
    ) -> Result<Vec<SectionMembership>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT conversation_id, conversation_type
             FROM section_memberships
             WHERE section_id = ?1
             ORDER BY added_at ASC",
        )?;

        let mut rows = stmt.query(params![section_id.to_string()])?;
        let mut memberships = Vec::new();
        while let Some(row) = rows.next()? {
            let conversation_id: String = row.get(0)?;
            let conv_type_str: String = row.get(1)?;
            let conversation_type = ConversationType::from_str_value(&conv_type_str)?;
            memberships.push(SectionMembership {
                conversation_id,
                conversation_type,
            });
        }
        Ok(memberships)
    }

    /// Get which section a conversation belongs to, if any.
    pub fn get_conversation_section(
        &self,
        conversation_id: &str,
    ) -> Result<Option<SectionId>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT section_id FROM section_memberships WHERE conversation_id = ?1",
        )?;

        match stmt.query_row(params![conversation_id], |row| {
            let id_str: String = row.get(0)?;
            Ok(id_str)
        }) {
            Ok(id_str) => {
                let id: SectionId = id_str
                    .parse()
                    .map_err(|e| StorageError::InvalidData(format!("Invalid section ID: {}", e)))?;
                Ok(Some(id))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Return the set of all conversation IDs that belong to any section.
    pub fn all_conversation_ids(&self) -> Result<std::collections::HashSet<String>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT DISTINCT conversation_id FROM section_memberships",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut ids = std::collections::HashSet::new();
        for row in rows {
            ids.insert(row.map_err(StorageError::Database)?);
        }
        Ok(ids)
    }
}

// ============================================================================
// Helper functions
// ============================================================================

/// Parse an RFC3339 datetime string, falling back to now if invalid.
fn parse_datetime(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

/// Convert a database row to Section.
fn row_to_section(row: &rusqlite::Row<'_>) -> Result<Section, StorageError> {
    let id_str: String = row.get(0)?;
    let name: String = row.get(1)?;
    let icon_emoji: String = row.get(2)?;
    let sort_order: i32 = row.get(3)?;
    let is_expanded: i32 = row.get(4)?;
    let created_at_str: String = row.get(5)?;
    let updated_at_str: String = row.get(6)?;

    let id: SectionId = id_str
        .parse()
        .map_err(|e| StorageError::InvalidData(format!("Invalid section ID: {}", e)))?;

    Ok(Section {
        id,
        name,
        icon_emoji,
        sort_order,
        is_expanded: is_expanded != 0,
        created_at: parse_datetime(&created_at_str),
        updated_at: parse_datetime(&updated_at_str),
    })
}
