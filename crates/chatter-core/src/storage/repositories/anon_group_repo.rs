//! Anonymous group management repository.
//!
//! Data access for NDSS'21 style anonymous groups.
//! Message operations are handled by the unified MessageRepository.

use crate::domain::anon_group::{
    AnonGroup, AnonGroupId, AnonGroupMailbox, AnonGroupMember, AnonGroupPending, AnonGroupRole,
    PendingActionType,
};
use crate::domain::UserId;
use crate::error::StorageError;
use crate::storage::Database;
use chrono::Utc;
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
            "INSERT OR REPLACE INTO anon_groups (id, name, owner_id, my_role, bootstrap_complete, created_at, updated_at, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                group.id.to_string(),
                group.name,
                group.owner_id.to_string(),
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
            "SELECT id, name, my_role, bootstrap_complete, created_at, updated_at, status, owner_id
             FROM anon_groups WHERE id = ?1",
        )?;

        let result = stmt.query_row(params![id.to_string()], row_to_group);

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
            "UPDATE anon_groups SET name = ?1, my_role = ?2, bootstrap_complete = ?3, updated_at = ?4, status = ?5, owner_id = ?6
             WHERE id = ?7",
            params![
                group.name,
                group.my_role.to_i32(),
                group.bootstrap_complete as i32,
                group.updated_at.to_rfc3339(),
                group.status.to_i32(),
                group.owner_id.to_string(),
                group.id.to_string(),
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
            params![name, now.to_rfc3339(), id.to_string()],
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
            params![id.to_string()],
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
            "SELECT id, name, my_role, bootstrap_complete, created_at, updated_at, status, owner_id
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
                id.to_string(),
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

    /// Add a member to a group. Uses upsert so accept_invite is idempotent on retry.
    pub fn add_member(&self, member: &AnonGroupMember) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT INTO anon_group_members (id, group_id, user_id, role, mailbox_pk, joined_at, profile_key, display_name, avatar)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(group_id, user_id) DO UPDATE SET
                role = excluded.role,
                mailbox_pk = COALESCE(excluded.mailbox_pk, mailbox_pk),
                profile_key = COALESCE(excluded.profile_key, profile_key),
                display_name = COALESCE(excluded.display_name, display_name),
                avatar = COALESCE(excluded.avatar, avatar)",
            params![
                member.id,
                member.group_id.to_string(),
                member.user_id.to_string(),
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
                group_id.to_string(),
                user_id.to_string(),
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
    ///
    /// Uses COALESCE to preserve existing profile data when new values are NULL.
    /// This prevents member list updates from clearing display names that were
    /// correctly stored during invite acceptance.
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
             SET mailbox_pk = ?1,
                 profile_key = COALESCE(?4, profile_key),
                 display_name = COALESCE(?5, display_name),
                 avatar = COALESCE(?6, avatar)
             WHERE group_id = ?2 AND user_id = ?3",
            params![
                mailbox_pk.as_slice(),
                group_id.to_string(),
                user_id.to_string(),
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

    /// Update a member's profile (display name, avatar) without touching mailbox key.
    ///
    /// Uses COALESCE to preserve existing profile data when new values are NULL.
    pub fn update_member_profile(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
        profile_key: Option<[u8; 32]>,
        display_name: Option<String>,
        avatar: Option<Vec<u8>>,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE anon_group_members
             SET profile_key = COALESCE(?1, profile_key),
                 display_name = COALESCE(?2, display_name),
                 avatar = COALESCE(?3, avatar)
             WHERE group_id = ?4 AND user_id = ?5",
            params![
                profile_key.as_ref().map(|pk| pk.as_slice()),
                display_name,
                avatar,
                group_id.to_string(),
                user_id.to_string(),
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
                group_id.to_string(),
                user_id.to_string(),
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
                group_id.to_string(),
                user_id.to_string(),
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

        let mut rows = stmt.query(params![group_id.to_string()])?;
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

        let mut rows = stmt.query(params![group_id.to_string()])?;
        let mut members = Vec::new();

        while let Some(row) = rows.next()? {
            members.push(row_to_member(row)?);
        }

        Ok(members)
    }

    /// Get members that do NOT yet have a confirmed mailbox key.
    ///
    /// These are invitees whose JoinAck has not yet been received. Messages
    /// destined for them must be queued via `enqueue_pending_message` and
    /// drained once the JoinAck arrives.
    pub fn get_members_without_mailbox(&self, group_id: &AnonGroupId) -> Result<Vec<AnonGroupMember>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, group_id, user_id, role, mailbox_pk, joined_at, profile_key, display_name, avatar
             FROM anon_group_members WHERE group_id = ?1 AND mailbox_pk IS NULL
             ORDER BY joined_at",
        )?;

        let mut rows = stmt.query(params![group_id.to_string()])?;
        let mut members = Vec::new();

        while let Some(row) = rows.next()? {
            members.push(row_to_member(row)?);
        }

        Ok(members)
    }

    /// Get a member of a group.
    pub fn get_member(&self, group_id: &AnonGroupId, user_id: &UserId) -> Result<Option<AnonGroupMember>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, group_id, user_id, role, mailbox_pk, joined_at, profile_key, display_name, avatar
             FROM anon_group_members WHERE group_id = ?1 AND user_id = ?2",
        )?;

        let result = stmt.query_row(
            params![
                group_id.to_string(),
                user_id.to_string(),
            ],
            row_to_member,
        );

        match result {
            Ok(member) => Ok(Some(member)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Check if a user is a member of a group.
    pub fn is_member(&self, group_id: &AnonGroupId, user_id: &UserId) -> Result<bool, StorageError> {
        let conn = self.db.read_connection()?;
        let count: i32 = conn.query_row(
            "SELECT COUNT(*) FROM anon_group_members WHERE group_id = ?1 AND user_id = ?2",
            params![
                group_id.to_string(),
                user_id.to_string(),
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
                group_id.to_string(),
                user_id.to_string(),
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
    /// Return any non-empty display name recorded for `user_id` across any group.
    ///
    /// Used by `UserService::get_display_name` to avoid raw-UUID fallback for
    /// anon group members who aren't in the main user directory.
    pub fn find_display_name_for_user(&self, user_id: &UserId) -> Result<Option<String>, StorageError> {
        let conn = self.db.read_connection()?;
        let result: Result<Option<String>, _> = conn.query_row(
            "SELECT display_name FROM anon_group_members \
             WHERE user_id = ?1 AND display_name IS NOT NULL AND display_name != '' LIMIT 1",
            params![user_id.to_string()],
            |row| row.get(0),
        );
        match result {
            Ok(name) => Ok(name),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    pub fn get_member_display_name(&self, group_id: &AnonGroupId, user_id: &UserId) -> Result<Option<String>, StorageError> {
        let conn = self.db.read_connection()?;
        let result: Result<Option<String>, _> = conn.query_row(
            "SELECT display_name FROM anon_group_members WHERE group_id = ?1 AND user_id = ?2",
            params![
                group_id.to_string(),
                user_id.to_string(),
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
            "INSERT OR REPLACE INTO anon_group_mailboxes (id, group_id, mailbox_public_key, mailbox_private_key, token_id, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                mailbox.id,
                mailbox.group_id.to_string(),
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

        let result = stmt.query_row(params![group_id.to_string()], row_to_mailbox);

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
            params![group_id.to_string()],
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
                pending.group_id.to_string(),
                pending.user_id.to_string(),
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

        let mut rows = stmt.query(params![group_id.to_string()])?;
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
                group_id.to_string(),
                user_id.to_string(),
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
                group_id.to_string(),
                user_id.to_string(),
                action.as_str(),
            ],
        )?;
        Ok(())
    }

    // Message CRUD methods have been removed.
    // All message operations now use the unified MessageRepository.

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
            params![status.to_i32(), now.to_rfc3339(), id.to_string()],
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
            "SELECT id, name, my_role, bootstrap_complete, created_at, updated_at, status, owner_id
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
                group_id.to_string(),
                user_id.to_string(),
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
                group_id.to_string(),
                user_id.to_string(),
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
                group_id.to_string(),
                user_id.to_string(),
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

        let mut rows = stmt.query(params![group_id.to_string()])?;
        let mut users = Vec::new();

        while let Some(row) = rows.next()? {
            let user_id_str: String = row.get(0)?;
            let user_id = user_id_str.parse().map_err(|_| rusqlite::Error::InvalidQuery)?;
            users.push(user_id);
        }

        Ok(users)
    }

    // ========================================================================
    // Rate Limiting
    // ========================================================================

    /// Get rate limit entry for a group.
    pub fn get_rate_limit(
        &self,
        group_id: &AnonGroupId,
    ) -> Result<Option<crate::domain::AnonRateLimitEntry>, StorageError> {
        super::repo_utils::get_rate_limit(&self.db, "group", group_id.as_bytes().as_slice())
    }

    /// Increment rate limit counter, resetting window if expired.
    pub fn increment_rate_limit(
        &self,
        group_id: &AnonGroupId,
        window_duration: chrono::Duration,
    ) -> Result<u32, StorageError> {
        super::repo_utils::increment_rate_limit(&self.db, "group", group_id.as_bytes().as_slice(), window_duration)
    }

    // ========================================================================
    // Pending Outbound Messages (awaiting recipient JoinAck)
    // ========================================================================

    /// Enqueue an outbound message for a member that does not yet have a
    /// confirmed mailbox key.
    ///
    /// `content_blob` is the encoded `Content` proto (same bytes `send_message`
    /// would otherwise pass to `send_to_anon_mailbox`). The blob is re-encrypted
    /// per-recipient when drained (Signal Protocol is pairwise).
    pub fn enqueue_pending_message(
        &self,
        group_id: &AnonGroupId,
        recipient_user_id: &UserId,
        content_blob: &[u8],
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO pending_anon_group_messages
               (group_id, recipient_user_id, encrypted_content_blob, created_at, attempts)
             VALUES (?1, ?2, ?3, ?4, 0)",
            params![
                group_id.to_string(),
                recipient_user_id.to_string(),
                content_blob,
                now,
            ],
        )?;
        Ok(())
    }

    /// Get all pending messages queued for a specific (group, recipient).
    ///
    /// Returns a vector of (row_id, content_blob) pairs ordered by insertion
    /// order so the drain preserves send order.
    pub fn get_pending_messages_for_member(
        &self,
        group_id: &AnonGroupId,
        recipient_user_id: &UserId,
    ) -> Result<Vec<(i64, Vec<u8>)>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, encrypted_content_blob FROM pending_anon_group_messages
             WHERE group_id = ?1 AND recipient_user_id = ?2
             ORDER BY id ASC",
        )?;

        let mut rows = stmt.query(params![
            group_id.to_string(),
            recipient_user_id.to_string(),
        ])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            out.push((id, blob));
        }
        Ok(out)
    }

    /// Delete a single pending message row after successful delivery.
    pub fn delete_pending_message(&self, row_id: i64) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "DELETE FROM pending_anon_group_messages WHERE id = ?1",
            params![row_id],
        )?;
        Ok(())
    }

    /// Increment the attempts counter for a pending message row (kept for
    /// diagnostics / future backoff).
    pub fn increment_pending_message_attempts(&self, row_id: i64) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "UPDATE pending_anon_group_messages SET attempts = attempts + 1 WHERE id = ?1",
            params![row_id],
        )?;
        Ok(())
    }
}

// ============================================================================
// Helper functions
// ============================================================================

use super::repo_utils::{parse_datetime, vec_to_array_32, require_array_32, require_array_64};

fn row_to_group(row: &rusqlite::Row<'_>) -> rusqlite::Result<AnonGroup> {
    use crate::domain::anon_group::AnonGroupStatus;

    let id_str: String = row.get(0)?;
    let id = id_str.parse().map_err(|_| rusqlite::Error::InvalidQuery)?;

    // Status column may not exist in older schemas, default to Active
    let status: i32 = row.get(6).unwrap_or(0);

    let owner_id_str: String = row.get(7)?;
    let owner_id = owner_id_str.parse().map_err(|_| rusqlite::Error::InvalidQuery)?;

    Ok(AnonGroup {
        id,
        name: row.get(1)?,
        owner_id,
        my_role: AnonGroupRole::from_i32(row.get(2)?),
        status: AnonGroupStatus::from_i32(status),
        bootstrap_complete: row.get::<_, i32>(3)? != 0,
        created_at: parse_datetime(&row.get::<_, String>(4)?),
        updated_at: parse_datetime(&row.get::<_, String>(5)?),
    })
}

fn row_to_member(row: &rusqlite::Row<'_>) -> rusqlite::Result<AnonGroupMember> {
    let group_id_str: String = row.get(1)?;
    let group_id = group_id_str.parse().map_err(|_| rusqlite::Error::InvalidQuery)?;

    let user_id_str: String = row.get(2)?;
    let user_id = user_id_str.parse().map_err(|_| rusqlite::Error::InvalidQuery)?;

    let mailbox_pk = vec_to_array_32(row.get(4)?);
    let profile_key = vec_to_array_32(row.get(6)?);

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
    let group_id_str: String = row.get(1)?;
    let group_id = group_id_str.parse().map_err(|_| rusqlite::Error::InvalidQuery)?;

    let pk_bytes: Vec<u8> = row.get(2)?;
    let sk_bytes: Vec<u8> = row.get(3)?;

    let public_key = require_array_32(&pk_bytes)
        .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid public key".into()))?;
    let private_key = require_array_64(&sk_bytes)
        .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid private key".into()))?;

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
    let group_id_str: String = row.get(1)?;
    let group_id = group_id_str.parse().map_err(|_| rusqlite::Error::InvalidQuery)?;

    let user_id_str: String = row.get(2)?;
    let user_id = user_id_str.parse().map_err(|_| rusqlite::Error::InvalidQuery)?;

    let action_str: String = row.get(3)?;
    let action = PendingActionType::parse(&action_str)
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
