//! Anonymous group repository.
//!
//! Data access for NDSS'21 style anonymous groups.

use crate::domain::anon_group::{
    AnonGroup, AnonGroupId, AnonGroupMailbox, AnonGroupMember, AnonGroupMessage,
    AnonGroupMessageId, AnonGroupPending, AnonGroupRole, PendingActionType,
};
use crate::domain::UserId;
use crate::error::StorageError;
use crate::storage::Database;
use chrono::{DateTime, Utc};
use rusqlite::params;
use std::sync::Arc;

/// Anonymous group data repository.
///
/// Provides CRUD operations for anonymous groups and their members.
pub struct AnonGroupRepository {
    db: Arc<Database>,
}

impl AnonGroupRepository {
    /// Create a new AnonGroupRepository with the given database.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    // ========================================================================
    // Group CRUD
    // ========================================================================

    /// Insert a new anonymous group.
    pub fn insert_group(&self, group: &AnonGroup) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT INTO anon_groups (id, name, my_role, bootstrap_complete, created_at, updated_at, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                group.id.as_bytes().as_slice(),
                group.name,
                group.my_role.to_i32(),
                group.bootstrap_complete as i32,
                group.created_at.to_rfc3339(),
                group.updated_at.to_rfc3339(),
                group.status.to_i32(),
            ],
        )?;
        Ok(())
    }

    /// Get an anonymous group by ID.
    pub fn get_group(&self, id: &AnonGroupId) -> Result<Option<AnonGroup>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, my_role, bootstrap_complete, created_at, updated_at, status
             FROM anon_groups WHERE id = ?1",
        )?;

        let result = stmt.query_row(params![id.as_bytes().as_slice()], row_to_group);

        match result {
            Ok(group) => Ok(Some(group)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Update an anonymous group.
    pub fn update_group(&self, group: &AnonGroup) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE anon_groups SET name = ?1, my_role = ?2, bootstrap_complete = ?3, updated_at = ?4, status = ?5
             WHERE id = ?6",
            params![
                group.name,
                group.my_role.to_i32(),
                group.bootstrap_complete as i32,
                group.updated_at.to_rfc3339(),
                group.status.to_i32(),
                group.id.as_bytes().as_slice(),
            ],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("AnonGroup {}", group.id)));
        }
        Ok(())
    }

    /// Update only the group name.
    pub fn update_group_name(&self, id: &AnonGroupId, name: &str) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let now = chrono::Utc::now();
        let rows = conn.execute(
            "UPDATE anon_groups SET name = ?1, updated_at = ?2 WHERE id = ?3",
            params![name, now.to_rfc3339(), id.as_bytes().as_slice()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("AnonGroup {}", id)));
        }
        Ok(())
    }

    /// Delete an anonymous group.
    pub fn delete_group(&self, id: &AnonGroupId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "DELETE FROM anon_groups WHERE id = ?1",
            params![id.as_bytes().as_slice()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("AnonGroup {}", id)));
        }
        Ok(())
    }

    /// List all anonymous groups.
    pub fn list_groups(&self) -> Result<Vec<AnonGroup>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, my_role, bootstrap_complete, created_at, updated_at, status
             FROM anon_groups ORDER BY updated_at DESC",
        )?;

        let mut rows = stmt.query([])?;
        let mut groups = Vec::new();

        while let Some(row) = rows.next()? {
            groups.push(row_to_group(row)?);
        }

        Ok(groups)
    }

    /// Set bootstrap complete for a group.
    pub fn set_bootstrap_complete(&self, id: &AnonGroupId, complete: bool) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE anon_groups SET bootstrap_complete = ?1, updated_at = ?2 WHERE id = ?3",
            params![
                complete as i32,
                Utc::now().to_rfc3339(),
                id.as_bytes().as_slice(),
            ],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("AnonGroup {}", id)));
        }
        Ok(())
    }

    // ========================================================================
    // Member CRUD
    // ========================================================================

    /// Add a member to a group.
    pub fn add_member(&self, member: &AnonGroupMember) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT INTO anon_group_members (id, group_id, user_id, role, mailbox_pk, joined_at, profile_key, display_name, avatar)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                member.id,
                member.group_id.as_bytes().as_slice(),
                member.user_id.as_bytes().as_slice(),
                member.role.to_i32(),
                member.mailbox_pk.as_ref().map(|pk| pk.as_slice()),
                member.joined_at.to_rfc3339(),
                member.profile_key.as_ref().map(|pk| pk.as_slice()),
                member.display_name,
                member.avatar,
            ],
        )?;
        Ok(())
    }

    /// Remove a member from a group.
    pub fn remove_member(&self, group_id: &AnonGroupId, user_id: &UserId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "DELETE FROM anon_group_members WHERE group_id = ?1 AND user_id = ?2",
            params![
                group_id.as_bytes().as_slice(),
                user_id.as_bytes().as_slice(),
            ],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!(
                "Member {} in group {}",
                user_id, group_id
            )));
        }
        Ok(())
    }

    /// Update a member's mailbox public key and profile (when JoinAck is received).
    pub fn update_member_mailbox_and_profile(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
        mailbox_pk: &[u8; 32],
        profile_key: Option<[u8; 32]>,
        display_name: Option<String>,
        avatar: Option<Vec<u8>>,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE anon_group_members 
             SET mailbox_pk = ?1, profile_key = ?4, display_name = ?5, avatar = ?6 
             WHERE group_id = ?2 AND user_id = ?3",
            params![
                mailbox_pk.as_slice(),
                group_id.as_bytes().as_slice(),
                user_id.as_bytes().as_slice(),
                profile_key.as_ref().map(|pk| pk.as_slice()),
                display_name,
                avatar,
            ],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!(
                "Member {} in group {}",
                user_id, group_id
            )));
        }
        Ok(())
    }

    /// Update a member's mailbox public key without touching the profile.
    pub fn update_member_mailbox(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
        mailbox_pk: &[u8; 32],
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE anon_group_members SET mailbox_pk = ?1 WHERE group_id = ?2 AND user_id = ?3",
            params![
                mailbox_pk.as_slice(),
                group_id.as_bytes().as_slice(),
                user_id.as_bytes().as_slice(),
            ],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!(
                "Member {} in group {}",
                user_id, group_id
            )));
        }
        Ok(())
    }

    /// Update a member's role.
    pub fn update_member_role(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
        role: AnonGroupRole,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE anon_group_members SET role = ?1 WHERE group_id = ?2 AND user_id = ?3",
            params![
                role.to_i32(),
                group_id.as_bytes().as_slice(),
                user_id.as_bytes().as_slice(),
            ],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!(
                "Member {} in group {}",
                user_id, group_id
            )));
        }
        Ok(())
    }

    /// Get all members of a group.
    pub fn get_members(&self, group_id: &AnonGroupId) -> Result<Vec<AnonGroupMember>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, group_id, user_id, role, mailbox_pk, joined_at, profile_key, display_name, avatar
             FROM anon_group_members WHERE group_id = ?1 ORDER BY joined_at",
        )?;

        let mut rows = stmt.query(params![group_id.as_bytes().as_slice()])?;
        let mut members = Vec::new();

        while let Some(row) = rows.next()? {
            members.push(row_to_member(row)?);
        }

        Ok(members)
    }

    /// Get members that have confirmed their mailbox key (for sending messages).
    pub fn get_members_with_mailbox(&self, group_id: &AnonGroupId) -> Result<Vec<AnonGroupMember>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, group_id, user_id, role, mailbox_pk, joined_at, profile_key, display_name, avatar
             FROM anon_group_members WHERE group_id = ?1 AND mailbox_pk IS NOT NULL
             ORDER BY joined_at",
        )?;

        let mut rows = stmt.query(params![group_id.as_bytes().as_slice()])?;
        let mut members = Vec::new();

        while let Some(row) = rows.next()? {
            members.push(row_to_member(row)?);
        }

        Ok(members)
    }

    /// Check if a user is a member of a group.
    pub fn is_member(&self, group_id: &AnonGroupId, user_id: &UserId) -> Result<bool, StorageError> {
        let conn = self.db.read_connection()?;
        let count: i32 = conn.query_row(
            "SELECT COUNT(*) FROM anon_group_members WHERE group_id = ?1 AND user_id = ?2",
            params![
                group_id.as_bytes().as_slice(),
                user_id.as_bytes().as_slice(),
            ],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Get a member's role in a group.
    pub fn get_member_role(&self, group_id: &AnonGroupId, user_id: &UserId) -> Result<Option<AnonGroupRole>, StorageError> {
        let conn = self.db.read_connection()?;
        let result: Result<i32, _> = conn.query_row(
            "SELECT role FROM anon_group_members WHERE group_id = ?1 AND user_id = ?2",
            params![
                group_id.as_bytes().as_slice(),
                user_id.as_bytes().as_slice(),
            ],
            |row| row.get(0),
        );

        match result {
            Ok(role) => Ok(Some(AnonGroupRole::from_i32(role))),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Get a member's display name from their profile info in the group.
    pub fn get_member_display_name(&self, group_id: &AnonGroupId, user_id: &UserId) -> Result<Option<String>, StorageError> {
        let conn = self.db.read_connection()?;
        let result: Result<Option<String>, _> = conn.query_row(
            "SELECT display_name FROM anon_group_members WHERE group_id = ?1 AND user_id = ?2",
            params![
                group_id.as_bytes().as_slice(),
                user_id.as_bytes().as_slice(),
            ],
            |row| row.get(0),
        );

        match result {
            Ok(name) => Ok(name),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    // ========================================================================
    // Mailbox CRUD
    // ========================================================================

    /// Insert a group mailbox.
    pub fn insert_mailbox(&self, mailbox: &AnonGroupMailbox) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT INTO anon_group_mailboxes (id, group_id, mailbox_public_key, mailbox_private_key, token_id, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                mailbox.id,
                mailbox.group_id.as_bytes().as_slice(),
                mailbox.public_key.as_slice(),
                mailbox.private_key.as_slice(),
                mailbox.token_id,
                mailbox.created_at.to_rfc3339(),
                mailbox.expires_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Get the mailbox for a group.
    pub fn get_mailbox(&self, group_id: &AnonGroupId) -> Result<Option<AnonGroupMailbox>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, group_id, mailbox_public_key, mailbox_private_key, token_id, created_at, expires_at
             FROM anon_group_mailboxes WHERE group_id = ?1",
        )?;

        let result = stmt.query_row(params![group_id.as_bytes().as_slice()], row_to_mailbox);

        match result {
            Ok(mailbox) => Ok(Some(mailbox)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Delete a group mailbox.
    pub fn delete_mailbox(&self, group_id: &AnonGroupId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "DELETE FROM anon_group_mailboxes WHERE group_id = ?1",
            params![group_id.as_bytes().as_slice()],
        )?;
        Ok(())
    }

    // ========================================================================
    // Pending Actions CRUD
    // ========================================================================

    /// Insert a pending action.
    pub fn insert_pending(&self, pending: &AnonGroupPending) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT OR REPLACE INTO anon_group_pending (id, group_id, user_id, action, created_at, expires_at, retry_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                pending.id,
                pending.group_id.as_bytes().as_slice(),
                pending.user_id.as_bytes().as_slice(),
                pending.action.as_str(),
                pending.created_at.to_rfc3339(),
                pending.expires_at.to_rfc3339(),
                pending.retry_count,
            ],
        )?;
        Ok(())
    }

    /// Get pending actions for a group.
    pub fn get_pending(&self, group_id: &AnonGroupId) -> Result<Vec<AnonGroupPending>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, group_id, user_id, action, created_at, expires_at, retry_count
             FROM anon_group_pending WHERE group_id = ?1 ORDER BY created_at",
        )?;

        let mut rows = stmt.query(params![group_id.as_bytes().as_slice()])?;
        let mut pending = Vec::new();

        while let Some(row) = rows.next()? {
            pending.push(row_to_pending(row)?);
        }

        Ok(pending)
    }

    /// Delete a pending action.
    pub fn delete_pending(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
        action: PendingActionType,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "DELETE FROM anon_group_pending WHERE group_id = ?1 AND user_id = ?2 AND action = ?3",
            params![
                group_id.as_bytes().as_slice(),
                user_id.as_bytes().as_slice(),
                action.as_str(),
            ],
        )?;
        Ok(())
    }

    /// Delete expired pending actions.
    pub fn delete_expired_pending(&self) -> Result<u32, StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "DELETE FROM anon_group_pending WHERE expires_at < ?1",
            params![Utc::now().to_rfc3339()],
        )?;
        Ok(rows as u32)
    }

    /// Increment retry count for a pending action.
    pub fn increment_pending_retry(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
        action: PendingActionType,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "UPDATE anon_group_pending SET retry_count = retry_count + 1
             WHERE group_id = ?1 AND user_id = ?2 AND action = ?3",
            params![
                group_id.as_bytes().as_slice(),
                user_id.as_bytes().as_slice(),
                action.as_str(),
            ],
        )?;
        Ok(())
    }

    // ========================================================================
    // Message CRUD
    // ========================================================================

    /// Insert a message into a group.
    pub fn insert_message(&self, message: &AnonGroupMessage) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT INTO anon_group_messages (id, group_id, sender_id, content_type, content_json, attachments_blob, is_deleted, sent_at, created_at, delivery_status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                message.id.as_str(),
                message.group_id.as_bytes().as_slice(),
                message.sender_id.as_bytes().as_slice(),
                message.content_type,
                message.content_json,
                message.attachments_blob,
                message.is_deleted as i32,
                message.sent_at as i64,
                message.created_at.to_rfc3339(),
                message.delivery_status,
            ],
        )?;
        Ok(())
    }

    /// Get a message by ID.
    pub fn get_message(&self, id: &AnonGroupMessageId) -> Result<Option<AnonGroupMessage>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, group_id, sender_id, content_type, content_json, attachments_blob, is_deleted, sent_at, created_at, delivery_status
             FROM anon_group_messages WHERE id = ?1",
        )?;

        let result = stmt.query_row(params![id.as_str()], row_to_message);

        match result {
            Ok(msg) => Ok(Some(msg)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Get messages for a group with pagination.
    ///
    /// Returns messages ordered by sent_at DESC (newest first).
    /// sent_at is the sender's timestamp for consistent ordering across devices.
    pub fn get_messages(
        &self,
        group_id: &AnonGroupId,
        before: Option<&DateTime<Utc>>,
        limit: i32,
    ) -> Result<Vec<AnonGroupMessage>, StorageError> {
        let conn = self.db.read_connection()?;

        // Convert before time to nanoseconds for sent_at comparison
        let before_ns = before.map(|t| t.timestamp_nanos_opt().unwrap_or(0) as u64);

        let messages = if let Some(before_nanos) = before_ns {
            let mut stmt = conn.prepare(
                "SELECT id, group_id, sender_id, content_type, content_json, attachments_blob, is_deleted, sent_at, created_at, delivery_status
                 FROM anon_group_messages
                 WHERE group_id = ?1 AND sent_at < ?2 AND is_deleted = 0
                 ORDER BY sent_at DESC, sender_id DESC
                 LIMIT ?3",
            )?;
            let mut rows = stmt.query(params![
                group_id.as_bytes().as_slice(),
                before_nanos as i64,
                limit,
            ])?;
            let mut msgs = Vec::new();
            while let Some(row) = rows.next()? {
                msgs.push(row_to_message(row)?);
            }
            msgs
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, group_id, sender_id, content_type, content_json, attachments_blob, is_deleted, sent_at, created_at, delivery_status
                 FROM anon_group_messages
                 WHERE group_id = ?1 AND is_deleted = 0
                 ORDER BY sent_at DESC, sender_id DESC
                 LIMIT ?2",
            )?;
            let mut rows = stmt.query(params![
                group_id.as_bytes().as_slice(),
                limit,
            ])?;
            let mut msgs = Vec::new();
            while let Some(row) = rows.next()? {
                msgs.push(row_to_message(row)?);
            }
            msgs
        };

        Ok(messages)
    }

    /// Soft delete a message.
    pub fn delete_message(&self, id: &AnonGroupMessageId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE anon_group_messages SET is_deleted = 1 WHERE id = ?1",
            params![id.as_str()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("AnonGroupMessage {}", id)));
        }
        Ok(())
    }

    /// Get the count of messages in a group (for unread tracking).
    pub fn get_message_count(&self, group_id: &AnonGroupId) -> Result<i64, StorageError> {
        let conn = self.db.read_connection()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM anon_group_messages WHERE group_id = ?1 AND is_deleted = 0",
            params![group_id.as_bytes().as_slice()],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Get the most recent message for a group (for preview).
    ///
    /// Uses (sent_at, sender_id) DESC for deterministic ordering.
    pub fn get_latest_message(&self, group_id: &AnonGroupId) -> Result<Option<AnonGroupMessage>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, group_id, sender_id, content_type, content_json, attachments_blob, is_deleted, sent_at, created_at, delivery_status
             FROM anon_group_messages
             WHERE group_id = ?1 AND is_deleted = 0
             ORDER BY sent_at DESC, sender_id DESC
             LIMIT 1",
        )?;

        let result = stmt.query_row(params![group_id.as_bytes().as_slice()], row_to_message);

        match result {
            Ok(msg) => Ok(Some(msg)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Get a message by sender ID and timestamp (Signal-style lookup).
    ///
    /// This is used for context menu operations, reactions, delete, pin, etc.
    /// Messages are identified by (sender_id, sent_at) tuple per Signal Protocol.
    pub fn get_message_by_sender_timestamp(
        &self,
        group_id: &AnonGroupId,
        sender_id: &UserId,
        timestamp_ns: u64,
    ) -> Result<Option<AnonGroupMessage>, StorageError> {
        let conn = self.db.read_connection()?;

        let mut stmt = conn.prepare(
            "SELECT id, group_id, sender_id, content_type, content_json, attachments_blob, is_deleted, sent_at, created_at, delivery_status
             FROM anon_group_messages
             WHERE group_id = ?1 AND sender_id = ?2 AND sent_at = ?3 AND is_deleted = 0
             LIMIT 1",
        )?;

        let result = stmt.query_row(
            params![
                group_id.as_bytes().as_slice(),
                sender_id.as_bytes().as_slice(),
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

    /// Soft delete a message by sender timestamp (for incoming delete-for-everyone).
    ///
    /// Uses (group_id, sender_id, sent_at) as the Signal-style lookup key.
    pub fn mark_message_deleted_by_timestamp(
        &self,
        group_id: &AnonGroupId,
        sender_id: &UserId,
        sent_timestamp: u64,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;

        let rows = conn.execute(
            "UPDATE anon_group_messages SET is_deleted = 1
             WHERE group_id = ?1 AND sender_id = ?2 AND sent_at = ?3 AND is_deleted = 0",
            params![
                group_id.as_bytes().as_slice(),
                sender_id.as_bytes().as_slice(),
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
        group_id: &AnonGroupId,
        sent_timestamp: u64,
        new_text: &str,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;

        // Build new content_json with updated text
        let content_json = serde_json::json!({ "text": new_text }).to_string();

        let rows = conn.execute(
            "UPDATE anon_group_messages SET content_json = ?1
             WHERE group_id = ?2 AND sent_at = ?3",
            params![
                content_json,
                group_id.as_bytes().as_slice(),
                sent_timestamp as i64,
            ],
        )?;

        Ok(rows > 0)
    }

    // ========================================================================
    // Group Status / Blocking
    // ========================================================================

    /// Update group status (active/muted/blocked).
    pub fn update_status(
        &self,
        id: &AnonGroupId,
        status: crate::domain::anon_group::AnonGroupStatus,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now();
        let rows = conn.execute(
            "UPDATE anon_groups SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![status.to_i32(), now.to_rfc3339(), id.as_bytes().as_slice()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("AnonGroup {}", id)));
        }
        Ok(())
    }

    /// List only active (non-blocked) groups.
    pub fn list_active_groups(&self) -> Result<Vec<AnonGroup>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, my_role, bootstrap_complete, created_at, updated_at, status
             FROM anon_groups WHERE status != 2 ORDER BY updated_at DESC",
        )?;

        let mut rows = stmt.query([])?;
        let mut groups = Vec::new();

        while let Some(row) = rows.next()? {
            groups.push(row_to_group(row)?);
        }

        Ok(groups)
    }

    // ========================================================================
    // Per-User Blocking within Groups
    // ========================================================================

    /// Block a user within a group.
    pub fn block_user(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
        reason: Option<&str>,
    ) -> Result<(), StorageError> {
        use crate::domain::anon_group::AnonGroupBlockedUser;

        let blocked = AnonGroupBlockedUser::new(
            *group_id,
            *user_id,
            reason.map(String::from),
        );

        let conn = self.db.connection()?;
        conn.execute(
            "INSERT OR REPLACE INTO anon_group_blocked_users (id, group_id, user_id, blocked_at, reason)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                blocked.id,
                group_id.as_bytes().as_slice(),
                user_id.as_bytes().as_slice(),
                blocked.blocked_at.to_rfc3339(),
                blocked.reason,
            ],
        )?;
        Ok(())
    }

    /// Unblock a user within a group.
    pub fn unblock_user(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "DELETE FROM anon_group_blocked_users WHERE group_id = ?1 AND user_id = ?2",
            params![
                group_id.as_bytes().as_slice(),
                user_id.as_bytes().as_slice(),
            ],
        )?;
        Ok(())
    }

    /// Check if a user is blocked within a group.
    pub fn is_user_blocked(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
    ) -> Result<bool, StorageError> {
        let conn = self.db.read_connection()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM anon_group_blocked_users WHERE group_id = ?1 AND user_id = ?2",
            params![
                group_id.as_bytes().as_slice(),
                user_id.as_bytes().as_slice(),
            ],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Get all blocked users in a group.
    pub fn get_blocked_users(
        &self,
        group_id: &AnonGroupId,
    ) -> Result<Vec<UserId>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT user_id FROM anon_group_blocked_users WHERE group_id = ?1",
        )?;

        let mut rows = stmt.query(params![group_id.as_bytes().as_slice()])?;
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

    /// Get or create rate limit entry for a group.
    pub fn get_rate_limit(
        &self,
        group_id: &AnonGroupId,
    ) -> Result<Option<crate::domain::anon_group::AnonRateLimitEntry>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, entity_type, entity_id, window_start, message_count, last_message_at
             FROM anon_rate_limits WHERE entity_type = 'group' AND entity_id = ?1",
        )?;

        let result = stmt.query_row(params![group_id.as_bytes().as_slice()], |row| {
            Ok(crate::domain::anon_group::AnonRateLimitEntry {
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
        group_id: &AnonGroupId,
        window_duration: chrono::Duration,
    ) -> Result<u32, StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now();

        // Try to get existing entry
        if let Some(entry) = self.get_rate_limit(group_id)? {
            if entry.is_window_expired(window_duration) {
                // Reset window
                conn.execute(
                    "UPDATE anon_rate_limits SET window_start = ?1, message_count = 1, last_message_at = ?1
                     WHERE entity_type = 'group' AND entity_id = ?2",
                    params![now.to_rfc3339(), group_id.as_bytes().as_slice()],
                )?;
                Ok(1)
            } else {
                // Increment counter
                let new_count = entry.message_count + 1;
                conn.execute(
                    "UPDATE anon_rate_limits SET message_count = ?1, last_message_at = ?2
                     WHERE entity_type = 'group' AND entity_id = ?3",
                    params![
                        new_count as i32,
                        now.to_rfc3339(),
                        group_id.as_bytes().as_slice(),
                    ],
                )?;
                Ok(new_count)
            }
        } else {
            // Create new entry
            let entry = crate::domain::anon_group::AnonRateLimitEntry::new(
                "group",
                group_id.as_bytes().to_vec(),
            );
            conn.execute(
                "INSERT INTO anon_rate_limits (id, entity_type, entity_id, window_start, message_count, last_message_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    entry.id,
                    entry.entity_type,
                    entry.entity_id,
                    entry.window_start.to_rfc3339(),
                    entry.message_count as i32,
                    entry.last_message_at.to_rfc3339(),
                ],
            )?;
            Ok(1)
        }
    }
}

// ============================================================================
// Helper functions
// ============================================================================

fn parse_datetime(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

fn row_to_group(row: &rusqlite::Row<'_>) -> rusqlite::Result<AnonGroup> {
    use crate::domain::anon_group::AnonGroupStatus;

    let id_bytes: Vec<u8> = row.get(0)?;
    let id = AnonGroupId::try_from_slice(&id_bytes)
        .ok_or_else(|| rusqlite::Error::InvalidParameterName("Invalid group ID".into()))?;

    // Status column may not exist in older schemas, default to Active
    let status: i32 = row.get(6).unwrap_or(0);

    Ok(AnonGroup {
        id,
        name: row.get(1)?,
        my_role: AnonGroupRole::from_i32(row.get(2)?),
        status: AnonGroupStatus::from_i32(status),
        bootstrap_complete: row.get::<_, i32>(3)? != 0,
        created_at: parse_datetime(&row.get::<_, String>(4)?),
        updated_at: parse_datetime(&row.get::<_, String>(5)?),
    })
}

fn row_to_member(row: &rusqlite::Row<'_>) -> rusqlite::Result<AnonGroupMember> {
    let group_id_bytes: Vec<u8> = row.get(1)?;
    let group_id = AnonGroupId::try_from_slice(&group_id_bytes)
        .ok_or_else(|| rusqlite::Error::InvalidParameterName("Invalid group ID".into()))?;

    let user_id_bytes: Vec<u8> = row.get(2)?;
    let user_id = UserId::try_from_slice(&user_id_bytes)
        .ok_or_else(|| rusqlite::Error::InvalidParameterName("Invalid user ID".into()))?;

    let mailbox_pk: Option<Vec<u8>> = row.get(4)?;
    let mailbox_pk = mailbox_pk.and_then(|bytes| {
        if bytes.len() == 32 {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes);
            Some(arr)
        } else {
            None
        }
    });

    let profile_key: Option<Vec<u8>> = row.get(6)?;
    let profile_key = profile_key.and_then(|bytes| {
        if bytes.len() == 32 {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes);
            Some(arr)
        } else {
            None
        }
    });

    Ok(AnonGroupMember {
        id: row.get(0)?,
        group_id,
        user_id,
        role: AnonGroupRole::from_i32(row.get(3)?),
        mailbox_pk,
        profile_key,
        display_name: row.get(7)?,
        avatar: row.get(8)?,
        joined_at: parse_datetime(&row.get::<_, String>(5)?),
    })
}

fn row_to_mailbox(row: &rusqlite::Row<'_>) -> rusqlite::Result<AnonGroupMailbox> {
    let group_id_bytes: Vec<u8> = row.get(1)?;
    let group_id = AnonGroupId::try_from_slice(&group_id_bytes)
        .ok_or_else(|| rusqlite::Error::InvalidParameterName("Invalid group ID".into()))?;

    let pk_bytes: Vec<u8> = row.get(2)?;
    let sk_bytes: Vec<u8> = row.get(3)?;

    if pk_bytes.len() != 32 {
        return Err(rusqlite::Error::InvalidParameterName("Invalid public key".into()));
    }
    if sk_bytes.len() != 64 {
        return Err(rusqlite::Error::InvalidParameterName("Invalid private key".into()));
    }

    let mut public_key = [0u8; 32];
    let mut private_key = [0u8; 64];
    public_key.copy_from_slice(&pk_bytes);
    private_key.copy_from_slice(&sk_bytes);

    Ok(AnonGroupMailbox {
        id: row.get(0)?,
        group_id,
        public_key,
        private_key,
        token_id: row.get(4)?,
        created_at: parse_datetime(&row.get::<_, String>(5)?),
        expires_at: parse_datetime(&row.get::<_, String>(6)?),
    })
}

fn row_to_pending(row: &rusqlite::Row<'_>) -> rusqlite::Result<AnonGroupPending> {
    let group_id_bytes: Vec<u8> = row.get(1)?;
    let group_id = AnonGroupId::try_from_slice(&group_id_bytes)
        .ok_or_else(|| rusqlite::Error::InvalidParameterName("Invalid group ID".into()))?;

    let user_id_bytes: Vec<u8> = row.get(2)?;
    let user_id = UserId::try_from_slice(&user_id_bytes)
        .ok_or_else(|| rusqlite::Error::InvalidParameterName("Invalid user ID".into()))?;

    let action_str: String = row.get(3)?;
    let action = PendingActionType::from_str(&action_str)
        .ok_or_else(|| rusqlite::Error::InvalidParameterName("Invalid action type".into()))?;

    Ok(AnonGroupPending {
        id: row.get(0)?,
        group_id,
        user_id,
        action,
        created_at: parse_datetime(&row.get::<_, String>(4)?),
        expires_at: parse_datetime(&row.get::<_, String>(5)?),
        retry_count: row.get(6)?,
    })
}

fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<AnonGroupMessage> {
    let group_id_bytes: Vec<u8> = row.get(1)?;
    let group_id = AnonGroupId::try_from_slice(&group_id_bytes)
        .ok_or_else(|| rusqlite::Error::InvalidParameterName("Invalid group ID".into()))?;

    let sender_id_bytes: Vec<u8> = row.get(2)?;
    let sender_id = UserId::try_from_slice(&sender_id_bytes)
        .ok_or_else(|| rusqlite::Error::InvalidParameterName("Invalid sender ID".into()))?;

    let sent_at: i64 = row.get(7)?;

    Ok(AnonGroupMessage {
        id: AnonGroupMessageId::from_string(row.get(0)?),
        group_id,
        sender_id,
        content_type: row.get(3)?,
        content_json: row.get(4)?,
        attachments_blob: row.get(5)?,
        is_deleted: row.get::<_, i32>(6)? != 0,
        reaction_counts: Vec::new(),
        current_user_reactions: Vec::new(),
        delivery_status: row.get(9)?,
        sent_at: sent_at as u64,
        created_at: parse_datetime(&row.get::<_, String>(8)?),
    })
}
