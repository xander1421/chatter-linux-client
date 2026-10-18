use super::*;

impl AnonGroupRepository {
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
    pub fn get_mailbox(
        &self,
        group_id: &AnonGroupId,
    ) -> Result<Option<AnonGroupMailbox>, StorageError> {
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
}
