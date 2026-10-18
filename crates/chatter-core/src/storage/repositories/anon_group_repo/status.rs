use super::*;

impl AnonGroupRepository {
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
             FROM anon_groups WHERE status NOT IN (2, 3) ORDER BY updated_at DESC",
        )?;

        let mut rows = stmt.query([])?;
        super::super::repo_utils::collect_decoded_rows(&mut rows, "anonymous_group", |row| {
            row_to_group(row).map_err(StorageError::Database)
        })
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

        let blocked = AnonGroupBlockedUser::new(*group_id, *user_id, reason.map(String::from));

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
            params![group_id.to_string(), user_id.to_string(),],
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
            params![group_id.to_string(), user_id.to_string(),],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Get all blocked users in a group.
    pub fn get_blocked_users(&self, group_id: &AnonGroupId) -> Result<Vec<UserId>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt =
            conn.prepare("SELECT user_id FROM anon_group_blocked_users WHERE group_id = ?1")?;

        let mut rows = stmt.query(params![group_id.to_string()])?;
        let mut users = Vec::new();

        while let Some(row) = rows.next()? {
            let user_id_str: String = row.get(0)?;
            let user_id = user_id_str
                .parse()
                .map_err(|_| rusqlite::Error::InvalidQuery)?;
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
        super::super::repo_utils::get_rate_limit(&self.db, "group", group_id.as_bytes().as_slice())
    }

    /// Increment rate limit counter, resetting window if expired.
    pub fn increment_rate_limit(
        &self,
        group_id: &AnonGroupId,
        window_duration: chrono::Duration,
    ) -> Result<u32, StorageError> {
        super::super::repo_utils::increment_rate_limit(
            &self.db,
            "group",
            group_id.as_bytes().as_slice(),
            window_duration,
        )
    }
}
