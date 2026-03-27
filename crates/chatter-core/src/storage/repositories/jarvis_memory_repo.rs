//! Jarvis memory repository.
//!
//! Data access for Jarvis conversation history, facts, and summaries.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use rusqlite::params;

use crate::domain::jarvis::{
    ContextType, JarvisContext, JarvisConversation, JarvisConversationId,
    JarvisFact, JarvisFactId, JarvisSummary, JarvisSummaryId, Role, SubjectType,
};
use crate::error::StorageError;
use crate::storage::Database;

/// Repository for Jarvis memory operations.
pub struct JarvisMemoryRepository {
    db: Arc<Database>,
}

impl JarvisMemoryRepository {
    /// Create a new repository with the given database.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    // ========================================================================
    // Conversation CRUD
    // ========================================================================

    /// Insert a conversation entry.
    pub fn insert_conversation(&self, conv: &JarvisConversation) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT INTO jarvis_conversations (id, context_type, context_id, role, content, timestamp, metadata_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                conv.id.as_str(),
                conv.context_type.to_i32(),
                conv.context_id,
                conv.role.to_i32(),
                conv.content,
                conv.timestamp as i64,
                conv.metadata_json,
                conv.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Get recent conversation history for a context.
    pub fn get_recent_conversations(
        &self,
        context: &JarvisContext,
        limit: u32,
    ) -> Result<Vec<JarvisConversation>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, context_type, context_id, role, content, timestamp, metadata_json, created_at
             FROM jarvis_conversations
             WHERE context_type = ?1 AND context_id = ?2
             ORDER BY timestamp DESC
             LIMIT ?3",
        )?;

        let mut rows = stmt.query(params![
            context.context_type.to_i32(),
            context.context_id,
            limit,
        ])?;
        let mut conversations = Vec::new();

        while let Some(row) = rows.next()? {
            conversations.push(row_to_conversation(row)?);
        }

        // Reverse to get chronological order
        conversations.reverse();
        Ok(conversations)
    }

    /// Get conversation history before a timestamp.
    pub fn get_conversations_before(
        &self,
        context: &JarvisContext,
        before_timestamp: u64,
        limit: u32,
    ) -> Result<Vec<JarvisConversation>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, context_type, context_id, role, content, timestamp, metadata_json, created_at
             FROM jarvis_conversations
             WHERE context_type = ?1 AND context_id = ?2 AND timestamp < ?3
             ORDER BY timestamp DESC
             LIMIT ?4",
        )?;

        let mut rows = stmt.query(params![
            context.context_type.to_i32(),
            context.context_id,
            before_timestamp as i64,
            limit,
        ])?;
        let mut conversations = Vec::new();

        while let Some(row) = rows.next()? {
            conversations.push(row_to_conversation(row)?);
        }

        conversations.reverse();
        Ok(conversations)
    }

    /// Count conversations in a context.
    pub fn count_conversations(&self, context: &JarvisContext) -> Result<u32, StorageError> {
        let conn = self.db.read_connection()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM jarvis_conversations WHERE context_type = ?1 AND context_id = ?2",
            params![context.context_type.to_i32(), context.context_id],
            |row| row.get(0),
        )?;
        Ok(count as u32)
    }

    /// Delete old conversations (for compression).
    pub fn delete_conversations_in_range(
        &self,
        context: &JarvisContext,
        start_timestamp: u64,
        end_timestamp: u64,
    ) -> Result<u32, StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "DELETE FROM jarvis_conversations
             WHERE context_type = ?1 AND context_id = ?2
             AND timestamp >= ?3 AND timestamp <= ?4",
            params![
                context.context_type.to_i32(),
                context.context_id,
                start_timestamp as i64,
                end_timestamp as i64,
            ],
        )?;
        Ok(rows as u32)
    }

    // ========================================================================
    // Fact CRUD
    // ========================================================================

    /// Insert or update a fact.
    ///
    /// If a fact with the same subject and predicate exists, reinforces it.
    pub fn upsert_fact(&self, fact: &JarvisFact) -> Result<(), StorageError> {
        let conn = self.db.connection()?;

        // Check for existing fact with same subject and predicate
        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM jarvis_facts WHERE subject_type = ?1 AND subject_id = ?2 AND predicate = ?3",
                params![fact.subject_type.to_i32(), fact.subject_id, fact.predicate],
                |row| row.get(0),
            )
            .ok();

        if let Some(existing_id) = existing {
            // Reinforce existing fact
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "UPDATE jarvis_facts SET
                    object = ?1,
                    confidence = MIN(confidence + 0.2, 1.0),
                    last_reinforced_at = ?2
                 WHERE id = ?3",
                params![fact.object, now, existing_id],
            )?;
        } else {
            // Insert new fact
            conn.execute(
                "INSERT INTO jarvis_facts (id, subject_type, subject_id, predicate, object, confidence, source_context_id, last_reinforced_at, decay_rate, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    fact.id.as_str(),
                    fact.subject_type.to_i32(),
                    fact.subject_id,
                    fact.predicate,
                    fact.object,
                    fact.confidence,
                    fact.source_context_id,
                    fact.last_reinforced_at.to_rfc3339(),
                    fact.decay_rate,
                    fact.created_at.to_rfc3339(),
                ],
            )?;
        }
        Ok(())
    }

    /// Get facts about a subject.
    pub fn get_facts_about(
        &self,
        subject_type: SubjectType,
        subject_id: &str,
    ) -> Result<Vec<JarvisFact>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, subject_type, subject_id, predicate, object, confidence, source_context_id, last_reinforced_at, decay_rate, created_at
             FROM jarvis_facts
             WHERE subject_type = ?1 AND subject_id = ?2
             ORDER BY last_reinforced_at DESC",
        )?;

        let mut rows = stmt.query(params![subject_type.to_i32(), subject_id])?;
        let mut facts = Vec::new();

        while let Some(row) = rows.next()? {
            facts.push(row_to_fact(row)?);
        }

        Ok(facts)
    }

    /// Get facts by predicate (e.g., all "works_on" facts).
    pub fn get_facts_by_predicate(&self, predicate: &str) -> Result<Vec<JarvisFact>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, subject_type, subject_id, predicate, object, confidence, source_context_id, last_reinforced_at, decay_rate, created_at
             FROM jarvis_facts
             WHERE predicate = ?1
             ORDER BY confidence DESC",
        )?;

        let mut rows = stmt.query(params![predicate])?;
        let mut facts = Vec::new();

        while let Some(row) = rows.next()? {
            facts.push(row_to_fact(row)?);
        }

        Ok(facts)
    }

    /// Search facts by object content.
    pub fn search_facts(&self, query: &str, limit: u32) -> Result<Vec<JarvisFact>, StorageError> {
        let conn = self.db.read_connection()?;
        let pattern = format!("%{}%", query);
        let mut stmt = conn.prepare(
            "SELECT id, subject_type, subject_id, predicate, object, confidence, source_context_id, last_reinforced_at, decay_rate, created_at
             FROM jarvis_facts
             WHERE object LIKE ?1 OR predicate LIKE ?1
             ORDER BY confidence DESC
             LIMIT ?2",
        )?;

        let mut rows = stmt.query(params![pattern, limit])?;
        let mut facts = Vec::new();

        while let Some(row) = rows.next()? {
            facts.push(row_to_fact(row)?);
        }

        Ok(facts)
    }

    /// Apply decay to all facts and delete low-confidence ones.
    ///
    /// Computes exponential decay in Rust since SQLite lacks EXP().
    /// Formula: effective_confidence = confidence * exp(-decay_rate * days_since_reinforced)
    pub fn apply_decay(&self, threshold: f64) -> Result<u32, StorageError> {
        let conn = self.db.connection()?;

        // Fetch all facts with their decay parameters
        let mut stmt = conn.prepare(
            "SELECT id, confidence, decay_rate, julianday('now') - julianday(last_reinforced_at) AS days_elapsed
             FROM jarvis_facts",
        )?;

        let ids_to_delete: Vec<String> = stmt
            .query_map([], |row| {
                let id: String = row.get(0)?;
                let confidence: f64 = row.get(1)?;
                let decay_rate: f64 = row.get(2)?;
                let days_elapsed: f64 = row.get(3)?;
                Ok((id, confidence, decay_rate, days_elapsed))
            })?
            .filter_map(|r| r.ok())
            .filter(|(_, confidence, decay_rate, days)| {
                confidence * (-decay_rate * days).exp() < threshold
            })
            .map(|(id, ..)| id)
            .collect();

        if ids_to_delete.is_empty() {
            return Ok(0);
        }

        let count = ids_to_delete.len() as u32;
        for id in &ids_to_delete {
            conn.execute("DELETE FROM jarvis_facts WHERE id = ?1", params![id])?;
        }

        Ok(count)
    }

    /// Delete a specific fact.
    pub fn delete_fact(&self, id: &JarvisFactId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute("DELETE FROM jarvis_facts WHERE id = ?1", params![id.as_str()])?;
        Ok(())
    }

    // ========================================================================
    // Summary CRUD
    // ========================================================================

    /// Insert a summary.
    pub fn insert_summary(&self, summary: &JarvisSummary) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT INTO jarvis_summaries (id, context_type, context_id, summary, message_range_start, message_range_end, message_count, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                summary.id.as_str(),
                summary.context_type.to_i32(),
                summary.context_id,
                summary.summary,
                summary.message_range_start as i64,
                summary.message_range_end as i64,
                summary.message_count,
                summary.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Get summaries for a context.
    pub fn get_summaries(
        &self,
        context: &JarvisContext,
        limit: u32,
    ) -> Result<Vec<JarvisSummary>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, context_type, context_id, summary, message_range_start, message_range_end, message_count, created_at
             FROM jarvis_summaries
             WHERE context_type = ?1 AND context_id = ?2
             ORDER BY message_range_end DESC
             LIMIT ?3",
        )?;

        let mut rows = stmt.query(params![
            context.context_type.to_i32(),
            context.context_id,
            limit,
        ])?;
        let mut summaries = Vec::new();

        while let Some(row) = rows.next()? {
            summaries.push(row_to_summary(row)?);
        }

        Ok(summaries)
    }

    /// Get latest summary end timestamp for a context.
    pub fn get_latest_summary_end(
        &self,
        context: &JarvisContext,
    ) -> Result<Option<u64>, StorageError> {
        let conn = self.db.read_connection()?;
        let result: Option<i64> = conn
            .query_row(
                "SELECT MAX(message_range_end) FROM jarvis_summaries WHERE context_type = ?1 AND context_id = ?2",
                params![context.context_type.to_i32(), context.context_id],
                |row| row.get(0),
            )
            .ok()
            .flatten();

        Ok(result.map(|v| v as u64))
    }
}

// ============================================================================
// Row Converters
// ============================================================================

fn row_to_conversation(row: &rusqlite::Row) -> Result<JarvisConversation, rusqlite::Error> {
    let id: String = row.get(0)?;
    let context_type: i32 = row.get(1)?;
    let context_id: String = row.get(2)?;
    let role: i32 = row.get(3)?;
    let content: String = row.get(4)?;
    let timestamp: i64 = row.get(5)?;
    let metadata_json: Option<String> = row.get(6)?;
    let created_at_str: String = row.get(7)?;

    Ok(JarvisConversation {
        id: JarvisConversationId::from_string(id),
        context_type: ContextType::from_i32(context_type),
        context_id,
        role: Role::from_i32(role),
        content,
        timestamp: timestamp as u64,
        metadata_json,
        created_at: parse_datetime(&created_at_str),
    })
}

fn row_to_fact(row: &rusqlite::Row) -> Result<JarvisFact, rusqlite::Error> {
    let id: String = row.get(0)?;
    let subject_type: i32 = row.get(1)?;
    let subject_id: String = row.get(2)?;
    let predicate: String = row.get(3)?;
    let object: String = row.get(4)?;
    let confidence: f64 = row.get(5)?;
    let source_context_id: Option<String> = row.get(6)?;
    let last_reinforced_at_str: String = row.get(7)?;
    let decay_rate: f64 = row.get(8)?;
    let created_at_str: String = row.get(9)?;

    Ok(JarvisFact {
        id: JarvisFactId::from_string(id),
        subject_type: SubjectType::from_i32(subject_type),
        subject_id,
        predicate,
        object,
        confidence,
        source_context_id,
        last_reinforced_at: parse_datetime(&last_reinforced_at_str),
        decay_rate,
        created_at: parse_datetime(&created_at_str),
    })
}

fn row_to_summary(row: &rusqlite::Row) -> Result<JarvisSummary, rusqlite::Error> {
    let id: String = row.get(0)?;
    let context_type: i32 = row.get(1)?;
    let context_id: String = row.get(2)?;
    let summary: String = row.get(3)?;
    let message_range_start: i64 = row.get(4)?;
    let message_range_end: i64 = row.get(5)?;
    let message_count: i32 = row.get(6)?;
    let created_at_str: String = row.get(7)?;

    Ok(JarvisSummary {
        id: JarvisSummaryId::from_string(id),
        context_type: ContextType::from_i32(context_type),
        context_id,
        summary,
        message_range_start: message_range_start as u64,
        message_range_end: message_range_end as u64,
        message_count: message_count as u32,
        created_at: parse_datetime(&created_at_str),
    })
}

fn parse_datetime(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

// Unit tests are in tests/integration_jarvis.rs to use tempfile for database creation
