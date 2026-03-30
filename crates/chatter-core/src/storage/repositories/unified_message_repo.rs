//! Unified message repository.
//!
//! Single repository for all message types (channel, DM, anon DM, anon group,
//! saved messages). All queries use the `conversation_type` discriminant.
//!
//! Also owns reaction operations via the `unified_reactions` table, keeping
//! `reaction_counts_blob` and `current_user_reactions_blob` on message rows
//! in sync.

use crate::domain::ids::{AttachmentId, ChannelId, IntegrationId, MessageId, ReactionId, UserId};
use crate::domain::message::{ConversationType, Message, MessageContent, ReactionCount, Thread};
use crate::domain::reaction::{Emoji, Reaction};
use crate::error::StorageError;
use crate::storage::serialization::{db_deserialize, db_serialize};
use crate::storage::Database;
use chrono::{DateTime, Utc};
use rusqlite::params;
use std::sync::Arc;

/// Unified message repository for all conversation types.
///
/// Replaces the three separate message repos (`message_repo`, `anon_dm_repo` messages,
/// `anon_group_repo` messages) with a single implementation that uses the
/// `conversation_type` discriminant.
pub struct UnifiedMessageRepository {
    db: Arc<Database>,
}

impl UnifiedMessageRepository {
    /// Create a new UnifiedMessageRepository.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    // ========================================================================
    // CRUD
    // ========================================================================

    /// Insert a new message.
    pub fn insert(&self, message: &Message) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let content_json = serde_json::to_string(&message.content)
            .map_err(|e| StorageError::InvalidData(format!("serialize content: {}", e)))?;
        let attachments_blob = db_serialize(&message.attachments)?;
        let reaction_counts_blob = db_serialize(&message.reaction_counts)?;
        let current_user_reactions_blob = db_serialize(&message.current_user_reactions)?;
        let mentioned_integrations_blob = db_serialize(&message.mentioned_integrations)?;

        conn.execute(
            "INSERT INTO messages (
                id, conversation_id, conversation_type, sender_id, device_id,
                content_type, content_json, encrypted_content,
                thread_id, reply_to, attachments_blob,
                is_pinned, is_edited, is_deleted, is_read,
                sent_at, created_at, updated_at, edited_at,
                reaction_counts_blob, current_user_reactions_blob,
                mentioned_integrations_blob, targets_ai_bot, delivery_status,
                reply_to_author_id, reply_to_text
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19,
                ?20, ?21, ?22, ?23, ?24, ?25, ?26
            )",
            params![
                message.id.to_string(),
                message.conversation_id.to_string(),
                message.conversation_type.as_str(),
                message.sender_id.map(|id| id.to_string()),
                message.device_id.map(|d| d as i64),
                content_type_discriminant(&message.content),
                content_json,
                message.encrypted_content.as_deref(),
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
                message.reply_to_author_id.map(|id| id.to_string()),
                message.reply_to_text.as_deref(),
            ],
        )?;
        Ok(())
    }

    /// Insert a message, ignoring duplicates (Signal dedup on primary key + sender_id/sent_at).
    ///
    /// Returns `true` if inserted, `false` if skipped.
    pub fn insert_or_ignore(&self, message: &Message) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let content_json = serde_json::to_string(&message.content)
            .map_err(|e| StorageError::InvalidData(format!("serialize content: {}", e)))?;
        let attachments_blob = db_serialize(&message.attachments)?;
        let reaction_counts_blob = db_serialize(&message.reaction_counts)?;
        let current_user_reactions_blob = db_serialize(&message.current_user_reactions)?;
        let mentioned_integrations_blob = db_serialize(&message.mentioned_integrations)?;

        let rows = conn.execute(
            "INSERT OR IGNORE INTO messages (
                id, conversation_id, conversation_type, sender_id, device_id,
                content_type, content_json, encrypted_content,
                thread_id, reply_to, attachments_blob,
                is_pinned, is_edited, is_deleted, is_read,
                sent_at, created_at, updated_at, edited_at,
                reaction_counts_blob, current_user_reactions_blob,
                mentioned_integrations_blob, targets_ai_bot, delivery_status,
                reply_to_author_id, reply_to_text
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19,
                ?20, ?21, ?22, ?23, ?24, ?25, ?26
            )",
            params![
                message.id.to_string(),
                message.conversation_id.to_string(),
                message.conversation_type.as_str(),
                message.sender_id.map(|id| id.to_string()),
                message.device_id.map(|d| d as i64),
                content_type_discriminant(&message.content),
                content_json,
                message.encrypted_content.as_deref(),
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
                message.reply_to_author_id.map(|id| id.to_string()),
                message.reply_to_text.as_deref(),
            ],
        )?;
        Ok(rows > 0)
    }

    /// Get a message by ID.
    pub fn get(&self, id: &MessageId) -> Result<Option<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM messages WHERE id = ?1",
            MSG_COLS
        ))?;
        match stmt.query_row(params![id.to_string()], row_to_message) {
            Ok(msg) => Ok(Some(msg?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Get a message by (sender_id, sent_at) — Signal-style dedup key.
    pub fn get_by_sender_timestamp(
        &self,
        sender_id: &UserId,
        sent_at: u64,
    ) -> Result<Option<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM messages WHERE sender_id = ?1 AND sent_at = ?2",
            MSG_COLS
        ))?;
        match stmt.query_row(params![sender_id.to_string(), sent_at as i64], row_to_message) {
            Ok(msg) => Ok(Some(msg?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// List messages for a conversation with cursor pagination (newest first).
    ///
    /// Pass `before_sent_at` for cursor-based paging.
    pub fn list(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
        before_sent_at: Option<u64>,
        limit: u32,
    ) -> Result<Vec<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        if let Some(before) = before_sent_at {
            let mut stmt = conn.prepare(&format!(
                "SELECT {} FROM messages \
                 WHERE conversation_id = ?1 AND conversation_type = ?2 \
                   AND sent_at < ?3 AND is_deleted = 0 \
                 ORDER BY sent_at DESC, sender_id DESC \
                 LIMIT ?4",
                MSG_COLS
            ))?;
            let mut rows = stmt.query(params![
                conversation_id.to_string(),
                conversation_type.as_str(),
                before as i64,
                limit,
            ])?;
            collect_messages(&mut rows)
        } else {
            let mut stmt = conn.prepare(&format!(
                "SELECT {} FROM messages \
                 WHERE conversation_id = ?1 AND conversation_type = ?2 AND is_deleted = 0 \
                 ORDER BY sent_at DESC, sender_id DESC \
                 LIMIT ?3",
                MSG_COLS
            ))?;
            let mut rows = stmt.query(params![
                conversation_id.to_string(),
                conversation_type.as_str(),
                limit,
            ])?;
            collect_messages(&mut rows)
        }
    }

    /// Get the latest message in a conversation (for preview in conversation list).
    pub fn get_latest(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<Option<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM messages \
             WHERE conversation_id = ?1 AND conversation_type = ?2 AND is_deleted = 0 \
             ORDER BY sent_at DESC LIMIT 1",
            MSG_COLS
        ))?;
        match stmt.query_row(
            params![conversation_id.to_string(), conversation_type.as_str()],
            row_to_message,
        ) {
            Ok(msg) => Ok(Some(msg?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Update an existing message (full replace of mutable fields).
    pub fn update(&self, message: &Message) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let content_json = serde_json::to_string(&message.content)
            .map_err(|e| StorageError::InvalidData(format!("serialize content: {}", e)))?;
        let attachments_blob = db_serialize(&message.attachments)?;
        let reaction_counts_blob = db_serialize(&message.reaction_counts)?;
        let current_user_reactions_blob = db_serialize(&message.current_user_reactions)?;
        let mentioned_integrations_blob = db_serialize(&message.mentioned_integrations)?;

        let rows = conn.execute(
            "UPDATE messages SET
                content_type = ?1, content_json = ?2, encrypted_content = ?3,
                thread_id = ?4, reply_to = ?5, attachments_blob = ?6,
                is_pinned = ?7, is_edited = ?8, is_deleted = ?9, is_read = ?10,
                updated_at = ?11, edited_at = ?12,
                reaction_counts_blob = ?13, current_user_reactions_blob = ?14,
                mentioned_integrations_blob = ?15, targets_ai_bot = ?16,
                delivery_status = ?17,
                reply_to_author_id = ?18, reply_to_text = ?19
             WHERE id = ?20",
            params![
                content_type_discriminant(&message.content),
                content_json,
                message.encrypted_content.as_deref(),
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
                &message.delivery_status,
                message.reply_to_author_id.map(|id| id.to_string()),
                message.reply_to_text.as_deref(),
                message.id.to_string(),
            ],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("Message {}", message.id)));
        }
        Ok(())
    }

    /// Delete a message by ID (hard delete — CASCADE removes unified_reactions).
    pub fn delete(&self, id: &MessageId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute("DELETE FROM messages WHERE id = ?1", params![id.to_string()])?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("Message {}", id)));
        }
        Ok(())
    }

    /// Soft-delete multiple messages by ID.
    pub fn soft_delete_batch(&self, ids: &[MessageId]) -> Result<usize, StorageError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.db.connection()?;
        let now = Utc::now().to_rfc3339();
        let placeholders: Vec<String> = (1..=ids.len()).map(|i| format!("?{}", i)).collect();
        let query = format!(
            "UPDATE messages SET is_deleted = 1, updated_at = ?{} WHERE id IN ({})",
            ids.len() + 1,
            placeholders.join(", ")
        );
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = ids
            .iter()
            .map(|id| Box::new(id.to_string()) as Box<dyn rusqlite::ToSql>)
            .collect();
        params_vec.push(Box::new(now));
        let rows = conn.execute(
            &query,
            rusqlite::params_from_iter(params_vec.iter().map(|b| b.as_ref())),
        )?;
        Ok(rows)
    }

    // ========================================================================
    // Pin / Unpin
    // ========================================================================

    /// Pin a message by ID.
    pub fn pin(&self, id: &MessageId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE messages SET is_pinned = 1, updated_at = ?1 WHERE id = ?2 AND is_deleted = 0",
            params![Utc::now().to_rfc3339(), id.to_string()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("Message {}", id)));
        }
        Ok(())
    }

    /// Unpin a message by ID.
    pub fn unpin(&self, id: &MessageId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE messages SET is_pinned = 0, updated_at = ?1 WHERE id = ?2",
            params![Utc::now().to_rfc3339(), id.to_string()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("Message {}", id)));
        }
        Ok(())
    }

    /// List all pinned messages in a conversation.
    pub fn list_pinned(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<Vec<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM messages \
             WHERE conversation_id = ?1 AND conversation_type = ?2 \
               AND is_pinned = 1 AND is_deleted = 0 \
             ORDER BY sent_at DESC",
            MSG_COLS
        ))?;
        let mut rows = stmt.query(params![
            conversation_id.to_string(),
            conversation_type.as_str(),
        ])?;
        collect_messages(&mut rows)
    }

    // ========================================================================
    // Read / Unread
    // ========================================================================

    /// Mark a single message as read.
    pub fn mark_read(&self, id: &MessageId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "UPDATE messages SET is_read = 1, updated_at = ?1 WHERE id = ?2 AND is_read = 0",
            params![Utc::now().to_rfc3339(), id.to_string()],
        )?;
        Ok(())
    }

    /// Mark a single message as unread.
    pub fn mark_unread(&self, id: &MessageId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "UPDATE messages SET is_read = 0, updated_at = ?1 WHERE id = ?2 AND is_read = 1",
            params![Utc::now().to_rfc3339(), id.to_string()],
        )?;
        Ok(())
    }

    /// Mark all messages in a conversation as read.
    pub fn mark_conversation_read(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "UPDATE messages SET is_read = 1              WHERE conversation_id = ?1 AND conversation_type = ?2 AND is_read = 0",
            params![conversation_id.to_string(), conversation_type.as_str()],
        )?;
        Ok(())
    }

    /// Get the count of unread messages in a conversation.
    pub fn get_unread_count(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<u64, StorageError> {
        let conn = self.db.read_connection()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM messages \
             WHERE conversation_id = ?1 AND conversation_type = ?2 \
               AND is_read = 0 AND is_deleted = 0",
            params![conversation_id.to_string(), conversation_type.as_str()],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    // ========================================================================
    // Reactions (unified_reactions table)
    // ========================================================================

    /// Add a reaction. Inserts into `unified_reactions` and updates blobs on the
    /// message row. No-op if the (message_id, user_id, emoji) triple already exists.
    ///
    /// Returns `true` if a new reaction was added.
    pub fn add_reaction(
        &self,
        message_id: &MessageId,
        user_id: &UserId,
        emoji: &str,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let reaction_id = ReactionId::new();
        let now = Utc::now().to_rfc3339();

        let inserted = conn.execute(
            "INSERT OR IGNORE INTO unified_reactions (id, message_id, user_id, emoji, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                reaction_id.to_string(),
                message_id.to_string(),
                user_id.to_string(),
                emoji,
                now,
            ],
        )?;

        if inserted == 0 {
            return Ok(false); // Already exists
        }

        // Rebuild blobs from ground truth in unified_reactions
        self.rebuild_reaction_blobs_with_conn(&conn, message_id, user_id)?;
        Ok(true)
    }

    /// Remove a reaction by (message_id, user_id, emoji). Updates blobs on the message row.
    ///
    /// Returns `true` if a reaction was actually removed.
    pub fn remove_reaction(
        &self,
        message_id: &MessageId,
        user_id: &UserId,
        emoji: &str,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let deleted = conn.execute(
            "DELETE FROM unified_reactions WHERE message_id = ?1 AND user_id = ?2 AND emoji = ?3",
            params![message_id.to_string(), user_id.to_string(), emoji],
        )?;

        if deleted == 0 {
            return Ok(false);
        }

        self.rebuild_reaction_blobs_with_conn(&conn, message_id, user_id)?;
        Ok(true)
    }

    /// Add a reaction by (conversation_id, sent_at) tuple — Signal-style identification.
    ///
    /// Uses a SQL subquery to resolve the message, avoiding a separate lookup round-trip.
    /// Returns `true` if a new reaction was added.
    pub fn add_reaction_by_timestamp(
        &self,
        conversation_id: &ChannelId,
        sent_at: u64,
        user_id: &UserId,
        emoji: &str,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let reaction_id = ReactionId::new();
        let now = Utc::now().to_rfc3339();

        // Resolve message_id via subquery and insert in one statement.
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO unified_reactions (id, message_id, user_id, emoji, created_at)
             SELECT ?1, m.id, ?2, ?3, ?4
             FROM messages m
             WHERE m.conversation_id = ?5 AND m.sent_at = ?6 AND m.is_deleted = 0
             LIMIT 1",
            params![
                reaction_id.to_string(),
                user_id.to_string(),
                emoji,
                now,
                conversation_id.to_string(),
                sent_at as i64,
            ],
        )?;

        if inserted == 0 {
            return Ok(false); // Message not found or reaction already exists
        }

        // Look up the resolved message_id to rebuild blobs.
        let message_id = self.resolve_message_id(&conn, conversation_id, sent_at)?;
        if let Some(mid) = message_id {
            self.rebuild_reaction_blobs_with_conn(&conn, &mid, user_id)?;
        }
        Ok(true)
    }

    /// Remove a reaction by (conversation_id, sent_at) tuple — Signal-style identification.
    ///
    /// Returns `true` if a reaction was actually removed.
    pub fn remove_reaction_by_timestamp(
        &self,
        conversation_id: &ChannelId,
        sent_at: u64,
        user_id: &UserId,
        emoji: &str,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;

        let deleted = conn.execute(
            "DELETE FROM unified_reactions
             WHERE user_id = ?1 AND emoji = ?2
               AND message_id IN (
                   SELECT id FROM messages
                   WHERE conversation_id = ?3 AND sent_at = ?4 AND is_deleted = 0
               )",
            params![
                user_id.to_string(),
                emoji,
                conversation_id.to_string(),
                sent_at as i64,
            ],
        )?;

        if deleted == 0 {
            return Ok(false);
        }

        let message_id = self.resolve_message_id(&conn, conversation_id, sent_at)?;
        if let Some(mid) = message_id {
            self.rebuild_reaction_blobs_with_conn(&conn, &mid, user_id)?;
        }
        Ok(true)
    }

    /// Resolve a message UUID from (conversation_id, sent_at).
    fn resolve_message_id(
        &self,
        conn: &rusqlite::Connection,
        conversation_id: &ChannelId,
        sent_at: u64,
    ) -> Result<Option<MessageId>, StorageError> {
        match conn.query_row(
            "SELECT id FROM messages WHERE conversation_id = ?1 AND sent_at = ?2 AND is_deleted = 0 LIMIT 1",
            params![conversation_id.to_string(), sent_at as i64],
            |row| {
                let id_str: String = row.get(0)?;
                Ok(id_str)
            },
        ) {
            Ok(id_str) => {
                let mid: MessageId = id_str.parse()
                    .map_err(|_| StorageError::InvalidData(format!("Invalid message UUID: {}", id_str)))?;
                Ok(Some(mid))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Get all reactions on a message.
    pub fn get_reactions(&self, message_id: &MessageId) -> Result<Vec<Reaction>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, message_id, user_id, emoji, created_at
             FROM unified_reactions WHERE message_id = ?1
             ORDER BY created_at ASC",
        )?;
        let mut rows = stmt.query(params![message_id.to_string()])?;
        let mut reactions = Vec::new();
        while let Some(row) = rows.next()? {
            reactions.push(row_to_reaction(row)?);
        }
        Ok(reactions)
    }

    /// Get reaction counts per emoji for a message (from unified_reactions, not blob).
    pub fn get_reaction_counts(
        &self,
        message_id: &MessageId,
    ) -> Result<Vec<ReactionCount>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT emoji, COUNT(*) FROM unified_reactions
             WHERE message_id = ?1 GROUP BY emoji ORDER BY COUNT(*) DESC",
        )?;
        let mut rows = stmt.query(params![message_id.to_string()])?;
        let mut counts = Vec::new();
        while let Some(row) = rows.next()? {
            let emoji_str: String = row.get(0)?;
            let count: u32 = row.get(1)?;
            counts.push(ReactionCount::new(emoji_str, count));
        }
        Ok(counts)
    }

    /// Get reaction counts for all messages in a conversation.
    ///
    /// Returns a map of `sent_at` -> `Vec<(emoji, count)>` for messages that have reactions.
    pub fn get_reaction_counts_for_conversation(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<std::collections::HashMap<u64, Vec<(String, u32)>>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT m.sent_at, r.emoji, COUNT(*) as cnt
             FROM unified_reactions r
             INNER JOIN messages m ON r.message_id = m.id
             WHERE m.conversation_id = ?1 AND m.conversation_type = ?2 AND m.is_deleted = 0
             GROUP BY m.sent_at, r.emoji
             ORDER BY m.sent_at, cnt DESC",
        )?;
        let mut rows = stmt.query(params![conversation_id.to_string(), conversation_type.as_str()])?;
        let mut map: std::collections::HashMap<u64, Vec<(String, u32)>> =
            std::collections::HashMap::new();
        while let Some(row) = rows.next()? {
            let sent_at: i64 = row.get(0)?;
            let emoji: String = row.get(1)?;
            let count: u32 = row.get(2)?;
            map.entry(sent_at as u64).or_default().push((emoji, count));
        }
        Ok(map)
    }

    // ========================================================================
    // Search
    // ========================================================================

    /// Search messages by text content within a conversation.
    ///
    /// Uses LIKE on `content_json` for basic text search.
    /// Results ordered by `sent_at DESC`.
    pub fn search(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
        query: &str,
        limit: u32,
    ) -> Result<Vec<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let pattern = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM messages \
             WHERE conversation_id = ?1 AND conversation_type = ?2 \
               AND content_json LIKE ?3 ESCAPE '\\' \
               AND is_deleted = 0 \
             ORDER BY sent_at DESC \
             LIMIT ?4",
            MSG_COLS
        ))?;
        let mut rows = stmt.query(params![
            conversation_id.to_string(),
            conversation_type.as_str(),
            pattern,
            limit,
        ])?;
        collect_messages(&mut rows)
    }

    /// Search messages globally (across all conversations) by text.
    pub fn search_global(&self, query: &str, limit: u32) -> Result<Vec<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let pattern = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM messages \
             WHERE content_json LIKE ?1 ESCAPE '\\' AND is_deleted = 0 \
             ORDER BY sent_at DESC \
             LIMIT ?2",
            MSG_COLS
        ))?;
        let mut rows = stmt.query(params![pattern, limit])?;
        collect_messages(&mut rows)
    }

    // ========================================================================
    // Conversation-level helpers
    // ========================================================================

    /// Count messages in a conversation.
    pub fn count(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<u64, StorageError> {
        let conn = self.db.read_connection()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM messages \
             WHERE conversation_id = ?1 AND conversation_type = ?2 AND is_deleted = 0",
            params![conversation_id.to_string(), conversation_type.as_str()],
            |row| row.get(0),
        )?;
        Ok(n as u64)
    }

    /// List messages in a thread.
    pub fn list_for_thread(
        &self,
        thread_id: &MessageId,
        before_sent_at: Option<u64>,
        limit: u32,
    ) -> Result<Vec<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        if let Some(before) = before_sent_at {
            let mut stmt = conn.prepare(&format!(
                "SELECT {} FROM messages \
                 WHERE thread_id = ?1 AND sent_at < ?2 AND is_deleted = 0 \
                 ORDER BY sent_at DESC LIMIT ?3",
                MSG_COLS
            ))?;
            let mut rows = stmt.query(params![thread_id.to_string(), before as i64, limit])?;
            collect_messages(&mut rows)
        } else {
            let mut stmt = conn.prepare(&format!(
                "SELECT {} FROM messages \
                 WHERE thread_id = ?1 AND is_deleted = 0 \
                 ORDER BY sent_at DESC LIMIT ?2",
                MSG_COLS
            ))?;
            let mut rows = stmt.query(params![thread_id.to_string(), limit])?;
            collect_messages(&mut rows)
        }
    }

    /// Update delivery status by (conversation_id, sent_at).
    pub fn update_delivery_status(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
        sent_at: u64,
        status: &str,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE messages SET delivery_status = ?1, updated_at = ?2
             WHERE conversation_id = ?3 AND conversation_type = ?4 AND sent_at = ?5",
            params![
                status,
                Utc::now().to_rfc3339(),
                conversation_id.to_string(),
                conversation_type.as_str(),
                sent_at as i64,
            ],
        )?;
        Ok(rows > 0)
    }

    /// Soft-delete by (conversation_id, sender_id, sent_at) — for delete-for-everyone.
    pub fn mark_deleted_by_sender_timestamp(
        &self,
        conversation_id: &ChannelId,
        sender_id: &UserId,
        sent_at: u64,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE messages SET is_deleted = 1, updated_at = ?1
             WHERE conversation_id = ?2 AND sender_id = ?3 AND sent_at = ?4 AND is_deleted = 0",
            params![
                Utc::now().to_rfc3339(),
                conversation_id.to_string(),
                sender_id.to_string(),
                sent_at as i64,
            ],
        )?;
        Ok(rows > 0)
    }

    /// Update message text by (conversation_id, sent_at) — for edits.
    pub fn update_text_by_timestamp(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
        sent_at: u64,
        new_text: &str,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let content = MessageContent::Text(new_text.to_string());
        let content_json = serde_json::to_string(&content)
            .map_err(|e| StorageError::InvalidData(format!("serialize content: {}", e)))?;
        let rows = conn.execute(
            "UPDATE messages SET content_json = ?1, is_edited = 1, updated_at = ?2
             WHERE conversation_id = ?3 AND conversation_type = ?4 AND sent_at = ?5",
            params![
                content_json,
                Utc::now().to_rfc3339(),
                conversation_id.to_string(),
                conversation_type.as_str(),
                sent_at as i64,
            ],
        )?;
        Ok(rows > 0)
    }

    /// Set pin state by (conversation_id, sent_at).
    pub fn set_pinned_by_timestamp(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
        sent_at: u64,
        pinned: bool,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE messages SET is_pinned = ?1, updated_at = ?2
             WHERE conversation_id = ?3 AND conversation_type = ?4 AND sent_at = ?5 AND is_deleted = 0",
            params![
                pinned as i32,
                Utc::now().to_rfc3339(),
                conversation_id.to_string(),
                conversation_type.as_str(),
                sent_at as i64,
            ],
        )?;
        Ok(rows > 0)
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
        match stmt.query_row(params![root_message_id.to_string()], row_to_thread) {
            Ok(thread) => Ok(Some(thread?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Upsert thread metadata.
    pub fn upsert_thread(&self, thread: &Thread) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
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

    // ========================================================================
    // Compatibility methods (matching MessageRepository API)
    // ========================================================================

    /// List messages for a channel with cursor-based pagination.
    ///
    /// Returns messages in reverse chronological order (newest first).
    /// Use `before` to paginate - pass the last message's ID to get the next page.
    pub fn list_for_channel(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
        before: Option<&MessageId>,
        limit: u32,
    ) -> Result<Vec<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        if let Some(before_id) = before {
            let aliased = format!("m.{}", MSG_COLS.replace(", ", ", m."));
            let mut stmt = conn.prepare(&format!(
                "SELECT {} FROM messages m \
                 WHERE m.conversation_id = ?1 AND m.conversation_type = ?2 \
                   AND (m.sent_at, m.sender_id) < (SELECT sent_at, sender_id FROM messages WHERE id = ?3) \
                 ORDER BY m.sent_at DESC, m.sender_id DESC \
                 LIMIT ?4",
                aliased,
            ))?;
            let mut rows = stmt.query(params![channel_id.to_string(), conversation_type.as_str(), before_id.to_string(), limit])?;
            collect_messages(&mut rows)
        } else {
            let mut stmt = conn.prepare(&format!(
                "SELECT {} FROM messages \
                 WHERE conversation_id = ?1 AND conversation_type = ?2 \
                 ORDER BY sent_at DESC, sender_id DESC \
                 LIMIT ?3",
                MSG_COLS,
            ))?;
            let mut rows = stmt.query(params![channel_id.to_string(), conversation_type.as_str(), limit])?;
            collect_messages(&mut rows)
        }
    }

    /// List messages for a conversation by channel_id and conversation_type.
    /// Alias for `list()` matching old MessageRepository API.
    pub fn list_for_conversation(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
        before_sent_at: Option<u64>,
        limit: u32,
    ) -> Result<Vec<Message>, StorageError> {
        self.list(channel_id, conversation_type, before_sent_at, limit)
    }

    /// Get the latest message in a conversation (for preview).
    /// Alias for `get_latest()` matching old MessageRepository API.
    pub fn get_latest_for_conversation(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<Option<Message>, StorageError> {
        self.get_latest(channel_id, conversation_type)
    }

    /// Get a message by conversation + timestamp (any sender).
    pub fn get_by_conversation_timestamp(
        &self,
        channel_id: &ChannelId,
        timestamp_ns: u64,
    ) -> Result<Option<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM messages \
             WHERE conversation_id = ?1 AND sent_at = ?2 AND is_deleted = 0 \
             LIMIT 1",
            MSG_COLS,
        ))?;
        match stmt.query_row(
            params![channel_id.to_string(), timestamp_ns as i64],
            row_to_message,
        ) {
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
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM messages \
             WHERE conversation_id = ?1 AND sender_id = ?2 AND sent_at = ?3 AND is_deleted = 0 \
             LIMIT 1",
            MSG_COLS,
        ))?;
        match stmt.query_row(
            params![channel_id.to_string(), sender_id.to_string(), timestamp_ns as i64],
            row_to_message,
        ) {
            Ok(msg) => Ok(Some(msg?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Count messages in a conversation (returns i64 for compat).
    pub fn count_in_conversation(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<i64, StorageError> {
        Ok(self.count(channel_id, conversation_type)? as i64)
    }

    /// Count messages in a channel (no conversation_type filter).
    /// Update delivery status by conversation + timestamp (scoped to one conversation).
    pub fn update_delivery_status_by_timestamp(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
        sent_at: u64,
        status: &str,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE messages SET delivery_status = ?1, updated_at = ?2
             WHERE conversation_id = ?3 AND conversation_type = ?4 AND sent_at = ?5",
            params![
                status,
                Utc::now().to_rfc3339(),
                conversation_id.to_string(),
                conversation_type.as_str(),
                sent_at as i64,
            ],
        )?;
        Ok(rows > 0)
    }

    /// Update delivery status by timestamp with automatic conversation lookup.
    ///
    /// For callers that lack conversation context (e.g. cross-channel receipts),
    /// looks up matching message IDs first, then performs scoped updates by
    /// primary key. Never does an unscoped `WHERE sent_at = ?` UPDATE.
    pub fn update_delivery_status_by_timestamp_with_lookup(
        &self,
        sent_at: u64,
        status: &str,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        // Step 1: Find message IDs matching this timestamp (read-only).
        let mut stmt = conn.prepare("SELECT id FROM messages WHERE sent_at = ?1")?;
        let ids: Vec<String> = stmt
            .query_map(params![sent_at as i64], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        if ids.is_empty() {
            return Ok(false);
        }
        // Step 2: Update each by primary key (scoped, no cross-conversation risk).
        let now = Utc::now().to_rfc3339();
        let mut updated = false;
        for id in &ids {
            let rows = conn.execute(
                "UPDATE messages SET delivery_status = ?1, updated_at = ?2 WHERE id = ?3",
                params![status, now, id],
            )?;
            if rows > 0 {
                updated = true;
            }
        }
        Ok(updated)
    }

    /// Update delivery status by conversation + timestamps (batch).
    pub fn update_delivery_status_by_timestamps(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
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
                 WHERE conversation_id = ?3 AND conversation_type = ?4 AND sent_at = ?5",
                params![status, now, channel_id.to_string(), conversation_type.as_str(), ts as i64],
            )?;
            total += rows;
        }
        Ok(total)
    }

    /// List all pinned messages for a conversation.
    /// Alias for `list_pinned()` matching old MessageRepository API.
    pub fn list_pinned_for_conversation(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<Vec<Message>, StorageError> {
        self.list_pinned(channel_id, conversation_type)
    }

    /// List all pinned messages in a channel (no conversation_type filter).
    pub fn list_pinned_all(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<Vec<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM messages \
             WHERE conversation_id = ?1 AND conversation_type = ?2 \
               AND is_pinned = 1 AND is_deleted = 0 \
             ORDER BY sent_at DESC",
            MSG_COLS,
        ))?;
        let mut rows = stmt.query(params![channel_id.to_string(), conversation_type.as_str()])?;
        collect_messages(&mut rows)
    }

    /// List recent messages by sender.
    pub fn list_by_sender(
        &self,
        sender_id: &UserId,
        limit: u32,
    ) -> Result<Vec<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM messages WHERE sender_id = ?1 ORDER BY sent_at DESC LIMIT ?2",
            MSG_COLS,
        ))?;
        let mut rows = stmt.query(params![sender_id.to_string(), limit])?;
        collect_messages(&mut rows)
    }

    // ========================================================================
    // Internal helpers
    // ========================================================================

    /// Rebuild `reaction_counts_blob` and `current_user_reactions_blob` on the
    /// message row from the `unified_reactions` table (source of truth).
    fn rebuild_reaction_blobs_with_conn(
        &self,
        conn: &rusqlite::Connection,
        message_id: &MessageId,
        current_user_id: &UserId,
    ) -> Result<(), StorageError> {
        let mid = message_id.to_string();
        let uid = current_user_id.to_string();

        // Reaction counts
        let mut stmt = conn.prepare(
            "SELECT emoji, COUNT(*) FROM unified_reactions WHERE message_id = ?1 GROUP BY emoji",
        )?;
        let mut rows = stmt.query(params![&mid])?;
        let mut counts: Vec<ReactionCount> = Vec::new();
        while let Some(row) = rows.next()? {
            let emoji_str: String = row.get(0)?;
            let count: u32 = row.get(1)?;
            counts.push(ReactionCount::new(emoji_str, count));
        }
        drop(rows);
        drop(stmt);

        // Current user's reactions
        let mut stmt2 = conn.prepare(
            "SELECT emoji FROM unified_reactions WHERE message_id = ?1 AND user_id = ?2",
        )?;
        let mut rows2 = stmt2.query(params![&mid, &uid])?;
        let mut user_reactions: Vec<String> = Vec::new();
        while let Some(row) = rows2.next()? {
            user_reactions.push(row.get(0)?);
        }
        drop(rows2);
        drop(stmt2);

        let counts_blob = db_serialize(&counts)?;
        let user_blob = db_serialize(&user_reactions)?;

        conn.execute(
            "UPDATE messages SET reaction_counts_blob = ?1, current_user_reactions_blob = ?2, updated_at = ?3
             WHERE id = ?4",
            params![counts_blob, user_blob, Utc::now().to_rfc3339(), &mid],
        )?;

        Ok(())
    }
}

// ============================================================================
// Column list & row mappers
// ============================================================================

/// SELECT column list for unified messages — must match `row_to_message` column indices.
const MSG_COLS: &str =
    "id, conversation_id, conversation_type, sender_id, device_id, \
     content_json, encrypted_content, \
     thread_id, reply_to, attachments_blob, \
     is_pinned, is_edited, is_deleted, is_read, \
     sent_at, created_at, updated_at, edited_at, \
     reaction_counts_blob, current_user_reactions_blob, \
     mentioned_integrations_blob, targets_ai_bot, delivery_status, \
     reply_to_author_id, reply_to_text";

/// Parse an RFC3339 datetime, falling back to now.
fn parse_datetime(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

/// Content type → integer discriminant.
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

/// Map a row to Message.
///
/// Column order: id(0), conversation_id(1), conversation_type(2), sender_id(3),
/// device_id(4), content_json(5), encrypted_content(6), thread_id(7), reply_to(8),
/// attachments_blob(9), is_pinned(10), is_edited(11), is_deleted(12), is_read(13),
/// sent_at(14), created_at(15), updated_at(16), edited_at(17),
/// reaction_counts_blob(18), current_user_reactions_blob(19),
/// mentioned_integrations_blob(20), targets_ai_bot(21), delivery_status(22),
/// reply_to_author_id(23), reply_to_text(24)
fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Message, StorageError>> {
    let content_json: String = row.get(5)?;
    let attachments_blob: Vec<u8> = row.get(9)?;
    let reaction_counts_blob: Vec<u8> = row.get(18)?;
    let current_user_reactions_blob: Vec<u8> = row.get(19)?;
    let mentioned_integrations_blob: Vec<u8> = row.get(20)?;

    let conversation_type_str: String = row.get(2)?;
    let sender_id: Option<String> = row.get(3)?;
    let device_id: Option<i64> = row.get(4)?;
    let encrypted_content: Option<Vec<u8>> = row.get(6)?;
    let thread_id: Option<String> = row.get(7)?;
    let reply_to: Option<String> = row.get(8)?;
    let edited_at: Option<String> = row.get(17)?;

    let content: MessageContent = match serde_json::from_str(&content_json) {
        Ok(c) => c,
        Err(e) => {
            return Ok(Err(StorageError::InvalidData(format!(
                "Invalid content JSON: {}",
                e
            ))))
        }
    };

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
    let mentioned_integrations: Vec<IntegrationId> =
        match db_deserialize(&mentioned_integrations_blob) {
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
        conversation_type: match ConversationType::from_str_value(&conversation_type_str) {
            Ok(ct) => ct,
            Err(e) => return Ok(Err(e)),
        },
        sender_id: sender_id
            .map(|s| s.parse())
            .transpose()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        device_id: device_id.map(|d| d as u32),
        content,
        encrypted_content,
        thread_id: thread_id
            .map(|s| s.parse())
            .transpose()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        reply_to: reply_to
            .map(|s| s.parse())
            .transpose()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        reply_to_author_id: row
            .get::<_, Option<String>>(23)?
            .and_then(|s| s.parse().ok()),
        reply_to_text: row.get::<_, Option<String>>(24)?,
        attachments,
        is_pinned: row.get::<_, i32>(10)? != 0,
        is_edited: row.get::<_, i32>(11)? != 0,
        is_deleted: row.get::<_, i32>(12)? != 0,
        is_read: row.get::<_, i32>(13)? != 0,
        sent_at: row.get::<_, i64>(14)? as u64,
        created_at: parse_datetime(&row.get::<_, String>(15)?),
        updated_at: parse_datetime(&row.get::<_, String>(16)?),
        edited_at: edited_at.map(|s| parse_datetime(&s)),
        reaction_counts,
        current_user_reactions,
        mentioned_integrations,
        targets_ai_bot: row.get::<_, i32>(21)? != 0,
        delivery_status: row.get::<_, String>(22).unwrap_or_default(),
    }))
}

/// Map a unified_reactions row to Reaction.
fn row_to_reaction(row: &rusqlite::Row<'_>) -> Result<Reaction, StorageError> {
    let id_str: String = row.get(0).map_err(StorageError::Database)?;
    let msg_id_str: String = row.get(1).map_err(StorageError::Database)?;
    let user_id_str: String = row.get(2).map_err(StorageError::Database)?;
    let emoji_str: String = row.get(3).map_err(StorageError::Database)?;
    let created_at_str: String = row.get(4).map_err(StorageError::Database)?;

    Ok(Reaction {
        id: id_str
            .parse()
            .map_err(|e| StorageError::InvalidData(format!("Invalid reaction ID: {}", e)))?,
        message_id: msg_id_str
            .parse()
            .map_err(|e| StorageError::InvalidData(format!("Invalid message ID: {}", e)))?,
        user_id: user_id_str
            .parse()
            .map_err(|e| StorageError::InvalidData(format!("Invalid user ID: {}", e)))?,
        emoji: Emoji::unicode(&emoji_str),
        created_at: parse_datetime(&created_at_str),
    })
}

/// Map a row to Thread.
fn row_to_thread(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Thread, StorageError>> {
    let participant_ids_blob: Vec<u8> = row.get(4)?;
    let last_reply_at: Option<String> = row.get(3)?;

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
        let msg = row_to_message(row)?;
        messages.push(msg?);
    }
    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::message::{EncryptedContent, EncryptedMessageType};

    #[test]
    fn test_content_type_discriminant_covers_all_variants() {
        // Ensure every MessageContent variant has a discriminant
        let text = MessageContent::Text("hello".to_string());
        assert_eq!(content_type_discriminant(&text), 0);

        let encrypted = MessageContent::Encrypted(EncryptedContent {
            ciphertext: vec![1, 2, 3],
            sender_key_id: None,
            message_type: EncryptedMessageType::Signal,
        });
        assert_eq!(content_type_discriminant(&encrypted), 1);
    }

    #[test]
    fn test_like_pattern_escaping() {
        // LIKE special chars are escaped so they match literally
        let query = "100% done_now";
        let pattern = format!(
            "%{}%",
            query.replace('%', "\\%").replace('_', "\\_")
        );
        assert_eq!(pattern, "%100\\% done\\_now%");
    }
}
