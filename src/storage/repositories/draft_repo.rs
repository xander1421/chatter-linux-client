//! Draft repository.
//!
//! Data access for draft entities.

use crate::domain::draft::{Draft, DraftKey};
use crate::domain::ids::{AttachmentId, ChannelId, DraftId, MessageId, UserId};
use crate::error::StorageError;
use crate::storage::Database;
use chrono::{DateTime, Utc};
use rusqlite::params;
use std::sync::Arc;

/// Draft data repository.
pub struct DraftRepository {
    db: Arc<Database>,
}

impl DraftRepository {
    /// Create a new DraftRepository.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    // ========================================================================
    // Draft CRUD
    // ========================================================================

    /// Insert or update a draft by its key.
    ///
    /// Uses upsert semantics: if a draft exists for this channel+thread,
    /// it will be updated; otherwise a new draft will be created.
    pub fn upsert(&self, draft: &Draft) -> Result<(), StorageError> {
        let conn = self.db.connection()?;

        let attachments_json = serde_json::to_string(&draft.attachments)
            .map_err(|e| StorageError::InvalidData(format!("Failed to serialize attachments: {}", e)))?;

        let mentions_json = serde_json::to_string(&draft.mentions)
            .map_err(|e| StorageError::InvalidData(format!("Failed to serialize mentions: {}", e)))?;

        let thread_id_str = draft.key.thread_id.as_ref().map(|id| id.to_string());
        let reply_to_str = draft.reply_to.as_ref().map(|id| id.to_string());

        conn.execute(
            "INSERT INTO drafts (id, channel_id, thread_id, content, reply_to, attachments_json, mentions_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(channel_id, COALESCE(thread_id, '')) DO UPDATE SET
                 content = excluded.content,
                 reply_to = excluded.reply_to,
                 attachments_json = excluded.attachments_json,
                 mentions_json = excluded.mentions_json,
                 updated_at = excluded.updated_at",
            params![
                draft.id.to_string(),
                draft.key.channel_id.to_string(),
                thread_id_str,
                draft.content,
                reply_to_str,
                attachments_json,
                mentions_json,
                draft.created_at.to_rfc3339(),
                draft.updated_at.to_rfc3339(),
            ],
        )?;

        Ok(())
    }

    /// Get a draft by ID.
    pub fn get(&self, id: &DraftId) -> Result<Option<Draft>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, channel_id, thread_id, content, reply_to, attachments_json, mentions_json, created_at, updated_at
             FROM drafts WHERE id = ?1",
        )?;

        let result = stmt.query_row(params![id.to_string()], row_to_draft);

        match result {
            Ok(draft) => Ok(Some(draft?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Get a draft by its key (channel + optional thread).
    pub fn get_by_key(&self, key: &DraftKey) -> Result<Option<Draft>, StorageError> {
        let conn = self.db.connection()?;
        let thread_id_str = key.thread_id.as_ref().map(|id| id.to_string());

        let mut stmt = conn.prepare(
            "SELECT id, channel_id, thread_id, content, reply_to, attachments_json, mentions_json, created_at, updated_at
             FROM drafts
             WHERE channel_id = ?1 AND COALESCE(thread_id, '') = COALESCE(?2, '')",
        )?;

        let result = stmt.query_row(params![key.channel_id.to_string(), thread_id_str], row_to_draft);

        match result {
            Ok(draft) => Ok(Some(draft?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Delete a draft by ID.
    pub fn delete(&self, id: &DraftId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "DELETE FROM drafts WHERE id = ?1",
            params![id.to_string()],
        )?;

        if rows == 0 {
            return Err(StorageError::NotFound(format!("Draft {} not found", id)));
        }

        Ok(())
    }

    /// Delete a draft by its key.
    pub fn delete_by_key(&self, key: &DraftKey) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let thread_id_str = key.thread_id.as_ref().map(|id| id.to_string());

        let rows = conn.execute(
            "DELETE FROM drafts
             WHERE channel_id = ?1 AND COALESCE(thread_id, '') = COALESCE(?2, '')",
            params![key.channel_id.to_string(), thread_id_str],
        )?;

        if rows == 0 {
            return Err(StorageError::NotFound(format!(
                "Draft for channel {} not found",
                key.channel_id
            )));
        }

        Ok(())
    }

    // ========================================================================
    // Queries
    // ========================================================================

    /// List all drafts for a channel (including thread drafts).
    pub fn list_for_channel(&self, channel_id: &ChannelId) -> Result<Vec<Draft>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, channel_id, thread_id, content, reply_to, attachments_json, mentions_json, created_at, updated_at
             FROM drafts WHERE channel_id = ?1
             ORDER BY updated_at DESC",
        )?;

        let mut rows = stmt.query(params![channel_id.to_string()])?;
        collect_drafts(&mut rows)
    }

    /// List all drafts (for sync/backup).
    pub fn list_all(&self) -> Result<Vec<Draft>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, channel_id, thread_id, content, reply_to, attachments_json, mentions_json, created_at, updated_at
             FROM drafts
             ORDER BY updated_at DESC",
        )?;

        let mut rows = stmt.query([])?;
        collect_drafts(&mut rows)
    }

    /// Count total number of drafts.
    pub fn count(&self) -> Result<u64, StorageError> {
        let conn = self.db.connection()?;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM drafts", [], |row| row.get(0))?;
        Ok(count as u64)
    }

    // ========================================================================
    // Cleanup
    // ========================================================================

    /// Delete empty drafts (no content and no attachments).
    ///
    /// Returns the number of drafts deleted.
    pub fn delete_empty_drafts(&self) -> Result<u64, StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "DELETE FROM drafts WHERE TRIM(content) = '' AND attachments_json = '[]'",
            [],
        )?;
        Ok(rows as u64)
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

/// Convert a database row to Draft.
fn row_to_draft(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Draft, StorageError>> {
    let id_str: String = row.get(0)?;
    let channel_id_str: String = row.get(1)?;
    let thread_id_str: Option<String> = row.get(2)?;
    let content: String = row.get(3)?;
    let reply_to_str: Option<String> = row.get(4)?;
    let attachments_json: String = row.get(5)?;
    let mentions_json: String = row.get(6)?;
    let created_at_str: String = row.get(7)?;
    let updated_at_str: String = row.get(8)?;

    // Parse ID
    let id: DraftId = match id_str.parse() {
        Ok(id) => id,
        Err(e) => {
            return Ok(Err(StorageError::InvalidData(format!(
                "Invalid draft ID: {}",
                e
            ))))
        }
    };

    // Parse channel_id
    let channel_id: ChannelId = match channel_id_str.parse() {
        Ok(id) => id,
        Err(e) => {
            return Ok(Err(StorageError::InvalidData(format!(
                "Invalid channel ID: {}",
                e
            ))))
        }
    };

    // Parse thread_id (optional)
    let thread_id: Option<MessageId> = match thread_id_str {
        Some(s) => match s.parse() {
            Ok(id) => Some(id),
            Err(e) => {
                return Ok(Err(StorageError::InvalidData(format!(
                    "Invalid thread ID: {}",
                    e
                ))))
            }
        },
        None => None,
    };

    // Parse reply_to (optional)
    let reply_to: Option<MessageId> = match reply_to_str {
        Some(s) => match s.parse() {
            Ok(id) => Some(id),
            Err(e) => {
                return Ok(Err(StorageError::InvalidData(format!(
                    "Invalid reply_to ID: {}",
                    e
                ))))
            }
        },
        None => None,
    };

    // Parse attachments JSON
    let attachments: Vec<AttachmentId> = match serde_json::from_str(&attachments_json) {
        Ok(a) => a,
        Err(e) => {
            return Ok(Err(StorageError::InvalidData(format!(
                "Invalid attachments JSON: {}",
                e
            ))))
        }
    };

    // Parse mentions JSON
    let mentions: Vec<UserId> = match serde_json::from_str(&mentions_json) {
        Ok(m) => m,
        Err(e) => {
            return Ok(Err(StorageError::InvalidData(format!(
                "Invalid mentions JSON: {}",
                e
            ))))
        }
    };

    // Build DraftKey
    let key = match thread_id {
        Some(tid) => DraftKey::for_thread(channel_id, tid),
        None => DraftKey::for_channel(channel_id),
    };

    Ok(Ok(Draft {
        id,
        key,
        content,
        reply_to,
        attachments,
        mentions,
        created_at: parse_datetime(&created_at_str),
        updated_at: parse_datetime(&updated_at_str),
    }))
}

/// Collect drafts from a Rows iterator.
fn collect_drafts(rows: &mut rusqlite::Rows<'_>) -> Result<Vec<Draft>, StorageError> {
    let mut drafts = Vec::new();
    while let Some(row) = rows.next()? {
        let draft_result = row_to_draft(row)?;
        drafts.push(draft_result?);
    }
    Ok(drafts)
}
