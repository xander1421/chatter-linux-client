//! Anonymous DM repository.
//!
//! Data access for client-side managed anonymous DM conversations.

use std::str::FromStr;

use crate::domain::anon_dm::{
    AnonDm, AnonDmId, AnonDmMailbox, AnonDmMessage, AnonDmMessageId, AnonDmStatus,
};
use crate::domain::UserId;
use crate::error::StorageError;
use crate::storage::Database;
use chrono::{DateTime, Utc};
use rusqlite::params;
use std::sync::Arc;

/// Anonymous DM data repository.
///
/// Provides CRUD operations for anonymous DM conversations.
pub struct AnonDmRepository {
    db: Arc<Database>,
}

impl AnonDmRepository {
    /// Create a new AnonDmRepository with the given database.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    // ========================================================================
    // Conversation CRUD
    // ========================================================================

    /// Insert a new anonymous DM conversation.
    pub fn insert_conversation(&self, dm: &AnonDm) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT INTO anon_dm_conversations (id, peer_id, initiated_by_us, status, peer_mailbox_pk, peer_profile_key, peer_display_name, peer_avatar, created_at, updated_at, last_message_at, unread_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                dm.id.to_string(),
                dm.peer_id.to_string(),
                dm.initiated_by_us as i32,
                dm.status.to_i32(),
                dm.peer_mailbox_pk.as_ref().map(|k| k.as_slice()),
                dm.peer_profile_key.as_ref().map(|k| k.as_slice()),
                dm.peer_display_name.as_deref(),
                dm.peer_avatar.as_deref(),
                dm.created_at.to_rfc3339(),
                dm.updated_at.to_rfc3339(),
                dm.last_message_at.as_ref().map(|t| t.to_rfc3339()),
                dm.unread_count,
            ],
        )?;
        Ok(())
    }

    /// Get an anonymous DM conversation by ID.
    pub fn get_conversation(&self, id: &AnonDmId) -> Result<Option<AnonDm>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, peer_id, initiated_by_us, status, peer_mailbox_pk, peer_profile_key, peer_display_name, peer_avatar, created_at, updated_at, last_message_at, unread_count
             FROM anon_dm_conversations WHERE id = ?1",
        )?;

        let result = stmt.query_row(params![id.to_string()], row_to_conversation);

        match result {
            Ok(dm) => Ok(Some(dm)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Get an anonymous DM conversation by peer ID.
    pub fn get_conversation_by_peer(&self, peer_id: &UserId) -> Result<Option<AnonDm>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, peer_id, initiated_by_us, status, peer_mailbox_pk, peer_profile_key, peer_display_name, peer_avatar, created_at, updated_at, last_message_at, unread_count
             FROM anon_dm_conversations WHERE peer_id = ?1",
        )?;

        let result = stmt.query_row(params![peer_id.to_string()], row_to_conversation);

        match result {
            Ok(dm) => Ok(Some(dm)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Update an anonymous DM conversation.
    pub fn update_conversation(&self, dm: &AnonDm) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE anon_dm_conversations SET status = ?1, peer_mailbox_pk = ?2, peer_profile_key = ?3, peer_display_name = ?4, peer_avatar = ?5, updated_at = ?6, last_message_at = ?7, unread_count = ?8
             WHERE id = ?9",
            params![
                dm.status.to_i32(),
                dm.peer_mailbox_pk.as_ref().map(|k| k.as_slice()),
                dm.peer_profile_key.as_ref().map(|k| k.as_slice()),
                dm.peer_display_name.as_deref(),
                dm.peer_avatar.as_deref(),
                dm.updated_at.to_rfc3339(),
                dm.last_message_at.as_ref().map(|t| t.to_rfc3339()),
                dm.unread_count,
                dm.id.to_string(),
            ],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("AnonDm {}", dm.id)));
        }
        Ok(())
    }

    /// Update peer's display name.
    pub fn update_peer_display_name(
        &self,
        id: &AnonDmId,
        display_name: &str,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now();
        let rows = conn.execute(
            "UPDATE anon_dm_conversations SET peer_display_name = ?1, updated_at = ?2 WHERE id = ?3",
            params![display_name, now.to_rfc3339(), id.to_string()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("AnonDm {}", id)));
        }
        Ok(())
    }

    /// Update peer's avatar.
    pub fn update_peer_avatar(
        &self,
        id: &AnonDmId,
        avatar: &[u8],
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now();
        let rows = conn.execute(
            "UPDATE anon_dm_conversations SET peer_avatar = ?1, updated_at = ?2 WHERE id = ?3",
            params![avatar, now.to_rfc3339(), id.to_string()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("AnonDm {}", id)));
        }
        Ok(())
    }

    /// Update conversation status.
    pub fn update_status(&self, id: &AnonDmId, status: AnonDmStatus) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now();
        let rows = conn.execute(
            "UPDATE anon_dm_conversations SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![status.to_i32(), now.to_rfc3339(), id.to_string()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("AnonDm {}", id)));
        }
        Ok(())
    }

    /// Update peer's mailbox public key.
    pub fn update_peer_mailbox(
        &self,
        id: &AnonDmId,
        mailbox_pk: &[u8; 32],
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now();
        let rows = conn.execute(
            "UPDATE anon_dm_conversations SET peer_mailbox_pk = ?1, updated_at = ?2 WHERE id = ?3",
            params![mailbox_pk.as_slice(), now.to_rfc3339(), id.to_string()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("AnonDm {}", id)));
        }
        Ok(())
    }

    /// Update peer's profile key.
    pub fn update_peer_profile_key(
        &self,
        id: &AnonDmId,
        profile_key: &[u8; 32],
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now();
        let rows = conn.execute(
            "UPDATE anon_dm_conversations SET peer_profile_key = ?1, updated_at = ?2 WHERE id = ?3",
            params![profile_key.as_slice(), now.to_rfc3339(), id.to_string()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("AnonDm {}", id)));
        }
        Ok(())
    }

    /// Delete an anonymous DM conversation.
    pub fn delete_conversation(&self, id: &AnonDmId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "DELETE FROM anon_dm_conversations WHERE id = ?1",
            params![id.to_string()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("AnonDm {}", id)));
        }
        Ok(())
    }

    /// List all anonymous DM conversations.
    pub fn list_conversations(&self) -> Result<Vec<AnonDm>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, peer_id, initiated_by_us, status, peer_mailbox_pk, peer_profile_key, peer_display_name, peer_avatar, created_at, updated_at, last_message_at, unread_count
             FROM anon_dm_conversations ORDER BY last_message_at DESC NULLS LAST, updated_at DESC",
        )?;

        let mut rows = stmt.query([])?;
        let mut dms = Vec::new();

        while let Some(row) = rows.next()? {
            dms.push(row_to_conversation(row)?);
        }

        Ok(dms)
    }

    /// List active conversations only.
    pub fn list_active_conversations(&self) -> Result<Vec<AnonDm>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, peer_id, initiated_by_us, status, peer_mailbox_pk, peer_profile_key, peer_display_name, peer_avatar, created_at, updated_at, last_message_at, unread_count
             FROM anon_dm_conversations WHERE status = ?1 ORDER BY last_message_at DESC NULLS LAST, updated_at DESC",
        )?;

        let mut rows = stmt.query(params![AnonDmStatus::Active.to_i32()])?;
        let mut dms = Vec::new();

        while let Some(row) = rows.next()? {
            dms.push(row_to_conversation(row)?);
        }

        Ok(dms)
    }

    /// List pending message requests (incoming only).
    pub fn list_message_requests(&self) -> Result<Vec<AnonDm>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, peer_id, initiated_by_us, status, peer_mailbox_pk, peer_profile_key, peer_display_name, peer_avatar, created_at, updated_at, last_message_at, unread_count
             FROM anon_dm_conversations WHERE status = ?1 AND initiated_by_us = 0 ORDER BY created_at DESC",
        )?;

        let mut rows = stmt.query(params![AnonDmStatus::Pending.to_i32()])?;
        let mut dms = Vec::new();

        while let Some(row) = rows.next()? {
            dms.push(row_to_conversation(row)?);
        }

        Ok(dms)
    }

    /// List outgoing pending requests.
    pub fn list_outgoing_requests(&self) -> Result<Vec<AnonDm>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, peer_id, initiated_by_us, status, peer_mailbox_pk, peer_profile_key, peer_display_name, peer_avatar, created_at, updated_at, last_message_at, unread_count
             FROM anon_dm_conversations WHERE status = ?1 AND initiated_by_us = 1 ORDER BY created_at DESC",
        )?;

        let mut rows = stmt.query(params![AnonDmStatus::Pending.to_i32()])?;
        let mut dms = Vec::new();

        while let Some(row) = rows.next()? {
            dms.push(row_to_conversation(row)?);
        }

        Ok(dms)
    }

    /// Get total unread count across all conversations.
    pub fn get_total_unread_count(&self) -> Result<u32, StorageError> {
        let conn = self.db.read_connection()?;
        let count: i64 = conn.query_row(
            "SELECT COALESCE(SUM(unread_count), 0) FROM anon_dm_conversations WHERE status = ?1",
            params![AnonDmStatus::Active.to_i32()],
            |row| row.get(0),
        )?;
        Ok(count as u32)
    }

    /// Increment unread count for a conversation.
    pub fn increment_unread(&self, id: &AnonDmId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "UPDATE anon_dm_conversations SET unread_count = unread_count + 1, updated_at = ?1 WHERE id = ?2",
            params![Utc::now().to_rfc3339(), id.to_string()],
        )?;
        Ok(())
    }

    /// Mark all messages as read for a conversation.
    pub fn mark_as_read(&self, id: &AnonDmId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "UPDATE anon_dm_conversations SET unread_count = 0, updated_at = ?1 WHERE id = ?2",
            params![Utc::now().to_rfc3339(), id.to_string()],
        )?;
        conn.execute(
            "UPDATE anon_dm_messages SET is_read = 1 WHERE conversation_id = ?1 AND is_read = 0",
            params![id.to_string()],
        )?;
        Ok(())
    }

    // ========================================================================
    // Mailbox CRUD
    // ========================================================================

    /// Insert a new mailbox.
    pub fn insert_mailbox(&self, mailbox: &AnonDmMailbox) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT INTO anon_dm_mailboxes (id, conversation_id, public_key, private_key, token_id, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                mailbox.id,
                mailbox.conversation_id.to_string(),
                mailbox.public_key.as_slice(),
                mailbox.private_key.as_slice(),
                mailbox.token_id,
                mailbox.created_at.to_rfc3339(),
                mailbox.expires_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Get mailbox for a conversation.
    pub fn get_mailbox(&self, conversation_id: &AnonDmId) -> Result<Option<AnonDmMailbox>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, conversation_id, public_key, private_key, token_id, created_at, expires_at
             FROM anon_dm_mailboxes WHERE conversation_id = ?1",
        )?;

        let result = stmt.query_row(params![conversation_id.to_string()], row_to_mailbox);

        match result {
            Ok(mailbox) => Ok(Some(mailbox)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Delete mailbox for a conversation.
    pub fn delete_mailbox(&self, conversation_id: &AnonDmId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "DELETE FROM anon_dm_mailboxes WHERE conversation_id = ?1",
            params![conversation_id.to_string()],
        )?;
        Ok(())
    }

    /// Get expired mailboxes that need renewal.
    pub fn get_expired_mailboxes(&self) -> Result<Vec<AnonDmMailbox>, StorageError> {
        let conn = self.db.read_connection()?;
        let now = Utc::now().to_rfc3339();
        let mut stmt = conn.prepare(
            "SELECT id, conversation_id, public_key, private_key, token_id, created_at, expires_at
             FROM anon_dm_mailboxes WHERE expires_at < ?1",
        )?;

        let mut rows = stmt.query(params![now])?;
        let mut mailboxes = Vec::new();

        while let Some(row) = rows.next()? {
            mailboxes.push(row_to_mailbox(row)?);
        }

        Ok(mailboxes)
    }

    /// Get all active (non-expired) mailboxes for polling.
    ///
    /// Returns mailboxes for:
    /// - Active conversations (status = 1)
    /// - Pending conversations that WE initiated (status = 0 AND initiated_by_us = 1)
    ///   These need polling to receive the acceptance message.
    pub fn get_active_mailboxes(&self) -> Result<Vec<AnonDmMailbox>, StorageError> {
        let conn = self.db.read_connection()?;
        let now = Utc::now().to_rfc3339();
        let mut stmt = conn.prepare(
            "SELECT m.id, m.conversation_id, m.public_key, m.private_key, m.token_id, m.created_at, m.expires_at
             FROM anon_dm_mailboxes m
             INNER JOIN anon_dm_conversations c ON m.conversation_id = c.id
             WHERE m.expires_at > ?1 AND (c.status = 1 OR (c.status = 0 AND c.initiated_by_us = 1))",
        )?;

        let mut rows = stmt.query(params![now])?;
        let mut mailboxes = Vec::new();

        while let Some(row) = rows.next()? {
            mailboxes.push(row_to_mailbox(row)?);
        }

        Ok(mailboxes)
    }

    /// Get mailbox keys for all active conversations (for polling).
    ///
    /// Returns (conversation_id, public_key, private_key) tuples.
    #[allow(clippy::type_complexity)]
    pub fn get_active_mailbox_keys(&self) -> Result<Vec<(AnonDmId, [u8; 32], [u8; 64])>, StorageError> {
        let mailboxes = self.get_active_mailboxes()?;
        Ok(mailboxes
            .into_iter()
            .map(|m| (m.conversation_id, m.public_key, m.private_key))
            .collect())
    }

    // ========================================================================
    // Message CRUD
    // ========================================================================

    /// Insert a new message.
    pub fn insert_message(&self, msg: &AnonDmMessage) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT INTO anon_dm_messages (id, conversation_id, sender_id, content_type, content_json, attachments_blob, is_deleted, is_read, sent_at, created_at, delivery_status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                msg.id.as_str(),
                msg.conversation_id.to_string(),
                msg.sender_id.to_string(),
                msg.content_type,
                msg.content_json,
                msg.attachments_blob,
                msg.is_deleted as i32,
                msg.is_read as i32,
                msg.sent_at as i64,
                msg.created_at.to_rfc3339(),
                msg.delivery_status,
            ],
        )?;

        // Update conversation's last_message_at
        conn.execute(
            "UPDATE anon_dm_conversations SET last_message_at = ?1, updated_at = ?1 WHERE id = ?2",
            params![msg.created_at.to_rfc3339(), msg.conversation_id.to_string()],
        )?;

        Ok(())
    }

    /// Get a message by ID.
    pub fn get_message(&self, id: &AnonDmMessageId) -> Result<Option<AnonDmMessage>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, conversation_id, sender_id, content_type, content_json, attachments_blob, is_deleted, is_read, sent_at, created_at, delivery_status
             FROM anon_dm_messages WHERE id = ?1",
        )?;

        let result = stmt.query_row(params![id.as_str()], row_to_message);

        match result {
            Ok(msg) => Ok(Some(msg)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Get messages for a conversation with pagination.
    ///
    /// Messages are sorted by (sent_at, sender_id) DESC for deterministic ordering.
    /// sent_at is the sender's timestamp which is consistent across devices.
    pub fn get_messages(
        &self,
        conversation_id: &AnonDmId,
        before: Option<&DateTime<Utc>>,
        limit: i32,
    ) -> Result<Vec<AnonDmMessage>, StorageError> {
        let conn = self.db.read_connection()?;

        // Convert before time to nanoseconds for sent_at comparison
        let before_ns = before.map(|t| {
            t.timestamp_nanos_opt().unwrap_or(0) as u64
        });

        let (sql, params_vec): (&str, Vec<Box<dyn rusqlite::ToSql>>) = if let Some(before_nanos) = before_ns {
            (
                "SELECT id, conversation_id, sender_id, content_type, content_json, attachments_blob, is_deleted, is_read, sent_at, created_at, delivery_status
                 FROM anon_dm_messages
                 WHERE conversation_id = ?1 AND sent_at < ?2 AND is_deleted = 0
                 ORDER BY sent_at DESC, sender_id DESC LIMIT ?3",
                vec![
                    Box::new(conversation_id.to_string()),
                    Box::new(before_nanos as i64),
                    Box::new(limit),
                ],
            )
        } else {
            (
                "SELECT id, conversation_id, sender_id, content_type, content_json, attachments_blob, is_deleted, is_read, sent_at, created_at, delivery_status
                 FROM anon_dm_messages
                 WHERE conversation_id = ?1 AND is_deleted = 0
                 ORDER BY sent_at DESC, sender_id DESC LIMIT ?2",
                vec![
                    Box::new(conversation_id.to_string()),
                    Box::new(limit),
                ],
            )
        };

        let mut stmt = conn.prepare(sql)?;
        let params_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();
        let mut rows = stmt.query(params_refs.as_slice())?;
        let mut messages = Vec::new();

        while let Some(row) = rows.next()? {
            messages.push(row_to_message(row)?);
        }

        Ok(messages)
    }

    /// Get the latest message in a conversation.
    ///
    /// Uses (sent_at, sender_id) DESC for deterministic ordering.
    pub fn get_latest_message(
        &self,
        conversation_id: &AnonDmId,
    ) -> Result<Option<AnonDmMessage>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, conversation_id, sender_id, content_type, content_json, attachments_blob, is_deleted, is_read, sent_at, created_at, delivery_status
             FROM anon_dm_messages
             WHERE conversation_id = ?1 AND is_deleted = 0
             ORDER BY sent_at DESC, sender_id DESC LIMIT 1",
        )?;

        let result = stmt.query_row(params![conversation_id.to_string()], row_to_message);

        match result {
            Ok(msg) => Ok(Some(msg)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Get message count for a conversation.
    pub fn get_message_count(&self, conversation_id: &AnonDmId) -> Result<i64, StorageError> {
        let conn = self.db.read_connection()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM anon_dm_messages WHERE conversation_id = ?1 AND is_deleted = 0",
            params![conversation_id.to_string()],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Get a message by sender ID and timestamp (Signal-style lookup).
    ///
    /// This is used for context menu operations, reactions, etc. where we identify
    /// messages by (sender_id, sent_at) tuple per Signal Protocol convention.
    pub fn get_message_by_sender_timestamp(
        &self,
        conversation_id: &AnonDmId,
        sender_id: &UserId,
        timestamp_ns: u64,
    ) -> Result<Option<AnonDmMessage>, StorageError> {
        let conn = self.db.read_connection()?;

        // Find message by exact sent_at match (now that we store sent_at properly)
        let mut stmt = conn.prepare(
            "SELECT id, conversation_id, sender_id, content_type, content_json, attachments_blob, is_deleted, is_read, sent_at, created_at, delivery_status
             FROM anon_dm_messages
             WHERE conversation_id = ?1 AND sender_id = ?2 AND sent_at = ?3 AND is_deleted = 0
             LIMIT 1",
        )?;

        let result = stmt.query_row(
            params![
                conversation_id.to_string(),
                sender_id.to_string(),
                timestamp_ns as i64,
            ],
            row_to_message,
        );

        match result {
            Ok(msg) => Ok(Some(msg)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Soft delete a message.
    pub fn delete_message(&self, id: &AnonDmMessageId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE anon_dm_messages SET is_deleted = 1 WHERE id = ?1",
            params![id.as_str()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("AnonDmMessage {}", id)));
        }
        Ok(())
    }

    /// Soft delete a message by sender timestamp (for incoming delete-for-everyone).
    ///
    /// Uses (conversation_id, sender_id, sent_at) as the Signal-style lookup key.
    pub fn mark_message_deleted_by_timestamp(
        &self,
        conversation_id: &AnonDmId,
        sender_id: &UserId,
        sent_timestamp: u64,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;

        // Delete message by exact (conversation_id, sender_id, sent_at) match
        let rows = conn.execute(
            "UPDATE anon_dm_messages SET is_deleted = 1
             WHERE conversation_id = ?1 AND sender_id = ?2 AND sent_at = ?3 AND is_deleted = 0",
            params![
                conversation_id.to_string(),
                sender_id.to_string(),
                sent_timestamp as i64,
            ],
        )?;

        Ok(rows > 0)
    }

    /// Update the text content of a message by timestamp.
    ///
    /// Used for editing messages.
    pub fn update_message_text(
        &self,
        conversation_id: &AnonDmId,
        sent_timestamp: u64,
        new_text: &str,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;

        // Build new content_json with updated text
        let content_json = serde_json::json!({ "text": new_text }).to_string();

        let rows = conn.execute(
            "UPDATE anon_dm_messages SET content_json = ?1
             WHERE conversation_id = ?2 AND sent_at = ?3",
            params![
                content_json,
                conversation_id.to_string(),
                sent_timestamp as i64,
            ],
        )?;

        Ok(rows > 0)
    }

    // ========================================================================
    // Global User Blocking
    // ========================================================================

    /// Block a user globally for anonymous DMs.
    pub fn block_user(&self, user_id: &UserId, reason: Option<&str>) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now();
        let id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT OR REPLACE INTO anon_dm_blocked_users (id, user_id, blocked_at, reason)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                id,
                user_id.as_bytes().as_slice(),
                now.to_rfc3339(),
                reason,
            ],
        )?;
        Ok(())
    }

    /// Unblock a user globally.
    pub fn unblock_user(&self, user_id: &UserId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "DELETE FROM anon_dm_blocked_users WHERE user_id = ?1",
            params![user_id.as_bytes().as_slice()],
        )?;
        Ok(())
    }

    /// Check if a user is globally blocked.
    pub fn is_user_blocked(&self, user_id: &UserId) -> Result<bool, StorageError> {
        let conn = self.db.read_connection()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM anon_dm_blocked_users WHERE user_id = ?1",
            params![user_id.as_bytes().as_slice()],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Get all globally blocked users.
    pub fn get_blocked_users(&self) -> Result<Vec<UserId>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare("SELECT user_id FROM anon_dm_blocked_users")?;

        let mut rows = stmt.query([])?;
        let mut users = Vec::new();

        while let Some(row) = rows.next()? {
            let user_id_bytes: Vec<u8> = row.get(0)?;
            if let Some(user_id) = UserId::try_from_slice(&user_id_bytes) {
                users.push(user_id);
            }
        }

        Ok(users)
    }

    // ========================================================================
    // Rate Limiting
    // ========================================================================

    /// Get rate limit entry for a conversation.
    pub fn get_rate_limit(
        &self,
        conversation_id: &AnonDmId,
    ) -> Result<Option<RateLimitEntry>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, entity_type, entity_id, window_start, message_count, last_message_at
             FROM anon_rate_limits WHERE entity_type = 'dm' AND entity_id = ?1",
        )?;

        let result = stmt.query_row(params![conversation_id.to_string().as_bytes()], |row| {
            Ok(RateLimitEntry {
                id: row.get(0)?,
                entity_type: row.get(1)?,
                entity_id: row.get(2)?,
                window_start: parse_datetime(&row.get::<_, String>(3)?),
                message_count: row.get::<_, i32>(4)? as u32,
                last_message_at: parse_datetime(&row.get::<_, String>(5)?),
            })
        });

        match result {
            Ok(entry) => Ok(Some(entry)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Increment rate limit counter, resetting window if expired.
    pub fn increment_rate_limit(
        &self,
        conversation_id: &AnonDmId,
        window_duration: chrono::Duration,
    ) -> Result<u32, StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now();
        let entity_id = conversation_id.to_string().into_bytes();

        // Try to get existing entry
        if let Some(entry) = self.get_rate_limit(conversation_id)? {
            if entry.is_window_expired(window_duration) {
                // Reset window
                conn.execute(
                    "UPDATE anon_rate_limits SET window_start = ?1, message_count = 1, last_message_at = ?1
                     WHERE entity_type = 'dm' AND entity_id = ?2",
                    params![now.to_rfc3339(), entity_id],
                )?;
                Ok(1)
            } else {
                // Increment counter
                let new_count = entry.message_count + 1;
                conn.execute(
                    "UPDATE anon_rate_limits SET message_count = ?1, last_message_at = ?2
                     WHERE entity_type = 'dm' AND entity_id = ?3",
                    params![new_count as i32, now.to_rfc3339(), entity_id],
                )?;
                Ok(new_count)
            }
        } else {
            // Create new entry
            let id = uuid::Uuid::new_v4().to_string();
            conn.execute(
                "INSERT INTO anon_rate_limits (id, entity_type, entity_id, window_start, message_count, last_message_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, "dm", entity_id, now.to_rfc3339(), 1, now.to_rfc3339()],
            )?;
            Ok(1)
        }
    }
}

// ============================================================================
// Row Converters
// ============================================================================

fn row_to_conversation(row: &rusqlite::Row) -> Result<AnonDm, rusqlite::Error> {
    let id_str: String = row.get(0)?;
    let peer_id_str: String = row.get(1)?;
    let initiated_by_us: i32 = row.get(2)?;
    let status: i32 = row.get(3)?;
    let peer_mailbox_pk: Option<Vec<u8>> = row.get(4)?;
    let peer_profile_key: Option<Vec<u8>> = row.get(5)?;
    let peer_display_name: Option<String> = row.get(6)?;
    let peer_avatar: Option<Vec<u8>> = row.get(7)?;
    let created_at_str: String = row.get(8)?;
    let updated_at_str: String = row.get(9)?;
    let last_message_at_str: Option<String> = row.get(10)?;
    let unread_count: u32 = row.get(11)?;

    let peer_mailbox_pk_arr = peer_mailbox_pk.and_then(|v| {
        if v.len() == 32 {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&v);
            Some(arr)
        } else {
            None
        }
    });

    let peer_profile_key_arr = peer_profile_key.and_then(|v| {
        if v.len() == 32 {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&v);
            Some(arr)
        } else {
            None
        }
    });

    Ok(AnonDm {
        id: id_str.parse().map_err(|_| rusqlite::Error::InvalidQuery)?,
        peer_id: UserId::from_str(&peer_id_str).map_err(|_| rusqlite::Error::InvalidQuery)?,
        initiated_by_us: initiated_by_us != 0,
        status: AnonDmStatus::from_i32(status),
        peer_mailbox_pk: peer_mailbox_pk_arr,
        peer_profile_key: peer_profile_key_arr,
        peer_display_name,
        peer_avatar,
        created_at: DateTime::parse_from_rfc3339(&created_at_str)
            .map_err(|_| rusqlite::Error::InvalidQuery)?
            .with_timezone(&Utc),
        updated_at: DateTime::parse_from_rfc3339(&updated_at_str)
            .map_err(|_| rusqlite::Error::InvalidQuery)?
            .with_timezone(&Utc),
        last_message_at: last_message_at_str
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&Utc)),
        unread_count,
    })
}

fn row_to_mailbox(row: &rusqlite::Row) -> Result<AnonDmMailbox, rusqlite::Error> {
    let id: String = row.get(0)?;
    let conversation_id_str: String = row.get(1)?;
    let public_key_vec: Vec<u8> = row.get(2)?;
    let private_key_vec: Vec<u8> = row.get(3)?;
    let token_id: String = row.get(4)?;
    let created_at_str: String = row.get(5)?;
    let expires_at_str: String = row.get(6)?;

    let mut public_key = [0u8; 32];
    if public_key_vec.len() != 32 {
        return Err(rusqlite::Error::InvalidQuery);
    }
    public_key.copy_from_slice(&public_key_vec);

    let mut private_key = [0u8; 64];
    if private_key_vec.len() != 64 {
        return Err(rusqlite::Error::InvalidQuery);
    }
    private_key.copy_from_slice(&private_key_vec);

    Ok(AnonDmMailbox {
        id,
        conversation_id: conversation_id_str.parse().map_err(|_| rusqlite::Error::InvalidQuery)?,
        public_key,
        private_key,
        token_id,
        created_at: DateTime::parse_from_rfc3339(&created_at_str)
            .map_err(|_| rusqlite::Error::InvalidQuery)?
            .with_timezone(&Utc),
        expires_at: DateTime::parse_from_rfc3339(&expires_at_str)
            .map_err(|_| rusqlite::Error::InvalidQuery)?
            .with_timezone(&Utc),
    })
}

fn row_to_message(row: &rusqlite::Row) -> Result<AnonDmMessage, rusqlite::Error> {
    let id: String = row.get(0)?;
    let conversation_id_str: String = row.get(1)?;
    let sender_id_str: String = row.get(2)?;
    let content_type: i32 = row.get(3)?;
    let content_json: String = row.get(4)?;
    let attachments_blob: Option<Vec<u8>> = row.get(5)?;
    let is_deleted: i32 = row.get(6)?;
    let is_read: i32 = row.get(7)?;
    let sent_at: i64 = row.get(8)?;
    let created_at_str: String = row.get(9)?;
    let delivery_status: String = row.get(10)?;

    Ok(AnonDmMessage {
        id: AnonDmMessageId::from_string(id),
        conversation_id: conversation_id_str.parse().map_err(|_| rusqlite::Error::InvalidQuery)?,
        sender_id: UserId::from_str(&sender_id_str).map_err(|_| rusqlite::Error::InvalidQuery)?,
        content_type,
        content_json,
        attachments_blob: attachments_blob.unwrap_or_else(|| vec![8, 0, 0, 0, 0, 0, 0, 0]),
        is_deleted: is_deleted != 0,
        is_read: is_read != 0,
        reaction_counts: Vec::new(), // Will be populated by caller using joins
        current_user_reactions: Vec::new(), // Will be populated by caller using joins
        delivery_status,
        sent_at: sent_at as u64,
        created_at: DateTime::parse_from_rfc3339(&created_at_str)
            .map_err(|_| rusqlite::Error::InvalidQuery)?
            .with_timezone(&Utc),
    })
}

// ============================================================================
// Rate Limiting Types
// ============================================================================

/// Rate limit entry for tracking message frequency.
#[allow(dead_code)]
pub struct RateLimitEntry {
    pub id: String,
    pub entity_type: String,
    pub entity_id: Vec<u8>,
    pub window_start: DateTime<Utc>,
    pub message_count: u32,
    pub last_message_at: DateTime<Utc>,
}

impl RateLimitEntry {
    /// Check if the rate limit window has expired.
    pub fn is_window_expired(&self, window_duration: chrono::Duration) -> bool {
        Utc::now() > self.window_start + window_duration
    }
}

/// Parse a datetime string, falling back to now on error.
fn parse_datetime(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}
