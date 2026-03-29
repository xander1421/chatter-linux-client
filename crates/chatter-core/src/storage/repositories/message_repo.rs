//! Message repository.
//!
//! Data access for message and thread entities.

use crate::domain::ids::{AttachmentId, ChannelId, IntegrationId, MessageId, UserId};
use crate::domain::message::{ConversationType, Message, MessageContent, ReactionCount, Thread};
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
            conversation_id = %message.conversation_id,
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
                id, conversation_id, conversation_type, sender_id, content_type, content_json,
                thread_id, reply_to, attachments_blob,
                is_pinned, is_edited, is_deleted, is_read,
                sent_at, created_at, updated_at, edited_at,
                reaction_counts_blob, current_user_reactions_blob,
                mentioned_integrations_blob, targets_ai_bot, delivery_status
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)",
            params![
                message.id.to_string(),
                message.conversation_id.to_string(),
                message.conversation_type.as_str(),
                message.sender_id.map(|id| id.to_string()),
                content_type_discriminant(&message.content),
                content_json,
                message.thread_id.map(|id| id.to_string()),
                message.reply_to.map(|id| id.to_string()),
                attachments_blob,
                message.is_pinned as i32,
                message.is_edited as i32,
                message.is_deleted as i32,
                message.is_read as i32,
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
            conversation_id = %message.conversation_id,
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
                id, conversation_id, conversation_type, sender_id, content_type, content_json,
                thread_id, reply_to, attachments_blob,
                is_pinned, is_edited, is_deleted, is_read,
                sent_at, created_at, updated_at, edited_at,
                reaction_counts_blob, current_user_reactions_blob,
                mentioned_integrations_blob, targets_ai_bot
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
            params![
                message.id.to_string(),
                message.conversation_id.to_string(),
                message.conversation_type.as_str(),
                message.sender_id.map(|id| id.to_string()),
                content_type_discriminant(&message.content),
                content_json,
                message.thread_id.map(|id| id.to_string()),
                message.reply_to.map(|id| id.to_string()),
                attachments_blob,
                message.is_pinned as i32,
                message.is_edited as i32,
                message.is_deleted as i32,
                message.is_read as i32,
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
            &format!("SELECT {} FROM messages WHERE id = ?1", MSG_SELECT_COLS),
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
            &format!("SELECT {} FROM messages WHERE sender_id = ?1 AND sent_at = ?2", MSG_SELECT_COLS),
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
            &format!("SELECT {} FROM messages WHERE sender_id = ?1 ORDER BY sent_at DESC LIMIT ?2", MSG_SELECT_COLS),
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
                is_pinned = ?6, is_edited = ?7, is_deleted = ?8, is_read = ?9,
                updated_at = ?10, edited_at = ?11,
                reaction_counts_blob = ?12, current_user_reactions_blob = ?13,
                mentioned_integrations_blob = ?14, targets_ai_bot = ?15
             WHERE id = ?16",
            params![
                content_type_discriminant(&message.content),
                content_json,
                message.thread_id.map(|id| id.to_string()),
                message.reply_to.map(|id| id.to_string()),
                attachments_blob,
                message.is_pinned as i32,
                message.is_edited as i32,
                message.is_deleted as i32,
                message.is_read as i32,
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
                &format!(
                    "SELECT m.{} FROM messages m \
                     WHERE m.conversation_id = ?1 \
                       AND (m.sent_at, m.sender_id) < (SELECT sent_at, sender_id FROM messages WHERE id = ?2) \
                     ORDER BY m.sent_at DESC, m.sender_id DESC \
                     LIMIT ?3",
                    MSG_SELECT_COLS.replace(", ", ", m."),
                ),
            )?;
            let mut rows = stmt.query(params![channel_id.to_string(), before_id.to_string(), limit])?;
            collect_messages(&mut rows)?
        } else {
            // First page - no cursor
            // Use sent_at (nanosecond timestamp) for ordering to handle rapid messages
            let mut stmt = conn.prepare(
                &format!(
                    "SELECT {} FROM messages \
                     WHERE conversation_id = ?1 \
                     ORDER BY sent_at DESC, sender_id DESC \
                     LIMIT ?2",
                    MSG_SELECT_COLS,
                ),
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
            let aliased = format!("m.{}", MSG_SELECT_COLS.replace(", ", ", m."));
            let mut stmt = conn.prepare(
                &format!(
                    "SELECT {} FROM messages m \
                     WHERE m.thread_id = ?1 \
                       AND (m.sent_at, m.sender_id) < (SELECT sent_at, sender_id FROM messages WHERE id = ?2) \
                     ORDER BY m.sent_at DESC, m.sender_id DESC \
                     LIMIT ?3",
                    aliased,
                ),
            )?;
            let mut rows = stmt.query(params![thread_id.to_string(), before_id.to_string(), limit])?;
            collect_messages(&mut rows)?
        } else {
            let mut stmt = conn.prepare(
                &format!(
                    "SELECT {} FROM messages \
                     WHERE thread_id = ?1 \
                     ORDER BY sent_at DESC, sender_id DESC \
                     LIMIT ?2",
                    MSG_SELECT_COLS,
                ),
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
            &format!(
                "SELECT {} FROM messages \
                 WHERE conversation_id = ?1 AND is_pinned = 1 \
                 ORDER BY sent_at DESC",
                MSG_SELECT_COLS,
            ),
        )?;
        let mut rows = stmt.query(params![channel_id.to_string()])?;
        collect_messages(&mut rows)
    }

    /// Count messages in a channel.
    pub fn count_in_channel(&self, channel_id: &ChannelId) -> Result<u64, StorageError> {
        let conn = self.db.read_connection()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1",
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
            "SELECT root_message_id, conversation_id, reply_count, last_reply_at, participant_ids_blob
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
             WHERE conversation_id = ?3 AND sent_at = ?4",
            params![
                status,
                Utc::now().to_rfc3339(),
                channel_id.to_string(),
                sent_at as i64,
            ],
        )?;
        Ok(rows > 0)
    }

    /// Update delivery status by sent_at timestamp only (no channel_id needed).
    /// Used for delivery/read receipts where the channel isn't always known.
    pub fn update_delivery_status_by_timestamp(
        &self,
        sent_at: u64,
        status: &str,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE messages SET delivery_status = ?1, updated_at = ?2
             WHERE sent_at = ?3",
            params![
                status,
                Utc::now().to_rfc3339(),
                sent_at as i64,
            ],
        )?;
        Ok(rows > 0)
    }

    // ========================================================================
    // Unified message operations (for anon DM/group messages)
    // ========================================================================

    /// List messages for a conversation by channel_id and conversation_type.
    ///
    /// This is the unified version of the old `anon_dm_repo.get_messages()`
    /// and `anon_group_repo.get_messages()` methods.
    pub fn list_for_conversation(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
        before_sent_at: Option<u64>,
        limit: u32,
    ) -> Result<Vec<Message>, StorageError> {
        let conn = self.db.read_connection()?;

        let messages = if let Some(before_ns) = before_sent_at {
            let mut stmt = conn.prepare(
                &format!(
                    "SELECT {} FROM messages \
                     WHERE conversation_id = ?1 AND conversation_type = ?2 \
                       AND sent_at < ?3 AND is_deleted = 0 \
                     ORDER BY sent_at DESC, sender_id DESC \
                     LIMIT ?4",
                    MSG_SELECT_COLS,
                ),
            )?;
            let mut rows = stmt.query(params![
                channel_id.to_string(),
                conversation_type.as_str(),
                before_ns as i64,
                limit,
            ])?;
            collect_messages(&mut rows)?
        } else {
            let mut stmt = conn.prepare(
                &format!(
                    "SELECT {} FROM messages \
                     WHERE conversation_id = ?1 AND conversation_type = ?2 AND is_deleted = 0 \
                     ORDER BY sent_at DESC, sender_id DESC \
                     LIMIT ?3",
                    MSG_SELECT_COLS,
                ),
            )?;
            let mut rows = stmt.query(params![
                channel_id.to_string(),
                conversation_type.as_str(),
                limit,
            ])?;
            collect_messages(&mut rows)?
        };

        Ok(messages)
    }

    /// Get the latest message in a conversation (for preview).
    pub fn get_latest_for_conversation(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<Option<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            &format!(
                "SELECT {} FROM messages \
                 WHERE conversation_id = ?1 AND conversation_type = ?2 AND is_deleted = 0 \
                 ORDER BY sent_at DESC, sender_id DESC LIMIT 1",
                MSG_SELECT_COLS,
            ),
        )?;
        let result = stmt.query_row(
            params![channel_id.to_string(), conversation_type.as_str()],
            row_to_message,
        );
        match result {
            Ok(msg) => Ok(Some(msg?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Get a message by conversation + timestamp (any sender).
    pub fn get_by_conversation_timestamp(
        &self,
        channel_id: &ChannelId,
        timestamp_ns: u64,
    ) -> Result<Option<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            &format!(
                "SELECT {} FROM messages \
                 WHERE conversation_id = ?1 AND sent_at = ?2 AND is_deleted = 0 \
                 LIMIT 1",
                MSG_SELECT_COLS,
            ),
        )?;
        let result = stmt.query_row(
            params![channel_id.to_string(), timestamp_ns as i64],
            row_to_message,
        );
        match result {
            Ok(msg) => Ok(Some(msg?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Get a message by (channel_id, sender_id, sent_at) tuple.
    pub fn get_by_conversation_sender_timestamp(
        &self,
        channel_id: &ChannelId,
        sender_id: &UserId,
        timestamp_ns: u64,
    ) -> Result<Option<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            &format!(
                "SELECT {} FROM messages \
                 WHERE conversation_id = ?1 AND sender_id = ?2 AND sent_at = ?3 AND is_deleted = 0 \
                 LIMIT 1",
                MSG_SELECT_COLS,
            ),
        )?;
        let result = stmt.query_row(
            params![
                channel_id.to_string(),
                sender_id.to_string(),
                timestamp_ns as i64,
            ],
            row_to_message,
        );
        match result {
            Ok(msg) => Ok(Some(msg?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Mark all messages in a conversation as read.
    pub fn mark_conversation_read(&self, channel_id: &ChannelId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "UPDATE messages SET is_read = 1 WHERE conversation_id = ?1 AND is_read = 0",
            params![channel_id.to_string()],
        )?;
        Ok(())
    }

    /// Count messages in a conversation.
    pub fn count_in_conversation(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<i64, StorageError> {
        let conn = self.db.read_connection()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1 AND conversation_type = ?2 AND is_deleted = 0",
            params![channel_id.to_string(), conversation_type.as_str()],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Soft delete a message by sender timestamp (for incoming delete-for-everyone).
    pub fn mark_deleted_by_sender_timestamp(
        &self,
        channel_id: &ChannelId,
        sender_id: &UserId,
        sent_timestamp: u64,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE messages SET is_deleted = 1, updated_at = ?1
             WHERE conversation_id = ?2 AND sender_id = ?3 AND sent_at = ?4 AND is_deleted = 0",
            params![
                Utc::now().to_rfc3339(),
                channel_id.to_string(),
                sender_id.to_string(),
                sent_timestamp as i64,
            ],
        )?;
        Ok(rows > 0)
    }

    /// Update message text by conversation + timestamp (for edits).
    pub fn update_text_by_timestamp(
        &self,
        channel_id: &ChannelId,
        sent_timestamp: u64,
        new_text: &str,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let content = MessageContent::Text(new_text.to_string());
        let content_json = serde_json::to_string(&content)
            .map_err(|e| StorageError::InvalidData(format!("Failed to serialize content: {}", e)))?;
        let rows = conn.execute(
            "UPDATE messages SET content_json = ?1, is_edited = 1, updated_at = ?2
             WHERE conversation_id = ?3 AND sent_at = ?4",
            params![
                content_json,
                Utc::now().to_rfc3339(),
                channel_id.to_string(),
                sent_timestamp as i64,
            ],
        )?;
        Ok(rows > 0)
    }

    /// Set pinned state by conversation + timestamp.
    pub fn set_pinned_by_timestamp(
        &self,
        channel_id: &ChannelId,
        sent_timestamp: u64,
        pinned: bool,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE messages SET is_pinned = ?1
             WHERE conversation_id = ?2 AND sent_at = ?3 AND is_deleted = 0",
            params![
                pinned as i32,
                channel_id.to_string(),
                sent_timestamp as i64,
            ],
        )?;
        Ok(rows > 0)
    }

    /// List all pinned messages for a conversation.
    pub fn list_pinned_for_conversation(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<Vec<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            &format!(
                "SELECT {} FROM messages \
                 WHERE conversation_id = ?1 AND conversation_type = ?2 \
                   AND is_pinned = 1 AND is_deleted = 0 \
                 ORDER BY sent_at DESC",
                MSG_SELECT_COLS,
            ),
        )?;
        let mut rows = stmt.query(params![
            channel_id.to_string(),
            conversation_type.as_str(),
        ])?;
        collect_messages(&mut rows)
    }

    /// Update delivery status by conversation + timestamps.
    pub fn update_delivery_status_by_timestamps(
        &self,
        channel_id: &ChannelId,
        timestamps: &[u64],
        status: &str,
    ) -> Result<usize, StorageError> {
        if timestamps.is_empty() {
            return Ok(0);
        }
        let conn = self.db.connection()?;
        let now = Utc::now().to_rfc3339();
        let mut total = 0;
        for &ts in timestamps {
            let rows = conn.execute(
                "UPDATE messages SET delivery_status = ?1, updated_at = ?2
                 WHERE conversation_id = ?3 AND sent_at = ?4",
                params![status, now, channel_id.to_string(), ts as i64],
            )?;
            total += rows;
        }
        Ok(total)
    }

    // ========================================================================
    // Thread operations
    // ========================================================================

    /// Insert or update thread metadata.
    pub fn upsert_thread(&self, thread: &Thread) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        // Use bincode for efficient BLOB storage
        let participant_ids_blob = db_serialize(&thread.participant_ids)?;

        conn.execute(
            "INSERT INTO threads (root_message_id, conversation_id, reply_count, last_reply_at, participant_ids_blob)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(root_message_id) DO UPDATE SET
                reply_count = excluded.reply_count,
                last_reply_at = excluded.last_reply_at,
                participant_ids_blob = excluded.participant_ids_blob",
            params![
                thread.root_message_id.to_string(),
                thread.conversation_id.to_string(),
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

/// Standard SELECT column list for messages.
/// Used by all message queries for consistency with row_to_message.
const MSG_SELECT_COLS: &str =
    "id, conversation_id, conversation_type, sender_id, content_json, \
     thread_id, reply_to, attachments_blob, \
     is_pinned, is_edited, is_deleted, is_read, \
     sent_at, created_at, updated_at, edited_at, \
     reaction_counts_blob, current_user_reactions_blob, \
     mentioned_integrations_blob, targets_ai_bot, delivery_status";

/// Convert a row to a Message.
/// Column order: id(0), conversation_id(1), conversation_type(2), sender_id(3),
///               content_json(4), thread_id(5), reply_to(6), attachments_blob(7),
///               is_pinned(8), is_edited(9), is_deleted(10), is_read(11),
///               sent_at(12), created_at(13), updated_at(14), edited_at(15),
///               reaction_counts_blob(16), current_user_reactions_blob(17),
///               mentioned_integrations_blob(18), targets_ai_bot(19), delivery_status(20)
fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Message, StorageError>> {
    let content_json: String = row.get(4)?;
    // Read from BLOB columns (bincode serialized)
    let attachments_blob: Vec<u8> = row.get(7)?;
    let reaction_counts_blob: Vec<u8> = row.get(16)?;
    let current_user_reactions_blob: Vec<u8> = row.get(17)?;
    let mentioned_integrations_blob: Vec<u8> = row.get(18)?;

    let conversation_type_str: String = row.get(2)?;
    let thread_id: Option<String> = row.get(5)?;
    let reply_to: Option<String> = row.get(6)?;
    let sender_id: Option<String> = row.get(3)?;
    let edited_at: Option<String> = row.get(15)?;

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
        conversation_id: row
            .get::<_, String>(1)?
            .parse()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        conversation_type: ConversationType::from_str_value(&conversation_type_str),
        sender_id: sender_id
            .map(|s| s.parse())
            .transpose()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        device_id: None,
        content,
        encrypted_content: None,
        thread_id: thread_id
            .map(|s| s.parse())
            .transpose()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        reply_to: reply_to
            .map(|s| s.parse())
            .transpose()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        attachments,
        is_pinned: row.get::<_, i32>(8)? != 0,
        is_edited: row.get::<_, i32>(9)? != 0,
        is_deleted: row.get::<_, i32>(10)? != 0,
        is_read: row.get::<_, i32>(11)? != 0,
        sent_at: row.get::<_, i64>(12)? as u64,
        created_at: parse_datetime(&row.get::<_, String>(13)?),
        updated_at: parse_datetime(&row.get::<_, String>(14)?),
        edited_at: edited_at.map(|s| parse_datetime(&s)),
        reaction_counts,
        current_user_reactions,
        mentioned_integrations,
        targets_ai_bot: row.get::<_, i32>(19)? != 0,
        delivery_status: row.get::<_, String>(20).unwrap_or_default(),
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
        conversation_id: row
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
    fn test_bincode_roundtrip_attachments() {
        let attachments: Vec<AttachmentId> = (0..50).map(|_| AttachmentId::new()).collect();
        let blob = db_serialize(&attachments).unwrap();
        let restored: Vec<AttachmentId> = db_deserialize(&blob).unwrap();
        assert_eq!(attachments, restored);
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
