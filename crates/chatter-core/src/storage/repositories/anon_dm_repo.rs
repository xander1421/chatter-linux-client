//! Anonymous DM conversation repository.
//!
//! Data access for client-side managed anonymous DM conversations.
//! Message operations are handled by the unified MessageRepository.

use std::str::FromStr;

use crate::domain::UserId;
use crate::domain::anon_dm::{AnonDm, AnonDmId, AnonDmMailbox, AnonDmStatus, RequestState};
use crate::error::StorageError;
use crate::storage::Database;
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
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
            "INSERT INTO anon_dm_conversations (id, peer_id, initiated_by_us, status, peer_mailbox_pk, peer_profile_key, peer_display_name, peer_avatar, created_at, updated_at, peer_device_id, request_state)
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
                dm.peer_device_id,
                dm.request as i32,
            ],
        )?;
        Ok(())
    }

    /// Keep a peer's previous chat and its messages, but let a new incoming
    /// request become the only live chat. Both changes must commit together.
    pub fn insert_incoming_replacing_live(&self, dm: &AnonDm) -> Result<(), StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE anon_dm_conversations SET status = ?1, updated_at = ?2
             WHERE peer_id = ?3 AND status IN (?4, ?5, ?6)",
            params![
                AnonDmStatus::Superseded.to_i32(),
                Utc::now().to_rfc3339(),
                dm.peer_id.to_string(),
                AnonDmStatus::Active.to_i32(),
                AnonDmStatus::Blocked.to_i32(),
                AnonDmStatus::Pending.to_i32(),
            ],
        )?;
        tx.execute(
            "INSERT INTO anon_dm_conversations (id, peer_id, initiated_by_us, status, peer_mailbox_pk, peer_profile_key, peer_display_name, peer_avatar, created_at, updated_at, peer_device_id, request_state)
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
                dm.peer_device_id,
                dm.request as i32,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Bind the conversation to `device_id` of the peer. First pin wins: a
    /// conversation already bound to another device is refused — a secret
    /// chat never moves to another of the peer's devices. Not part of
    /// `update_conversation`, so a stale snapshot can never unpin or re-pin it.
    pub fn pin_peer_device(&self, id: &AnonDmId, device_id: u32) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE anon_dm_conversations SET peer_device_id = ?1
             WHERE id = ?2 AND (peer_device_id IS NULL OR peer_device_id = ?1)",
            params![device_id, id.to_string()],
        )?;
        if rows == 0 {
            return Err(StorageError::InvalidData(format!(
                "AnonDm {id} is missing or bound to another peer device"
            )));
        }
        Ok(())
    }

    /// Record how far our request for this conversation got
    /// ([`RequestState`]).
    pub fn set_request_state(
        &self,
        id: &AnonDmId,
        state: RequestState,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE anon_dm_conversations SET request_state = ?2 WHERE id = ?1",
            params![id.to_string(), state as i32],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("AnonDm {id}")));
        }
        Ok(())
    }

    /// Get an anonymous DM conversation by ID.
    pub fn get_conversation(&self, id: &AnonDmId) -> Result<Option<AnonDm>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, peer_id, initiated_by_us, status, peer_mailbox_pk, peer_profile_key, peer_display_name, peer_avatar, created_at, updated_at, \
             (SELECT MAX(created_at) FROM messages WHERE conversation_id = anon_dm_conversations.id AND conversation_type = 'anon_dm' AND is_deleted = 0) as last_message_at, \
             (SELECT COUNT(*) FROM messages WHERE conversation_id = anon_dm_conversations.id AND conversation_type = 'anon_dm' AND is_read = 0 AND is_deleted = 0) as unread_count, peer_device_id, request_state \
             FROM anon_dm_conversations WHERE id = ?1",
        )?;

        let result = stmt.query_row(params![id.to_string()], row_to_conversation);

        match result {
            Ok(dm) => Ok(Some(dm)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// The live secret chat with `peer_id`: Active first, then Blocked, then
    /// a pending request, preferring message history within each status. Declined
    /// and Superseded chats are never live. One peer can hold several rows
    /// (a declined request next to a new one, a chat per peer device, requests
    /// crossing), so this is a
    /// deterministic pick for peer-level questions ("is there a chat with
    /// them?"). Anything acting on one specific chat goes by its id.
    pub fn get_live_conversation_by_peer(
        &self,
        peer_id: &UserId,
    ) -> Result<Option<AnonDm>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, peer_id, initiated_by_us, status, peer_mailbox_pk, peer_profile_key, peer_display_name, peer_avatar, created_at, updated_at, \
             (SELECT MAX(created_at) FROM messages WHERE conversation_id = anon_dm_conversations.id AND conversation_type = 'anon_dm' AND is_deleted = 0) as last_message_at, \
             (SELECT COUNT(*) FROM messages WHERE conversation_id = anon_dm_conversations.id AND conversation_type = 'anon_dm' AND is_read = 0 AND is_deleted = 0) as unread_count, peer_device_id, request_state \
             FROM anon_dm_conversations WHERE peer_id = ?1 AND status IN (?2, ?3, ?4) \
             ORDER BY CASE status WHEN ?2 THEN 0 WHEN ?3 THEN 1 ELSE 2 END, last_message_at DESC NULLS LAST, created_at DESC, id DESC LIMIT 1",
        )?;
        stmt.query_row(
            params![
                peer_id.to_string(),
                AnonDmStatus::Active.to_i32(),
                AnonDmStatus::Blocked.to_i32(),
                AnonDmStatus::Pending.to_i32(),
            ],
            row_to_conversation,
        )
        .optional()
        .map_err(StorageError::Database)
    }

    /// Ids of the Declined chats with `peer_id`.
    pub fn declined_conversation_ids_by_peer(
        &self,
        peer_id: &UserId,
    ) -> Result<Vec<AnonDmId>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn
            .prepare("SELECT id FROM anon_dm_conversations WHERE peer_id = ?1 AND status = ?2")?;
        let ids = stmt
            .query_map(
                params![peer_id.to_string(), AnonDmStatus::Declined.to_i32()],
                |row| {
                    row.get::<_, String>(0)?
                        .parse()
                        .map_err(|_| rusqlite::Error::InvalidQuery)
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
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

    /// Set the peer's avatar alone.
    pub fn set_peer_avatar(&self, id: &AnonDmId, avatar: &[u8]) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE anon_dm_conversations SET peer_avatar = ?1, updated_at = ?2 WHERE id = ?3",
            params![avatar, Utc::now().to_rfc3339(), id.to_string()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("AnonDm {id}")));
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
             (SELECT COUNT(*) FROM messages WHERE conversation_id = anon_dm_conversations.id AND conversation_type = 'anon_dm' AND is_read = 0 AND is_deleted = 0) as unread_count, peer_device_id, request_state \
             FROM anon_dm_conversations ORDER BY last_msg_at DESC NULLS LAST, updated_at DESC",
        )?;

        let mut rows = stmt.query([])?;
        super::repo_utils::collect_decoded_rows(&mut rows, "anonymous_dm", |row| {
            row_to_conversation(row).map_err(StorageError::Database)
        })
    }

    /// List active conversations only.
    pub fn list_active_conversations(&self) -> Result<Vec<AnonDm>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, peer_id, initiated_by_us, status, peer_mailbox_pk, peer_profile_key, peer_display_name, peer_avatar, created_at, updated_at, \
             (SELECT MAX(created_at) FROM messages WHERE conversation_id = anon_dm_conversations.id AND conversation_type = 'anon_dm' AND is_deleted = 0) as last_msg_at, \
             (SELECT COUNT(*) FROM messages WHERE conversation_id = anon_dm_conversations.id AND conversation_type = 'anon_dm' AND is_read = 0 AND is_deleted = 0) as unread_count, peer_device_id, request_state \
             FROM anon_dm_conversations WHERE status = ?1 ORDER BY last_msg_at DESC NULLS LAST, updated_at DESC",
        )?;

        let mut rows = stmt.query(params![AnonDmStatus::Active.to_i32()])?;
        super::repo_utils::collect_decoded_rows(&mut rows, "anonymous_dm", |row| {
            row_to_conversation(row).map_err(StorageError::Database)
        })
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
             (SELECT COUNT(*) FROM messages WHERE conversation_id = anon_dm_conversations.id AND conversation_type = 'anon_dm' AND is_read = 0 AND is_deleted = 0) as unread_count, peer_device_id, request_state \
             FROM anon_dm_conversations WHERE status = ?1 AND initiated_by_us = 0 ORDER BY created_at DESC",
        )?;

        let mut rows = stmt.query(params![AnonDmStatus::Pending.to_i32()])?;
        super::repo_utils::collect_decoded_rows(&mut rows, "anonymous_dm", |row| {
            row_to_conversation(row).map_err(StorageError::Database)
        })
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
             (SELECT COUNT(*) FROM messages WHERE conversation_id = anon_dm_conversations.id AND conversation_type = 'anon_dm' AND is_read = 0 AND is_deleted = 0) as unread_count, peer_device_id, request_state \
             FROM anon_dm_conversations WHERE status = ?1 AND initiated_by_us = 1 ORDER BY created_at DESC",
        )?;

        let mut rows = stmt.query(params![AnonDmStatus::Pending.to_i32()])?;
        super::repo_utils::collect_decoded_rows(&mut rows, "anonymous_dm", |row| {
            row_to_conversation(row).map_err(StorageError::Database)
        })
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
    pub fn get_mailbox(
        &self,
        conversation_id: &AnonDmId,
    ) -> Result<Option<AnonDmMailbox>, StorageError> {
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
        super::repo_utils::collect_decoded_rows(&mut rows, "anonymous_dm_mailbox", |row| {
            row_to_mailbox(row).map_err(StorageError::Database)
        })
    }

    /// Get mailbox keys for all active conversations (for polling).
    ///
    /// Returns (conversation_id, public_key, private_key) tuples.
    #[allow(clippy::type_complexity)]
    pub fn get_active_mailbox_keys(
        &self,
    ) -> Result<Vec<(AnonDmId, [u8; 32], [u8; 64])>, StorageError> {
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
            params![id, user_id.as_bytes().as_slice(), now.to_rfc3339(), reason,],
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
    pub fn set_blocked_display_name(
        &self,
        user_id: &UserId,
        name: &str,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "UPDATE anon_dm_blocked_users SET display_name = ?2 WHERE user_id = ?1",
            params![user_id.as_bytes().as_slice(), name],
        )?;
        Ok(())
    }

    /// The name stored on a blocked peer's block row, if any.
    pub fn get_blocked_display_name(
        &self,
        user_id: &UserId,
    ) -> Result<Option<String>, StorageError> {
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
        super::repo_utils::increment_rate_limit(
            &self.db,
            "dm",
            conversation_id.to_string().as_bytes(),
            window_duration,
        )
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
    let peer_device_id: Option<u32> = row.get(12)?;
    let request_state: i32 = row.get(13)?;

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
        peer_device_id,
        request: RequestState::from_i32(request_state),
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
        conversation_id: conversation_id_str
            .parse()
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
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

#[cfg(test)]
mod corrupt_mailbox_tests {
    use super::*;
    #[test]
    fn corrupt_mailbox_key_does_not_hide_other_polling_mailboxes() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open(&dir.path().join("mailboxes.db"), "fixture").unwrap());
        db.run_migrations().unwrap();
        let repo = AnonDmRepository::new(db.clone());
        let mut conversations = Vec::new();
        for id in ["good", "bad"] {
            let conversation = AnonDm::new_initiated(UserId::new());
            repo.insert_conversation(&conversation).unwrap();
            repo.insert_mailbox(&AnonDmMailbox {
                id: id.into(),
                conversation_id: conversation.id,
                public_key: [1; 32],
                private_key: [2; 64],
                token_id: "fixture".into(),
                created_at: Utc::now(),
                expires_at: Utc::now() + chrono::Duration::days(1),
            })
            .unwrap();
            conversations.push(conversation.id);
        }
        db.connection()
            .unwrap()
            .execute(
                "UPDATE anon_dm_mailboxes SET private_key=X'00' WHERE id='bad'",
                [],
            )
            .unwrap();
        let mailboxes = repo.get_active_mailboxes().unwrap();
        assert_eq!(mailboxes.len(), 1);
        assert_eq!(mailboxes[0].id, "good");
        assert!(repo.get_mailbox(&conversations[1]).is_err());
    }
}
