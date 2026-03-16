//! Draft repository.
//!
//! Data access for draft entities.

use crate::domain::draft::{Draft, DraftKey};
use crate::domain::ids::{AttachmentId, ChannelId, DraftId, MessageId, UserId};
use crate::error::StorageError;
use crate::storage::serialization::{db_deserialize, db_serialize};
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

        // Use bincode for efficient BLOB storage
        let attachments_blob = db_serialize(&draft.attachments)?;
        let mentions_blob = db_serialize(&draft.mentions)?;

        let thread_id_str = draft.key.thread_id.as_ref().map(|id| id.to_string());
        let reply_to_str = draft.reply_to.as_ref().map(|id| id.to_string());

        conn.execute(
            "INSERT INTO drafts (id, channel_id, thread_id, content, reply_to, attachments_blob, mentions_blob, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(channel_id, COALESCE(thread_id, '')) DO UPDATE SET
                 content = excluded.content,
                 reply_to = excluded.reply_to,
                 attachments_blob = excluded.attachments_blob,
                 mentions_blob = excluded.mentions_blob,
                 updated_at = excluded.updated_at",
            params![
                draft.id.to_string(),
                draft.key.channel_id.to_string(),
                thread_id_str,
                draft.content,
                reply_to_str,
                attachments_blob,
                mentions_blob,
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
            "SELECT id, channel_id, thread_id, content, reply_to, attachments_blob, mentions_blob, created_at, updated_at
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
            "SELECT id, channel_id, thread_id, content, reply_to, attachments_blob, mentions_blob, created_at, updated_at
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
            "SELECT id, channel_id, thread_id, content, reply_to, attachments_blob, mentions_blob, created_at, updated_at
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
            "SELECT id, channel_id, thread_id, content, reply_to, attachments_blob, mentions_blob, created_at, updated_at
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
        // Empty Vec<AttachmentId> serialized with bincode is 8 bytes (u64 length = 0)
        let empty_blob = db_serialize::<Vec<AttachmentId>>(&vec![])?;
        let rows = conn.execute(
            "DELETE FROM drafts WHERE TRIM(content) = '' AND attachments_blob = ?1",
            params![empty_blob],
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
    // Read from BLOB columns (bincode serialized)
    let attachments_blob: Vec<u8> = row.get(5)?;
    let mentions_blob: Vec<u8> = row.get(6)?;
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

    // Deserialize BLOB fields from bincode
    let attachments: Vec<AttachmentId> = match db_deserialize(&attachments_blob) {
        Ok(a) => a,
        Err(e) => return Ok(Err(e)),
    };

    let mentions: Vec<UserId> = match db_deserialize(&mentions_blob) {
        Ok(m) => m,
        Err(e) => return Ok(Err(e)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::serialization::{db_deserialize, db_serialize};

    // ========================================================================
    // Bincode serialization tests for draft fields
    // ========================================================================

    #[test]
    fn test_bincode_attachments_roundtrip() {
        let attachments: Vec<AttachmentId> = vec![
            AttachmentId::new(),
            AttachmentId::new(),
            AttachmentId::new(),
        ];

        let blob = db_serialize(&attachments).unwrap();
        let deserialized: Vec<AttachmentId> = db_deserialize(&blob).unwrap();

        assert_eq!(attachments, deserialized);
    }

    #[test]
    fn test_bincode_empty_attachments() {
        let attachments: Vec<AttachmentId> = vec![];

        let blob = db_serialize(&attachments).unwrap();
        let deserialized: Vec<AttachmentId> = db_deserialize(&blob).unwrap();

        assert_eq!(attachments, deserialized);
    }

    #[test]
    fn test_bincode_mentions_roundtrip() {
        let mentions: Vec<UserId> = vec![
            UserId::new(),
            UserId::new(),
            UserId::new(),
        ];

        let blob = db_serialize(&mentions).unwrap();
        let deserialized: Vec<UserId> = db_deserialize(&blob).unwrap();

        assert_eq!(mentions, deserialized);
    }

    #[test]
    fn test_bincode_empty_mentions() {
        let mentions: Vec<UserId> = vec![];

        let blob = db_serialize(&mentions).unwrap();
        let deserialized: Vec<UserId> = db_deserialize(&blob).unwrap();

        assert_eq!(mentions, deserialized);
    }

    // ========================================================================
    // Size comparison tests
    // ========================================================================

    #[test]
    fn test_bincode_smaller_than_json_for_attachments() {
        let attachments: Vec<AttachmentId> = (0..20).map(|_| AttachmentId::new()).collect();

        let bincode_size = db_serialize(&attachments).unwrap().len();
        let json_size = serde_json::to_vec(&attachments).unwrap().len();

        // UUIDs are much more compact in bincode (16 bytes vs ~38 chars)
        assert!(bincode_size < json_size);
        assert!(bincode_size < json_size / 2, "Bincode should be at least 50% smaller for UUIDs");
    }

    #[test]
    fn test_bincode_smaller_than_json_for_mentions() {
        let mentions: Vec<UserId> = (0..20).map(|_| UserId::new()).collect();

        let bincode_size = db_serialize(&mentions).unwrap().len();
        let json_size = serde_json::to_vec(&mentions).unwrap().len();

        assert!(bincode_size < json_size);
        assert!(bincode_size < json_size / 2, "Bincode should be at least 50% smaller for UUIDs");
    }

    // ========================================================================
    // Edge case tests
    // ========================================================================

    #[test]
    fn test_bincode_large_attachment_list() {
        let attachments: Vec<AttachmentId> = (0..100).map(|_| AttachmentId::new()).collect();

        let blob = db_serialize(&attachments).unwrap();
        let deserialized: Vec<AttachmentId> = db_deserialize(&blob).unwrap();

        assert_eq!(attachments.len(), deserialized.len());
        assert_eq!(attachments, deserialized);
    }

    #[test]
    fn test_bincode_large_mentions_list() {
        let mentions: Vec<UserId> = (0..100).map(|_| UserId::new()).collect();

        let blob = db_serialize(&mentions).unwrap();
        let deserialized: Vec<UserId> = db_deserialize(&blob).unwrap();

        assert_eq!(mentions.len(), deserialized.len());
        assert_eq!(mentions, deserialized);
    }

    // ========================================================================
    // Empty draft detection (used in delete_empty_drafts)
    // ========================================================================

    #[test]
    fn test_empty_attachments_blob_is_consistent() {
        let empty1: Vec<AttachmentId> = vec![];
        let empty2: Vec<AttachmentId> = vec![];

        let blob1 = db_serialize(&empty1).unwrap();
        let blob2 = db_serialize(&empty2).unwrap();

        // Empty blobs should be identical
        assert_eq!(blob1, blob2);
    }

    #[test]
    fn test_empty_blob_size() {
        let empty: Vec<AttachmentId> = vec![];
        let blob = db_serialize(&empty).unwrap();

        // Empty vec should be just the length prefix (8 bytes for u64)
        assert_eq!(blob.len(), 8);
    }

    // ========================================================================
    // Error handling tests
    // ========================================================================

    #[test]
    fn test_bincode_invalid_attachments_blob() {
        let invalid_blob = vec![0xFF, 0x00, 0xFF, 0x00];

        let result: Result<Vec<AttachmentId>, _> = db_deserialize(&invalid_blob);
        assert!(result.is_err());
    }

    #[test]
    fn test_bincode_truncated_mentions_blob() {
        let mentions: Vec<UserId> = vec![UserId::new(), UserId::new()];
        let mut blob = db_serialize(&mentions).unwrap();
        blob.truncate(blob.len() / 2);

        let result: Result<Vec<UserId>, _> = db_deserialize(&blob);
        assert!(result.is_err());
    }

    // ========================================================================
    // Determinism tests
    // ========================================================================

    #[test]
    fn test_bincode_serialization_deterministic_attachments() {
        let attachments: Vec<AttachmentId> = vec![AttachmentId::new()];

        let blob1 = db_serialize(&attachments).unwrap();
        let blob2 = db_serialize(&attachments).unwrap();

        assert_eq!(blob1, blob2);
    }

    #[test]
    fn test_bincode_serialization_deterministic_mentions() {
        let mentions: Vec<UserId> = vec![UserId::new()];

        let blob1 = db_serialize(&mentions).unwrap();
        let blob2 = db_serialize(&mentions).unwrap();

        assert_eq!(blob1, blob2);
    }
}
