//! Message repository.
//!
//! Data access for message and thread entities.

use crate::domain::ids::{AttachmentId, ChannelId, IntegrationId, MessageId, UserId};
use crate::domain::message::{Message, MessageContent, ReactionCount, Thread};
use crate::error::StorageError;
use crate::storage::Database;
use chrono::{DateTime, Utc};
use rusqlite::params;
use std::sync::Arc;

/// Message data repository.
///
/// Provides CRUD operations for messages and threads with cursor-based pagination.
pub struct MessageRepository {
    db: Arc<Database>,
}

impl MessageRepository {
    /// Create a new MessageRepository with the given database.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    // ========================================================================
    // Message CRUD
    // ========================================================================

    /// Insert a new message.
    pub fn insert(&self, message: &Message) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let content_json = serde_json::to_string(&message.content)
            .map_err(|e| StorageError::InvalidData(format!("Failed to serialize content: {}", e)))?;
        let attachments_json = serde_json::to_string(&message.attachments)
            .map_err(|e| StorageError::InvalidData(format!("Failed to serialize attachments: {}", e)))?;
        let reaction_counts_json = serde_json::to_string(&message.reaction_counts)
            .map_err(|e| StorageError::InvalidData(format!("Failed to serialize reactions: {}", e)))?;
        let current_user_reactions_json = serde_json::to_string(&message.current_user_reactions)
            .map_err(|e| StorageError::InvalidData(format!("Failed to serialize user reactions: {}", e)))?;
        let mentioned_integrations_json = serde_json::to_string(&message.mentioned_integrations)
            .map_err(|e| StorageError::InvalidData(format!("Failed to serialize integrations: {}", e)))?;

        conn.execute(
            "INSERT INTO messages (
                id, channel_id, sender_id, content_type, content_json,
                thread_id, reply_to, attachments_json,
                is_pinned, is_edited, is_deleted,
                created_at, updated_at, edited_at,
                reaction_counts_json, current_user_reactions_json,
                mentioned_integrations_json, targets_ai_bot
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            params![
                message.id.to_string(),
                message.channel_id.to_string(),
                message.sender_id.map(|id| id.to_string()),
                content_type_discriminant(&message.content),
                content_json,
                message.thread_id.map(|id| id.to_string()),
                message.reply_to.map(|id| id.to_string()),
                attachments_json,
                message.is_pinned as i32,
                message.is_edited as i32,
                message.is_deleted as i32,
                message.created_at.to_rfc3339(),
                message.updated_at.to_rfc3339(),
                message.edited_at.map(|dt| dt.to_rfc3339()),
                reaction_counts_json,
                current_user_reactions_json,
                mentioned_integrations_json,
                message.targets_ai_bot as i32,
            ],
        )?;
        Ok(())
    }

    /// Insert a new message, ignoring if it already exists (for deduplication).
    ///
    /// Returns `true` if the message was inserted, `false` if it was skipped (duplicate).
    pub fn insert_or_ignore(&self, message: &Message) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let content_json = serde_json::to_string(&message.content)
            .map_err(|e| StorageError::InvalidData(format!("Failed to serialize content: {}", e)))?;
        let attachments_json = serde_json::to_string(&message.attachments)
            .map_err(|e| StorageError::InvalidData(format!("Failed to serialize attachments: {}", e)))?;
        let reaction_counts_json = serde_json::to_string(&message.reaction_counts)
            .map_err(|e| StorageError::InvalidData(format!("Failed to serialize reactions: {}", e)))?;
        let current_user_reactions_json = serde_json::to_string(&message.current_user_reactions)
            .map_err(|e| StorageError::InvalidData(format!("Failed to serialize user reactions: {}", e)))?;
        let mentioned_integrations_json = serde_json::to_string(&message.mentioned_integrations)
            .map_err(|e| StorageError::InvalidData(format!("Failed to serialize integrations: {}", e)))?;

        let rows = conn.execute(
            "INSERT OR IGNORE INTO messages (
                id, channel_id, sender_id, content_type, content_json,
                thread_id, reply_to, attachments_json,
                is_pinned, is_edited, is_deleted,
                created_at, updated_at, edited_at,
                reaction_counts_json, current_user_reactions_json,
                mentioned_integrations_json, targets_ai_bot
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            params![
                message.id.to_string(),
                message.channel_id.to_string(),
                message.sender_id.map(|id| id.to_string()),
                content_type_discriminant(&message.content),
                content_json,
                message.thread_id.map(|id| id.to_string()),
                message.reply_to.map(|id| id.to_string()),
                attachments_json,
                message.is_pinned as i32,
                message.is_edited as i32,
                message.is_deleted as i32,
                message.created_at.to_rfc3339(),
                message.updated_at.to_rfc3339(),
                message.edited_at.map(|dt| dt.to_rfc3339()),
                reaction_counts_json,
                current_user_reactions_json,
                mentioned_integrations_json,
                message.targets_ai_bot as i32,
            ],
        )?;
        Ok(rows > 0)
    }

    /// Get a message by ID.
    pub fn get(&self, id: &MessageId) -> Result<Option<Message>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, channel_id, sender_id, content_json,
                    thread_id, reply_to, attachments_json,
                    is_pinned, is_edited, is_deleted,
                    created_at, updated_at, edited_at,
                    reaction_counts_json, current_user_reactions_json,
                    mentioned_integrations_json, targets_ai_bot
             FROM messages WHERE id = ?1",
        )?;

        let result = stmt.query_row(params![id.to_string()], row_to_message);

        match result {
            Ok(message) => Ok(Some(message?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Update an existing message.
    pub fn update(&self, message: &Message) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let content_json = serde_json::to_string(&message.content)
            .map_err(|e| StorageError::InvalidData(format!("Failed to serialize content: {}", e)))?;
        let attachments_json = serde_json::to_string(&message.attachments)
            .map_err(|e| StorageError::InvalidData(format!("Failed to serialize attachments: {}", e)))?;
        let reaction_counts_json = serde_json::to_string(&message.reaction_counts)
            .map_err(|e| StorageError::InvalidData(format!("Failed to serialize reactions: {}", e)))?;
        let current_user_reactions_json = serde_json::to_string(&message.current_user_reactions)
            .map_err(|e| StorageError::InvalidData(format!("Failed to serialize user reactions: {}", e)))?;
        let mentioned_integrations_json = serde_json::to_string(&message.mentioned_integrations)
            .map_err(|e| StorageError::InvalidData(format!("Failed to serialize integrations: {}", e)))?;

        let rows = conn.execute(
            "UPDATE messages SET
                content_type = ?1, content_json = ?2,
                thread_id = ?3, reply_to = ?4, attachments_json = ?5,
                is_pinned = ?6, is_edited = ?7, is_deleted = ?8,
                updated_at = ?9, edited_at = ?10,
                reaction_counts_json = ?11, current_user_reactions_json = ?12,
                mentioned_integrations_json = ?13, targets_ai_bot = ?14
             WHERE id = ?15",
            params![
                content_type_discriminant(&message.content),
                content_json,
                message.thread_id.map(|id| id.to_string()),
                message.reply_to.map(|id| id.to_string()),
                attachments_json,
                message.is_pinned as i32,
                message.is_edited as i32,
                message.is_deleted as i32,
                message.updated_at.to_rfc3339(),
                message.edited_at.map(|dt| dt.to_rfc3339()),
                reaction_counts_json,
                current_user_reactions_json,
                mentioned_integrations_json,
                message.targets_ai_bot as i32,
                message.id.to_string(),
            ],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("Message {}", message.id)));
        }
        Ok(())
    }

    /// Delete a message by ID.
    pub fn delete(&self, id: &MessageId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute("DELETE FROM messages WHERE id = ?1", params![id.to_string()])?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("Message {}", id)));
        }
        Ok(())
    }

    // ========================================================================
    // Message queries
    // ========================================================================

    /// List messages for a channel with cursor-based pagination.
    ///
    /// Returns messages in reverse chronological order (newest first).
    /// Use `before` to paginate - pass the last message's ID to get the next page.
    pub fn list_for_channel(
        &self,
        channel_id: &ChannelId,
        before: Option<&MessageId>,
        limit: u32,
    ) -> Result<Vec<Message>, StorageError> {
        let conn = self.db.connection()?;

        let messages = if let Some(before_id) = before {
            // Cursor-based pagination using (created_at, id) tuple
            let mut stmt = conn.prepare(
                "SELECT m.id, m.channel_id, m.sender_id, m.content_json,
                        m.thread_id, m.reply_to, m.attachments_json,
                        m.is_pinned, m.is_edited, m.is_deleted,
                        m.created_at, m.updated_at, m.edited_at,
                        m.reaction_counts_json, m.current_user_reactions_json,
                        m.mentioned_integrations_json, m.targets_ai_bot
                 FROM messages m
                 WHERE m.channel_id = ?1
                   AND (m.created_at, m.id) < (SELECT created_at, id FROM messages WHERE id = ?2)
                 ORDER BY m.created_at DESC, m.id DESC
                 LIMIT ?3",
            )?;
            let mut rows = stmt.query(params![channel_id.to_string(), before_id.to_string(), limit])?;
            collect_messages(&mut rows)?
        } else {
            // First page - no cursor
            let mut stmt = conn.prepare(
                "SELECT id, channel_id, sender_id, content_json,
                        thread_id, reply_to, attachments_json,
                        is_pinned, is_edited, is_deleted,
                        created_at, updated_at, edited_at,
                        reaction_counts_json, current_user_reactions_json,
                        mentioned_integrations_json, targets_ai_bot
                 FROM messages
                 WHERE channel_id = ?1
                 ORDER BY created_at DESC, id DESC
                 LIMIT ?2",
            )?;
            let mut rows = stmt.query(params![channel_id.to_string(), limit])?;
            collect_messages(&mut rows)?
        };

        Ok(messages)
    }

    /// List messages in a thread with cursor-based pagination.
    pub fn list_for_thread(
        &self,
        thread_id: &MessageId,
        before: Option<&MessageId>,
        limit: u32,
    ) -> Result<Vec<Message>, StorageError> {
        let conn = self.db.connection()?;

        let messages = if let Some(before_id) = before {
            let mut stmt = conn.prepare(
                "SELECT m.id, m.channel_id, m.sender_id, m.content_json,
                        m.thread_id, m.reply_to, m.attachments_json,
                        m.is_pinned, m.is_edited, m.is_deleted,
                        m.created_at, m.updated_at, m.edited_at,
                        m.reaction_counts_json, m.current_user_reactions_json,
                        m.mentioned_integrations_json, m.targets_ai_bot
                 FROM messages m
                 WHERE m.thread_id = ?1
                   AND (m.created_at, m.id) < (SELECT created_at, id FROM messages WHERE id = ?2)
                 ORDER BY m.created_at DESC, m.id DESC
                 LIMIT ?3",
            )?;
            let mut rows = stmt.query(params![thread_id.to_string(), before_id.to_string(), limit])?;
            collect_messages(&mut rows)?
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, channel_id, sender_id, content_json,
                        thread_id, reply_to, attachments_json,
                        is_pinned, is_edited, is_deleted,
                        created_at, updated_at, edited_at,
                        reaction_counts_json, current_user_reactions_json,
                        mentioned_integrations_json, targets_ai_bot
                 FROM messages
                 WHERE thread_id = ?1
                 ORDER BY created_at DESC, id DESC
                 LIMIT ?2",
            )?;
            let mut rows = stmt.query(params![thread_id.to_string(), limit])?;
            collect_messages(&mut rows)?
        };

        Ok(messages)
    }

    /// List all pinned messages in a channel.
    pub fn list_pinned(&self, channel_id: &ChannelId) -> Result<Vec<Message>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, channel_id, sender_id, content_json,
                    thread_id, reply_to, attachments_json,
                    is_pinned, is_edited, is_deleted,
                    created_at, updated_at, edited_at,
                    reaction_counts_json, current_user_reactions_json,
                    mentioned_integrations_json, targets_ai_bot
             FROM messages
             WHERE channel_id = ?1 AND is_pinned = 1
             ORDER BY created_at DESC",
        )?;
        let mut rows = stmt.query(params![channel_id.to_string()])?;
        collect_messages(&mut rows)
    }

    /// Count messages in a channel.
    pub fn count_in_channel(&self, channel_id: &ChannelId) -> Result<u64, StorageError> {
        let conn = self.db.connection()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE channel_id = ?1",
            params![channel_id.to_string()],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    // ========================================================================
    // Thread operations
    // ========================================================================

    /// Get thread metadata.
    pub fn get_thread(&self, root_message_id: &MessageId) -> Result<Option<Thread>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT root_message_id, channel_id, reply_count, last_reply_at, participant_ids_json
             FROM threads WHERE root_message_id = ?1",
        )?;

        let result = stmt.query_row(params![root_message_id.to_string()], |row| {
            row_to_thread(row)
        });

        match result {
            Ok(thread) => Ok(Some(thread?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Insert or update thread metadata.
    pub fn upsert_thread(&self, thread: &Thread) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let participant_ids_json = serde_json::to_string(&thread.participant_ids)
            .map_err(|e| StorageError::InvalidData(format!("Failed to serialize participants: {}", e)))?;

        conn.execute(
            "INSERT INTO threads (root_message_id, channel_id, reply_count, last_reply_at, participant_ids_json)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(root_message_id) DO UPDATE SET
                reply_count = excluded.reply_count,
                last_reply_at = excluded.last_reply_at,
                participant_ids_json = excluded.participant_ids_json",
            params![
                thread.root_message_id.to_string(),
                thread.channel_id.to_string(),
                thread.reply_count,
                thread.last_reply_at.map(|dt| dt.to_rfc3339()),
                participant_ids_json,
            ],
        )?;
        Ok(())
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

/// Get content type discriminant for filtering.
fn content_type_discriminant(content: &MessageContent) -> i32 {
    match content {
        MessageContent::Text(_) => 0,
        MessageContent::Encrypted(_) => 1,
        MessageContent::SealedSender(_) => 2,
        MessageContent::System(_) => 3,
        MessageContent::Call(_) => 4,
        MessageContent::AI(_) => 5,
        MessageContent::Mixed(_) => 6,
    }
}

/// Convert a row to a Message.
fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Message, StorageError>> {
    let content_json: String = row.get(3)?;
    let attachments_json: String = row.get(6)?;
    let reaction_counts_json: String = row.get(13)?;
    let current_user_reactions_json: String = row.get(14)?;
    let mentioned_integrations_json: String = row.get(15)?;

    let thread_id: Option<String> = row.get(4)?;
    let reply_to: Option<String> = row.get(5)?;
    let sender_id: Option<String> = row.get(2)?;
    let edited_at: Option<String> = row.get(12)?;

    // Deserialize JSON fields
    let content: MessageContent = match serde_json::from_str(&content_json) {
        Ok(c) => c,
        Err(e) => return Ok(Err(StorageError::InvalidData(format!("Invalid content JSON: {}", e)))),
    };

    let attachments: Vec<AttachmentId> = match serde_json::from_str(&attachments_json) {
        Ok(a) => a,
        Err(e) => return Ok(Err(StorageError::InvalidData(format!("Invalid attachments JSON: {}", e)))),
    };

    let reaction_counts: Vec<ReactionCount> = match serde_json::from_str(&reaction_counts_json) {
        Ok(r) => r,
        Err(e) => return Ok(Err(StorageError::InvalidData(format!("Invalid reaction counts JSON: {}", e)))),
    };

    let current_user_reactions: Vec<String> = match serde_json::from_str(&current_user_reactions_json) {
        Ok(r) => r,
        Err(e) => return Ok(Err(StorageError::InvalidData(format!("Invalid user reactions JSON: {}", e)))),
    };

    let mentioned_integrations: Vec<IntegrationId> = match serde_json::from_str(&mentioned_integrations_json) {
        Ok(m) => m,
        Err(e) => return Ok(Err(StorageError::InvalidData(format!("Invalid integrations JSON: {}", e)))),
    };

    Ok(Ok(Message {
        id: row
            .get::<_, String>(0)?
            .parse()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        channel_id: row
            .get::<_, String>(1)?
            .parse()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        sender_id: sender_id
            .map(|s| s.parse())
            .transpose()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        content,
        thread_id: thread_id
            .map(|s| s.parse())
            .transpose()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        reply_to: reply_to
            .map(|s| s.parse())
            .transpose()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        attachments,
        is_pinned: row.get::<_, i32>(7)? != 0,
        is_edited: row.get::<_, i32>(8)? != 0,
        is_deleted: row.get::<_, i32>(9)? != 0,
        created_at: parse_datetime(&row.get::<_, String>(10)?),
        updated_at: parse_datetime(&row.get::<_, String>(11)?),
        edited_at: edited_at.map(|s| parse_datetime(&s)),
        reaction_counts,
        current_user_reactions,
        mentioned_integrations,
        targets_ai_bot: row.get::<_, i32>(16)? != 0,
    }))
}

/// Convert a row to a Thread.
fn row_to_thread(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Thread, StorageError>> {
    let participant_ids_json: String = row.get(4)?;
    let last_reply_at: Option<String> = row.get(3)?;

    let participant_ids: Vec<UserId> = match serde_json::from_str(&participant_ids_json) {
        Ok(p) => p,
        Err(e) => return Ok(Err(StorageError::InvalidData(format!("Invalid participants JSON: {}", e)))),
    };

    Ok(Ok(Thread {
        root_message_id: row
            .get::<_, String>(0)?
            .parse()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        channel_id: row
            .get::<_, String>(1)?
            .parse()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        reply_count: row.get(2)?,
        last_reply_at: last_reply_at.map(|s| parse_datetime(&s)),
        participant_ids,
    }))
}

/// Collect messages from a Rows iterator.
fn collect_messages(rows: &mut rusqlite::Rows<'_>) -> Result<Vec<Message>, StorageError> {
    let mut messages = Vec::new();
    while let Some(row) = rows.next()? {
        let msg_result = row_to_message(row)?;
        messages.push(msg_result?);
    }
    Ok(messages)
}
