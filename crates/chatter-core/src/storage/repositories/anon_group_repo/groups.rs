use super::*;

impl AnonGroupRepository {
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

    /// Record a new owner, and our own role after the hand-off.
    pub fn set_owner(
        &self,
        id: &AnonGroupId,
        owner: &UserId,
        my_role: crate::domain::anon_group::AnonGroupRole,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE anon_groups SET owner_id = ?1, my_role = ?2, updated_at = ?3 WHERE id = ?4",
            params![
                owner.to_string(),
                my_role.to_i32(),
                chrono::Utc::now().to_rfc3339(),
                id.to_string()
            ],
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
        super::super::repo_utils::collect_decoded_rows(&mut rows, "anonymous_group", |row| {
            row_to_group(row).map_err(StorageError::Database)
        })
    }
}
