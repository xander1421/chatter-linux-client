//! Message repository.
//!
//! Data access for message and thread entities.

use crate::domain::ids::{AttachmentId, ChannelId, IntegrationId, MessageId, UserId};
use crate::domain::message::{Message, MessageContent, ReactionCount, Thread};
use crate::error::StorageError;
use crate::storage::serialization::{db_deserialize, db_serialize};
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
        tracing::debug!(
            message_id = %message.id,
            channel_id = %message.channel_id,
            sender_id = ?message.sender_id,
            content_type = ?std::mem::discriminant(&message.content),
            "[MESSAGE_REPO] Inserting message"
        );
        let conn = self.db.connection()?;
        // content_json kept as JSON for FTS5 json_extract() compatibility
        let content_json = serde_json::to_string(&message.content)
            .map_err(|e| StorageError::InvalidData(format!("Failed to serialize content: {}", e)))?;
        // Use bincode for efficient BLOB storage
        let attachments_blob = db_serialize(&message.attachments)?;
        let reaction_counts_blob = db_serialize(&message.reaction_counts)?;
        let current_user_reactions_blob = db_serialize(&message.current_user_reactions)?;
        let mentioned_integrations_blob = db_serialize(&message.mentioned_integrations)?;

        conn.execute(
            "INSERT INTO messages (
                id, channel_id, sender_id, content_type, content_json,
                thread_id, reply_to, attachments_blob,
                is_pinned, is_edited, is_deleted,
                sent_at, created_at, updated_at, edited_at,
                reaction_counts_blob, current_user_reactions_blob,
                mentioned_integrations_blob, targets_ai_bot, delivery_status
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
            params![
                message.id.to_string(),
                message.channel_id.to_string(),
                message.sender_id.map(|id| id.to_string()),
                content_type_discriminant(&message.content),
                content_json,
                message.thread_id.map(|id| id.to_string()),
                message.reply_to.map(|id| id.to_string()),
                attachments_blob,
                message.is_pinned as i32,
                message.is_edited as i32,
                message.is_deleted as i32,
                message.sent_at as i64,
                message.created_at.to_rfc3339(),
                message.updated_at.to_rfc3339(),
                message.edited_at.map(|dt| dt.to_rfc3339()),
                reaction_counts_blob,
                current_user_reactions_blob,
                mentioned_integrations_blob,
                message.targets_ai_bot as i32,
                &message.delivery_status,
            ],
        )?;
        tracing::debug!(
            message_id = %message.id,
            channel_id = %message.channel_id,
            "[MESSAGE_REPO] Message inserted successfully"
        );
        Ok(())
    }

    /// Insert a new message, ignoring if it already exists (for deduplication).
    ///
    /// Returns `true` if the message was inserted, `false` if it was skipped (duplicate).
    /// Deduplication is based on:
    /// - Primary key (message id)
    /// - Unique index on (sender_id, sent_at) for Signal-style dedup
    pub fn insert_or_ignore(&self, message: &Message) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        // content_json kept as JSON for FTS5 json_extract() compatibility
        let content_json = serde_json::to_string(&message.content)
            .map_err(|e| StorageError::InvalidData(format!("Failed to serialize content: {}", e)))?;
        // Use bincode for efficient BLOB storage
        let attachments_blob = db_serialize(&message.attachments)?;
        let reaction_counts_blob = db_serialize(&message.reaction_counts)?;
        let current_user_reactions_blob = db_serialize(&message.current_user_reactions)?;
        let mentioned_integrations_blob = db_serialize(&message.mentioned_integrations)?;

        let rows = conn.execute(
            "INSERT OR IGNORE INTO messages (
                id, channel_id, sender_id, content_type, content_json,
                thread_id, reply_to, attachments_blob,
                is_pinned, is_edited, is_deleted,
                sent_at, created_at, updated_at, edited_at,
                reaction_counts_blob, current_user_reactions_blob,
                mentioned_integrations_blob, targets_ai_bot
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            params![
                message.id.to_string(),
                message.channel_id.to_string(),
                message.sender_id.map(|id| id.to_string()),
                content_type_discriminant(&message.content),
                content_json,
                message.thread_id.map(|id| id.to_string()),
                message.reply_to.map(|id| id.to_string()),
                attachments_blob,
                message.is_pinned as i32,
                message.is_edited as i32,
                message.is_deleted as i32,
                message.sent_at as i64,
                message.created_at.to_rfc3339(),
                message.updated_at.to_rfc3339(),
                message.edited_at.map(|dt| dt.to_rfc3339()),
                reaction_counts_blob,
                current_user_reactions_blob,
                mentioned_integrations_blob,
                message.targets_ai_bot as i32,
            ],
        )?;
        Ok(rows > 0)
    }

    /// Get a message by ID.
    pub fn get(&self, id: &MessageId) -> Result<Option<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, channel_id, sender_id, content_json,
                    thread_id, reply_to, attachments_blob,
                    is_pinned, is_edited, is_deleted,
                    sent_at, created_at, updated_at, edited_at,
                    reaction_counts_blob, current_user_reactions_blob,
                    mentioned_integrations_blob, targets_ai_bot, delivery_status
             FROM messages WHERE id = ?1",
        )?;

        let result = stmt.query_row(params![id.to_string()], row_to_message);

        match result {
            Ok(message) => Ok(Some(message?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Get a message by sender ID and timestamp (Signal-style lookup).
    ///
    /// This is the preferred lookup method for reactions, edits, pins, and other
    /// message actions because it uses the Signal Protocol's (sender_id, sent_at)
    /// tuple which is consistent across devices, unlike server-assigned message IDs.
    pub fn get_by_sender_timestamp(
        &self,
        sender_id: &UserId,
        sent_at: u64,
    ) -> Result<Option<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, channel_id, sender_id, content_json,
                    thread_id, reply_to, attachments_blob,
                    is_pinned, is_edited, is_deleted,
                    sent_at, created_at, updated_at, edited_at,
                    reaction_counts_blob, current_user_reactions_blob,
                    mentioned_integrations_blob, targets_ai_bot, delivery_status
             FROM messages WHERE sender_id = ?1 AND sent_at = ?2",
        )?;

        let result = stmt.query_row(params![sender_id.to_string(), sent_at as i64], row_to_message);

        match result {
            Ok(message) => Ok(Some(message?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// List recent messages by sender (for debugging Signal-style lookups).
    pub fn list_by_sender(
        &self,
        sender_id: &UserId,
        limit: u32,
    ) -> Result<Vec<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, channel_id, sender_id, content_json,
                    thread_id, reply_to, attachments_blob,
                    is_pinned, is_edited, is_deleted,
                    sent_at, created_at, updated_at, edited_at,
                    reaction_counts_blob, current_user_reactions_blob,
                    mentioned_integrations_blob, targets_ai_bot, delivery_status
             FROM messages WHERE sender_id = ?1
             ORDER BY sent_at DESC LIMIT ?2",
        )?;

        let rows = stmt.query_map(params![sender_id.to_string(), limit], row_to_message)?;

        let mut messages = Vec::new();
        for row in rows {
            messages.push(row??);
        }
        Ok(messages)
    }

    /// Update an existing message.
    pub fn update(&self, message: &Message) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        // content_json kept as JSON for FTS5 json_extract() compatibility
        let content_json = serde_json::to_string(&message.content)
            .map_err(|e| StorageError::InvalidData(format!("Failed to serialize content: {}", e)))?;
        // Use bincode for efficient BLOB storage
        let attachments_blob = db_serialize(&message.attachments)?;
        let reaction_counts_blob = db_serialize(&message.reaction_counts)?;
        let current_user_reactions_blob = db_serialize(&message.current_user_reactions)?;
        let mentioned_integrations_blob = db_serialize(&message.mentioned_integrations)?;

        let rows = conn.execute(
            "UPDATE messages SET
                content_type = ?1, content_json = ?2,
                thread_id = ?3, reply_to = ?4, attachments_blob = ?5,
                is_pinned = ?6, is_edited = ?7, is_deleted = ?8,
                updated_at = ?9, edited_at = ?10,
                reaction_counts_blob = ?11, current_user_reactions_blob = ?12,
                mentioned_integrations_blob = ?13, targets_ai_bot = ?14
             WHERE id = ?15",
            params![
                content_type_discriminant(&message.content),
                content_json,
                message.thread_id.map(|id| id.to_string()),
                message.reply_to.map(|id| id.to_string()),
                attachments_blob,
                message.is_pinned as i32,
                message.is_edited as i32,
                message.is_deleted as i32,
                message.updated_at.to_rfc3339(),
                message.edited_at.map(|dt| dt.to_rfc3339()),
                reaction_counts_blob,
                current_user_reactions_blob,
                mentioned_integrations_blob,
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

    /// Soft-delete multiple messages by ID in a single query.
    ///
    /// More efficient than calling update() N times for expired ephemeral cleanup.
    /// Returns the number of messages updated.
    pub fn soft_delete_batch(&self, ids: &[MessageId]) -> Result<usize, StorageError> {
        if ids.is_empty() {
            return Ok(0);
        }

        let conn = self.db.connection()?;
        let now = Utc::now().to_rfc3339();

        // Build placeholders for IN clause
        let placeholders: Vec<String> = (1..=ids.len()).map(|i| format!("?{}", i)).collect();
        let in_clause = placeholders.join(", ");

        let query = format!(
            "UPDATE messages SET is_deleted = 1, updated_at = ?{} WHERE id IN ({})",
            ids.len() + 1,
            in_clause
        );

        // Build params: first all IDs, then the timestamp
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = ids
            .iter()
            .map(|id| Box::new(id.to_string()) as Box<dyn rusqlite::ToSql>)
            .collect();
        params_vec.push(Box::new(now));

        let rows = conn.execute(&query, rusqlite::params_from_iter(params_vec.iter().map(|b| b.as_ref())))?;
        Ok(rows)
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
        tracing::debug!(
            channel_id = %channel_id,
            before = ?before,
            limit = limit,
            "[MESSAGE_REPO] Listing messages for channel"
        );
        let conn = self.db.read_connection()?;

        let messages = if let Some(before_id) = before {
            // Cursor-based pagination using (sent_at, id) tuple for proper ordering
            // sent_at is nanosecond timestamp, ensuring correct order for rapid messages
            let mut stmt = conn.prepare(
                "SELECT m.id, m.channel_id, m.sender_id, m.content_json,
                        m.thread_id, m.reply_to, m.attachments_blob,
                        m.is_pinned, m.is_edited, m.is_deleted,
                        m.sent_at, m.created_at, m.updated_at, m.edited_at,
                        m.reaction_counts_blob, m.current_user_reactions_blob,
                        m.mentioned_integrations_blob, m.targets_ai_bot, m.delivery_status
                 FROM messages m
                 WHERE m.channel_id = ?1
                   AND (m.sent_at, m.sender_id) < (SELECT sent_at, sender_id FROM messages WHERE id = ?2)
                 ORDER BY m.sent_at DESC, m.sender_id DESC
                 LIMIT ?3",
            )?;
            let mut rows = stmt.query(params![channel_id.to_string(), before_id.to_string(), limit])?;
            collect_messages(&mut rows)?
        } else {
            // First page - no cursor
            // Use sent_at (nanosecond timestamp) for ordering to handle rapid messages
            let mut stmt = conn.prepare(
                "SELECT id, channel_id, sender_id, content_json,
                        thread_id, reply_to, attachments_blob,
                        is_pinned, is_edited, is_deleted,
                        sent_at, created_at, updated_at, edited_at,
                        reaction_counts_blob, current_user_reactions_blob,
                        mentioned_integrations_blob, targets_ai_bot, delivery_status
                 FROM messages
                 WHERE channel_id = ?1
                 ORDER BY sent_at DESC, sender_id DESC
                 LIMIT ?2",
            )?;
            let mut rows = stmt.query(params![channel_id.to_string(), limit])?;
            collect_messages(&mut rows)?
        };

        tracing::debug!(
            channel_id = %channel_id,
            message_count = messages.len(),
            "[MESSAGE_REPO] Retrieved {} messages for channel",
            messages.len()
        );
        Ok(messages)
    }

    /// List messages in a thread with cursor-based pagination.
    pub fn list_for_thread(
        &self,
        thread_id: &MessageId,
        before: Option<&MessageId>,
        limit: u32,
    ) -> Result<Vec<Message>, StorageError> {
        let conn = self.db.read_connection()?;

        let messages = if let Some(before_id) = before {
            let mut stmt = conn.prepare(
                "SELECT m.id, m.channel_id, m.sender_id, m.content_json,
                        m.thread_id, m.reply_to, m.attachments_blob,
                        m.is_pinned, m.is_edited, m.is_deleted,
                        m.sent_at, m.created_at, m.updated_at, m.edited_at,
                        m.reaction_counts_blob, m.current_user_reactions_blob,
                        m.mentioned_integrations_blob, m.targets_ai_bot, m.delivery_status
                 FROM messages m
                 WHERE m.thread_id = ?1
                   AND (m.sent_at, m.sender_id) < (SELECT sent_at, sender_id FROM messages WHERE id = ?2)
                 ORDER BY m.sent_at DESC, m.sender_id DESC
                 LIMIT ?3",
            )?;
            let mut rows = stmt.query(params![thread_id.to_string(), before_id.to_string(), limit])?;
            collect_messages(&mut rows)?
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, channel_id, sender_id, content_json,
                        thread_id, reply_to, attachments_blob,
                        is_pinned, is_edited, is_deleted,
                        sent_at, created_at, updated_at, edited_at,
                        reaction_counts_blob, current_user_reactions_blob,
                        mentioned_integrations_blob, targets_ai_bot, delivery_status
                 FROM messages
                 WHERE thread_id = ?1
                 ORDER BY sent_at DESC, sender_id DESC
                 LIMIT ?2",
            )?;
            let mut rows = stmt.query(params![thread_id.to_string(), limit])?;
            collect_messages(&mut rows)?
        };

        Ok(messages)
    }

    /// List all pinned messages in a channel.
    pub fn list_pinned(&self, channel_id: &ChannelId) -> Result<Vec<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, channel_id, sender_id, content_json,
                    thread_id, reply_to, attachments_blob,
                    is_pinned, is_edited, is_deleted,
                    sent_at, created_at, updated_at, edited_at,
                    reaction_counts_blob, current_user_reactions_blob,
                    mentioned_integrations_blob, targets_ai_bot, delivery_status
             FROM messages
             WHERE channel_id = ?1 AND is_pinned = 1
             ORDER BY sent_at DESC",
        )?;
        let mut rows = stmt.query(params![channel_id.to_string()])?;
        collect_messages(&mut rows)
    }

    /// Count messages in a channel.
    pub fn count_in_channel(&self, channel_id: &ChannelId) -> Result<u64, StorageError> {
        let conn = self.db.read_connection()?;
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
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT root_message_id, channel_id, reply_count, last_reply_at, participant_ids_blob
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

    /// Update delivery status for a message by sent_at timestamp.
    ///
    /// Used when receipts (delivered/read) are received.
    pub fn update_delivery_status(
        &self,
        channel_id: &ChannelId,
        sent_at: u64,
        status: &str,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE messages SET delivery_status = ?1, updated_at = ?2
             WHERE channel_id = ?3 AND sent_at = ?4",
            params![
                status,
                Utc::now().to_rfc3339(),
                channel_id.to_string(),
                sent_at as i64,
            ],
        )?;
        Ok(rows > 0)
    }

    /// Insert or update thread metadata.
    pub fn upsert_thread(&self, thread: &Thread) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        // Use bincode for efficient BLOB storage
        let participant_ids_blob = db_serialize(&thread.participant_ids)?;

        conn.execute(
            "INSERT INTO threads (root_message_id, channel_id, reply_count, last_reply_at, participant_ids_blob)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(root_message_id) DO UPDATE SET
                reply_count = excluded.reply_count,
                last_reply_at = excluded.last_reply_at,
                participant_ids_blob = excluded.participant_ids_blob",
            params![
                thread.root_message_id.to_string(),
                thread.channel_id.to_string(),
                thread.reply_count,
                thread.last_reply_at.map(|dt| dt.to_rfc3339()),
                participant_ids_blob,
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
/// Column order: id(0), channel_id(1), sender_id(2), content_json(3),
///               thread_id(4), reply_to(5), attachments_blob(6),
///               is_pinned(7), is_edited(8), is_deleted(9),
///               sent_at(10), created_at(11), updated_at(12), edited_at(13),
///               reaction_counts_blob(14), current_user_reactions_blob(15),
///               mentioned_integrations_blob(16), targets_ai_bot(17), delivery_status(18)
fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Message, StorageError>> {
    let content_json: String = row.get(3)?;
    // Read from BLOB columns (bincode serialized)
    let attachments_blob: Vec<u8> = row.get(6)?;
    let reaction_counts_blob: Vec<u8> = row.get(14)?;
    let current_user_reactions_blob: Vec<u8> = row.get(15)?;
    let mentioned_integrations_blob: Vec<u8> = row.get(16)?;

    let thread_id: Option<String> = row.get(4)?;
    let reply_to: Option<String> = row.get(5)?;
    let sender_id: Option<String> = row.get(2)?;
    let edited_at: Option<String> = row.get(13)?;

    // Deserialize content from JSON (kept for FTS5 compatibility)
    let content: MessageContent = match serde_json::from_str(&content_json) {
        Ok(c) => c,
        Err(e) => return Ok(Err(StorageError::InvalidData(format!("Invalid content JSON: {}", e)))),
    };

    // Deserialize BLOB fields from bincode
    let attachments: Vec<AttachmentId> = match db_deserialize(&attachments_blob) {
        Ok(a) => a,
        Err(e) => return Ok(Err(e)),
    };

    let reaction_counts: Vec<ReactionCount> = match db_deserialize(&reaction_counts_blob) {
        Ok(r) => r,
        Err(e) => return Ok(Err(e)),
    };

    let current_user_reactions: Vec<String> = match db_deserialize(&current_user_reactions_blob) {
        Ok(r) => r,
        Err(e) => return Ok(Err(e)),
    };

    let mentioned_integrations: Vec<IntegrationId> = match db_deserialize(&mentioned_integrations_blob) {
        Ok(m) => m,
        Err(e) => return Ok(Err(e)),
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
        sent_at: row.get::<_, i64>(10)? as u64,
        created_at: parse_datetime(&row.get::<_, String>(11)?),
        updated_at: parse_datetime(&row.get::<_, String>(12)?),
        edited_at: edited_at.map(|s| parse_datetime(&s)),
        reaction_counts,
        current_user_reactions,
        mentioned_integrations,
        targets_ai_bot: row.get::<_, i32>(17)? != 0,
        delivery_status: row.get::<_, String>(18).unwrap_or_default(),
    }))
}

/// Convert a row to a Thread.
fn row_to_thread(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Thread, StorageError>> {
    // Read from BLOB column (bincode serialized)
    let participant_ids_blob: Vec<u8> = row.get(4)?;
    let last_reply_at: Option<String> = row.get(3)?;

    // Deserialize BLOB field from bincode
    let participant_ids: Vec<UserId> = match db_deserialize(&participant_ids_blob) {
        Ok(p) => p,
        Err(e) => return Ok(Err(e)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::serialization::{db_deserialize, db_serialize};

    // ========================================================================
    // Bincode serialization unit tests
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
        // Empty vec should be small (just length prefix)
        assert!(blob.len() <= 8);
    }

    #[test]
    fn test_bincode_reaction_counts_roundtrip() {
        let reaction_counts: Vec<ReactionCount> = vec![
            ReactionCount { emoji: "👍".to_string(), count: 10 },
            ReactionCount { emoji: "❤️".to_string(), count: 5 },
            ReactionCount { emoji: "🎉".to_string(), count: 3 },
        ];

        let blob = db_serialize(&reaction_counts).unwrap();
        let deserialized: Vec<ReactionCount> = db_deserialize(&blob).unwrap();

        assert_eq!(reaction_counts, deserialized);
    }

    #[test]
    fn test_bincode_current_user_reactions_roundtrip() {
        let reactions: Vec<String> = vec!["👍".to_string(), "❤️".to_string()];

        let blob = db_serialize(&reactions).unwrap();
        let deserialized: Vec<String> = db_deserialize(&blob).unwrap();

        assert_eq!(reactions, deserialized);
    }

    #[test]
    fn test_bincode_mentioned_integrations_roundtrip() {
        let integrations: Vec<IntegrationId> = vec![
            IntegrationId::new(),
            IntegrationId::new(),
        ];

        let blob = db_serialize(&integrations).unwrap();
        let deserialized: Vec<IntegrationId> = db_deserialize(&blob).unwrap();

        assert_eq!(integrations, deserialized);
    }

    #[test]
    fn test_bincode_participant_ids_roundtrip() {
        let participants: Vec<UserId> = vec![
            UserId::new(),
            UserId::new(),
            UserId::new(),
        ];

        let blob = db_serialize(&participants).unwrap();
        let deserialized: Vec<UserId> = db_deserialize(&blob).unwrap();

        assert_eq!(participants, deserialized);
    }

    // ========================================================================
    // Size comparison tests
    // ========================================================================

    #[test]
    fn test_bincode_smaller_than_json_for_attachments() {
        let attachments: Vec<AttachmentId> = (0..50).map(|_| AttachmentId::new()).collect();

        let bincode_size = db_serialize(&attachments).unwrap().len();
        let json_size = serde_json::to_vec(&attachments).unwrap().len();

        // Bincode should be significantly smaller for UUIDs
        // JSON: 36 chars per UUID + quotes + commas = ~40 bytes per UUID
        // Bincode: 16 bytes per UUID (raw bytes)
        assert!(bincode_size < json_size);
        assert!(bincode_size < json_size / 2, "Bincode should be at least 50% smaller");
    }

    #[test]
    fn test_bincode_smaller_than_json_for_reactions() {
        let reaction_counts: Vec<ReactionCount> = vec![
            ReactionCount { emoji: "👍".to_string(), count: 100 },
            ReactionCount { emoji: "❤️".to_string(), count: 50 },
            ReactionCount { emoji: "😂".to_string(), count: 25 },
            ReactionCount { emoji: "🎉".to_string(), count: 10 },
            ReactionCount { emoji: "🔥".to_string(), count: 5 },
        ];

        let bincode_size = db_serialize(&reaction_counts).unwrap().len();
        let json_size = serde_json::to_vec(&reaction_counts).unwrap().len();

        assert!(bincode_size < json_size, "Bincode should be smaller than JSON");
    }

    // ========================================================================
    // Edge case tests
    // ========================================================================

    #[test]
    fn test_bincode_unicode_emoji_reactions() {
        let reaction_counts: Vec<ReactionCount> = vec![
            ReactionCount { emoji: "🇺🇸".to_string(), count: 1 },  // Flag emoji
            ReactionCount { emoji: "👨‍👩‍👧‍👦".to_string(), count: 2 },  // Family emoji (ZWJ sequence)
            ReactionCount { emoji: "👍🏽".to_string(), count: 3 },  // Skin tone modifier
        ];

        let blob = db_serialize(&reaction_counts).unwrap();
        let deserialized: Vec<ReactionCount> = db_deserialize(&blob).unwrap();

        assert_eq!(reaction_counts, deserialized);
    }

    #[test]
    fn test_bincode_large_attachment_list() {
        let attachments: Vec<AttachmentId> = (0..1000).map(|_| AttachmentId::new()).collect();

        let blob = db_serialize(&attachments).unwrap();
        let deserialized: Vec<AttachmentId> = db_deserialize(&blob).unwrap();

        assert_eq!(attachments.len(), deserialized.len());
        assert_eq!(attachments, deserialized);
    }

    #[test]
    fn test_bincode_high_reaction_counts() {
        let reaction_counts: Vec<ReactionCount> = vec![
            ReactionCount { emoji: "👍".to_string(), count: u32::MAX },
            ReactionCount { emoji: "❤️".to_string(), count: 1_000_000 },
        ];

        let blob = db_serialize(&reaction_counts).unwrap();
        let deserialized: Vec<ReactionCount> = db_deserialize(&blob).unwrap();

        assert_eq!(reaction_counts, deserialized);
    }

    // ========================================================================
    // Error handling tests
    // ========================================================================

    #[test]
    fn test_bincode_invalid_data_error() {
        let invalid_blob = vec![0xFF, 0xFF, 0xFF, 0xFF];

        let result: Result<Vec<AttachmentId>, _> = db_deserialize(&invalid_blob);
        assert!(result.is_err());
    }

    #[test]
    fn test_bincode_truncated_data_error() {
        let attachments: Vec<AttachmentId> = vec![AttachmentId::new(), AttachmentId::new()];
        let mut blob = db_serialize(&attachments).unwrap();

        // Truncate the blob
        blob.truncate(blob.len() / 2);

        let result: Result<Vec<AttachmentId>, _> = db_deserialize(&blob);
        assert!(result.is_err());
    }

    // ========================================================================
    // Determinism tests
    // ========================================================================

    #[test]
    fn test_bincode_serialization_deterministic() {
        let attachments: Vec<AttachmentId> = vec![
            AttachmentId::new(),
            AttachmentId::new(),
        ];

        let blob1 = db_serialize(&attachments).unwrap();
        let blob2 = db_serialize(&attachments).unwrap();

        assert_eq!(blob1, blob2);
    }
}
