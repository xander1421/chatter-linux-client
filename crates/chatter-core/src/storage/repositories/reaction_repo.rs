//! Reaction repository.
//!
//! Data access for reaction entities. Target messages are identified by
//! (sender_id, sent_at_ns) composite key matching Signal Protocol.

use crate::domain::ids::{ReactionId, UserId};
use crate::domain::message::ReactionCount;
use crate::domain::reaction::{Emoji, Reaction};
use crate::error::StorageError;
use crate::storage::Database;
use chrono::{DateTime, Utc};
use rusqlite::params;
use std::sync::Arc;

/// Reaction data repository.
pub struct ReactionRepository {
    db: Arc<Database>,
}

impl ReactionRepository {
    /// Create a new ReactionRepository.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    // ========================================================================
    // Reaction CRUD
    // ========================================================================

    /// Insert a new reaction.
    pub fn insert(&self, reaction: &Reaction) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let emoji_json = serde_json::to_string(&reaction.emoji)
            .map_err(|e| StorageError::InvalidData(format!("Failed to serialize emoji: {}", e)))?;

        conn.execute(
            "INSERT INTO reactions (id, sender_id, sent_at_ns, user_id, emoji_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                reaction.id.to_string(),
                reaction.sender_id.to_string(),
                reaction.sent_at_ns as i64,
                reaction.user_id.to_string(),
                emoji_json,
                reaction.created_at.to_rfc3339(),
            ],
        )?;

        Ok(())
    }

    /// Get a reaction by ID.
    pub fn get(&self, id: &ReactionId) -> Result<Option<Reaction>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, sender_id, sent_at_ns, user_id, emoji_json, created_at
             FROM reactions WHERE id = ?1",
        )?;

        let result = stmt.query_row(params![id.to_string()], row_to_reaction);

        match result {
            Ok(reaction) => Ok(Some(reaction?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Delete a reaction by ID.
    pub fn delete(&self, id: &ReactionId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "DELETE FROM reactions WHERE id = ?1",
            params![id.to_string()],
        )?;

        if rows == 0 {
            return Err(StorageError::NotFound(format!("Reaction {} not found", id)));
        }

        Ok(())
    }

    // ========================================================================
    // Queries
    // ========================================================================

    /// List all reactions on a message identified by (sender_id, sent_at_ns).
    pub fn list_for_message(
        &self,
        sender_id: &UserId,
        sent_at_ns: u64,
    ) -> Result<Vec<Reaction>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, sender_id, sent_at_ns, user_id, emoji_json, created_at
             FROM reactions WHERE sender_id = ?1 AND sent_at_ns = ?2
             ORDER BY created_at ASC",
        )?;

        let mut rows = stmt.query(params![sender_id.to_string(), sent_at_ns as i64])?;
        collect_reactions(&mut rows)
    }

    /// List a user's reactions on a specific message.
    pub fn list_user_reactions_on_message(
        &self,
        sender_id: &UserId,
        sent_at_ns: u64,
        user_id: &UserId,
    ) -> Result<Vec<Reaction>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, sender_id, sent_at_ns, user_id, emoji_json, created_at
             FROM reactions WHERE sender_id = ?1 AND sent_at_ns = ?2 AND user_id = ?3
             ORDER BY created_at ASC",
        )?;

        let mut rows = stmt.query(params![
            sender_id.to_string(),
            sent_at_ns as i64,
            user_id.to_string(),
        ])?;
        collect_reactions(&mut rows)
    }

    /// Toggle a reaction: add if not exists, remove if exists.
    ///
    /// Returns `true` if the reaction was added, `false` if removed.
    pub fn toggle(
        &self,
        sender_id: &UserId,
        sent_at_ns: u64,
        user_id: &UserId,
        emoji: &Emoji,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let emoji_json = serde_json::to_string(emoji)
            .map_err(|e| StorageError::InvalidData(format!("Failed to serialize emoji: {}", e)))?;

        // Try to find existing reaction with matching (sender_id, sent_at_ns, user_id, emoji_json)
        let existing_id: Option<String> = conn
            .query_row(
                "SELECT id FROM reactions
                 WHERE sender_id = ?1 AND sent_at_ns = ?2 AND user_id = ?3 AND emoji_json = ?4",
                params![
                    sender_id.to_string(),
                    sent_at_ns as i64,
                    user_id.to_string(),
                    emoji_json,
                ],
                |row| row.get(0),
            )
            .ok();

        if let Some(existing_id) = existing_id {
            // Exists: delete and return false
            conn.execute("DELETE FROM reactions WHERE id = ?1", params![existing_id])?;
            Ok(false)
        } else {
            // Doesn't exist: insert new reaction and return true
            let reaction = Reaction::new(*sender_id, sent_at_ns, *user_id, emoji.clone());
            let new_emoji_json = serde_json::to_string(&reaction.emoji)
                .map_err(|e| {
                    StorageError::InvalidData(format!("Failed to serialize emoji: {}", e))
                })?;

            conn.execute(
                "INSERT INTO reactions (id, sender_id, sent_at_ns, user_id, emoji_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    reaction.id.to_string(),
                    reaction.sender_id.to_string(),
                    reaction.sent_at_ns as i64,
                    reaction.user_id.to_string(),
                    new_emoji_json,
                    reaction.created_at.to_rfc3339(),
                ],
            )?;
            Ok(true)
        }
    }

    // ========================================================================
    // Aggregation
    // ========================================================================

    /// Get reaction counts per emoji for a message.
    pub fn get_reaction_counts(
        &self,
        sender_id: &UserId,
        sent_at_ns: u64,
    ) -> Result<Vec<ReactionCount>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT emoji_json, COUNT(*) as count
             FROM reactions
             WHERE sender_id = ?1 AND sent_at_ns = ?2
             GROUP BY emoji_json",
        )?;

        let mut rows = stmt.query(params![sender_id.to_string(), sent_at_ns as i64])?;
        let mut counts = Vec::new();

        while let Some(row) = rows.next()? {
            let emoji_json: String = row.get(0)?;
            let count: u32 = row.get(1)?;

            // Deserialize emoji to extract the value for ReactionCount
            let emoji: Emoji = serde_json::from_str(&emoji_json).map_err(|e| {
                StorageError::InvalidData(format!("Invalid emoji JSON: {}", e))
            })?;

            counts.push(ReactionCount::new(emoji.value, count));
        }

        Ok(counts)
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

/// Convert a database row to Reaction.
fn row_to_reaction(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Reaction, StorageError>> {
    let id_str: String = row.get(0)?;
    let sender_id_str: String = row.get(1)?;
    let sent_at_ns: i64 = row.get(2)?;
    let user_id_str: String = row.get(3)?;
    let emoji_json: String = row.get(4)?;
    let created_at_str: String = row.get(5)?;

    let id: ReactionId = match id_str.parse() {
        Ok(id) => id,
        Err(e) => {
            return Ok(Err(StorageError::InvalidData(format!(
                "Invalid reaction ID: {}",
                e
            ))))
        }
    };

    let sender_id: UserId = match sender_id_str.parse() {
        Ok(id) => id,
        Err(e) => {
            return Ok(Err(StorageError::InvalidData(format!(
                "Invalid sender ID: {}",
                e
            ))))
        }
    };

    let user_id: UserId = match user_id_str.parse() {
        Ok(id) => id,
        Err(e) => {
            return Ok(Err(StorageError::InvalidData(format!(
                "Invalid user ID: {}",
                e
            ))))
        }
    };

    let emoji: Emoji = match serde_json::from_str(&emoji_json) {
        Ok(e) => e,
        Err(e) => {
            return Ok(Err(StorageError::InvalidData(format!(
                "Invalid emoji JSON: {}",
                e
            ))))
        }
    };

    Ok(Ok(Reaction {
        id,
        sender_id,
        sent_at_ns: sent_at_ns as u64,
        user_id,
        emoji,
        created_at: parse_datetime(&created_at_str),
    }))
}

/// Collect reactions from a Rows iterator.
fn collect_reactions(rows: &mut rusqlite::Rows<'_>) -> Result<Vec<Reaction>, StorageError> {
    let mut reactions = Vec::new();
    while let Some(row) = rows.next()? {
        let reaction_result = row_to_reaction(row)?;
        reactions.push(reaction_result?);
    }
    Ok(reactions)
}
