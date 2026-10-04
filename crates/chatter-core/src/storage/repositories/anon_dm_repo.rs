//! Anonymous DM conversation repository.
//!
//! Data access for client-side managed anonymous DM conversations.
//! Message operations are handled by the unified MessageRepository.

use std::str::FromStr;

use crate::domain::anon_dm::{AnonDm, AnonDmId, AnonDmMailbox, AnonDmStatus};
use crate::domain::UserId;
use crate::error::StorageError;
use crate::storage::Database;
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
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
            "INSERT INTO anon_dm_conversations (id, peer_id, initiated_by_us, status, peer_mailbox_pk, peer_profile_key, peer_display_name, peer_avatar, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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
            ],
        )?;
        Ok(())
    }

    /// Get an anonymous DM conversation by ID.
    pub fn get_conversation(&self, id: &AnonDmId) -> Result<Option<AnonDm>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, peer_id, initiated_by_us, status, peer_mailbox_pk, peer_profile_key, peer_display_name, peer_avatar, created_at, updated_at, \
             (SELECT MAX(created_at) FROM messages WHERE conversation_id = anon_dm_conversations.id AND conversation_type = 'anon_dm' AND is_deleted = 0) as last_message_at, \
             (SELECT COUNT(*) FROM messages WHERE conversation_id = anon_dm_conversations.id AND conversation_type = 'anon_dm' AND is_read = 0 AND is_deleted = 0) as unread_count \
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
            "SELECT id, peer_id, initiated_by_us, status, peer_mailbox_pk, peer_profile_key, peer_display_name, peer_avatar, created_at, updated_at, \
             (SELECT MAX(created_at) FROM messages WHERE conversation_id = anon_dm_conversations.id AND conversation_type = 'anon_dm' AND is_deleted = 0) as last_message_at, \
             (SELECT COUNT(*) FROM messages WHERE conversation_id = anon_dm_conversations.id AND conversation_type = 'anon_dm' AND is_read = 0 AND is_deleted = 0) as unread_count \
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
            "UPDATE anon_dm_conversations SET status = ?1, peer_mailbox_pk = ?2, peer_profile_key = ?3, peer_display_name = ?4, peer_avatar = ?5, updated_at = ?6
             WHERE id = ?7",
            params![
                dm.status.to_i32(),
                dm.peer_mailbox_pk.as_ref().map(|k| k.as_slice()),
                dm.peer_profile_key.as_ref().map(|k| k.as_slice()),
                dm.peer_display_name.as_deref(),
                dm.peer_avatar.as_deref(),
                dm.updated_at.to_rfc3339(),
                dm.id.to_string(),
            ],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("AnonDm {}", dm.id)));
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
            "SELECT id, peer_id, initiated_by_us, status, peer_mailbox_pk, peer_profile_key, peer_display_name, peer_avatar, created_at, updated_at, \
             (SELECT MAX(created_at) FROM messages WHERE conversation_id = anon_dm_conversations.id AND conversation_type = 'anon_dm' AND is_deleted = 0) as last_msg_at, \
             (SELECT COUNT(*) FROM messages WHERE conversation_id = anon_dm_conversations.id AND conversation_type = 'anon_dm' AND is_read = 0 AND is_deleted = 0) as unread_count \
             FROM anon_dm_conversations ORDER BY last_msg_at DESC NULLS LAST, updated_at DESC",
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
            "SELECT id, peer_id, initiated_by_us, status, peer_mailbox_pk, peer_profile_key, peer_display_name, peer_avatar, created_at, updated_at, \
             (SELECT MAX(created_at) FROM messages WHERE conversation_id = anon_dm_conversations.id AND conversation_type = 'anon_dm' AND is_deleted = 0) as last_msg_at, \
             (SELECT COUNT(*) FROM messages WHERE conversation_id = anon_dm_conversations.id AND conversation_type = 'anon_dm' AND is_read = 0 AND is_deleted = 0) as unread_count \
             FROM anon_dm_conversations WHERE status = ?1 ORDER BY last_msg_at DESC NULLS LAST, updated_at DESC",
        )?;

        let mut rows = stmt.query(params![AnonDmStatus::Active.to_i32()])?;
        let mut dms = Vec::new();

        while let Some(row) = rows.next()? {
            dms.push(row_to_conversation(row)?);
        }

        Ok(dms)
    }

    /// List pending message requests (incoming only).
    ///
    /// Filters by `initiated_by_us = 0` so that conversations the local user
    /// initiated themselves do NOT appear here. Without this filter, the
    /// caller's own outgoing friend-code requests would surface as fake
    /// "incoming" requests in the message-requests UI, producing duplicate
    /// Accept/Decline cards and confusing the user (and any acceptance
    /// attempt would fail because chatter-core only accepts requests where
    /// the local user is the recipient, not the initiator).
    pub fn list_message_requests(&self) -> Result<Vec<AnonDm>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, peer_id, initiated_by_us, status, peer_mailbox_pk, peer_profile_key, peer_display_name, peer_avatar, created_at, updated_at, \
             (SELECT MAX(created_at) FROM messages WHERE conversation_id = anon_dm_conversations.id AND conversation_type = 'anon_dm' AND is_deleted = 0) as last_msg_at, \
             (SELECT COUNT(*) FROM messages WHERE conversation_id = anon_dm_conversations.id AND conversation_type = 'anon_dm' AND is_read = 0 AND is_deleted = 0) as unread_count \
             FROM anon_dm_conversations WHERE status = ?1 AND initiated_by_us = 0 ORDER BY created_at DESC",
        )?;

        let mut rows = stmt.query(params![AnonDmStatus::Pending.to_i32()])?;
        let mut dms = Vec::new();

        while let Some(row) = rows.next()? {
            dms.push(row_to_conversation(row)?);
        }

        Ok(dms)
    }

    /// List outgoing pending requests (initiated by the local user, awaiting
    /// the peer's acceptance).
    ///
    /// Filters by `initiated_by_us = 1`. Without this filter, this method
    /// would return the same set as `list_message_requests` and the two
    /// would be indistinguishable.
    pub fn list_outgoing_requests(&self) -> Result<Vec<AnonDm>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, peer_id, initiated_by_us, status, peer_mailbox_pk, peer_profile_key, peer_display_name, peer_avatar, created_at, updated_at, \
             (SELECT MAX(created_at) FROM messages WHERE conversation_id = anon_dm_conversations.id AND conversation_type = 'anon_dm' AND is_deleted = 0) as last_msg_at, \
             (SELECT COUNT(*) FROM messages WHERE conversation_id = anon_dm_conversations.id AND conversation_type = 'anon_dm' AND is_read = 0 AND is_deleted = 0) as unread_count \
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
            "SELECT COUNT(*) FROM messages m \
             JOIN anon_dm_conversations c ON m.conversation_id = c.id \
             WHERE m.conversation_type = 'anon_dm' \
               AND c.status = ?1 \
               AND m.is_read = 0 \
               AND m.is_deleted = 0",
            params![AnonDmStatus::Active.to_i32()],
            |row| row.get(0),
        )?;
        Ok(count as u32)
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

    // Message CRUD methods have been removed.
    // All message operations now use the unified MessageRepository.

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

    /// Remember a blocked peer's name on their block row, so it outlives the
    /// conversation row that normally holds it. No-op for an unblocked peer.
    pub fn set_blocked_display_name(&self, user_id: &UserId, name: &str) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "UPDATE anon_dm_blocked_users SET display_name = ?2 WHERE user_id = ?1",
            params![user_id.as_bytes().as_slice(), name],
        )?;
        Ok(())
    }

    /// The name stored on a blocked peer's block row, if any.
    pub fn get_blocked_display_name(&self, user_id: &UserId) -> Result<Option<String>, StorageError> {
        let conn = self.db.read_connection()?;
        let name = conn
            .query_row(
                "SELECT display_name FROM anon_dm_blocked_users WHERE user_id = ?1",
                params![user_id.as_bytes().as_slice()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(name)
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
    ) -> Result<Option<crate::domain::AnonRateLimitEntry>, StorageError> {
        super::repo_utils::get_rate_limit(&self.db, "dm", conversation_id.to_string().as_bytes())
    }

    /// Increment rate limit counter, resetting window if expired.
    pub fn increment_rate_limit(
        &self,
        conversation_id: &AnonDmId,
        window_duration: chrono::Duration,
    ) -> Result<u32, StorageError> {
        super::repo_utils::increment_rate_limit(&self.db, "dm", conversation_id.to_string().as_bytes(), window_duration)
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

    let peer_mailbox_pk_arr = vec_to_array_32(peer_mailbox_pk);
    let peer_profile_key_arr = vec_to_array_32(peer_profile_key);

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

    let public_key = require_array_32(&public_key_vec)?;
    let private_key = require_array_64(&private_key_vec)?;

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

use super::repo_utils::{require_array_32, require_array_64, vec_to_array_32};
