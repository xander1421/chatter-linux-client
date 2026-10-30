use super::*;

impl GroupMailboxManager {
    /// Update last_polled_at timestamp for a mailbox.
    ///
    /// Tries both `group_mailboxes` (server-managed) and `anon_group_mailboxes` (anonymous groups).
    pub fn update_poll_time(&self, mailbox_id: &str) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let now = Utc::now().to_rfc3339();

        // Try server-managed group mailboxes first
        let rows = conn
            .execute(
                "UPDATE group_mailboxes SET last_polled_at = ?1 WHERE id = ?2",
                rusqlite::params![now, mailbox_id],
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        if rows > 0 {
            return Ok(());
        }

        // Try anonymous group mailboxes
        let rows = conn
            .execute(
                "UPDATE anon_group_mailboxes SET last_polled_at = ?1 WHERE id = ?2",
                rusqlite::params![now, mailbox_id],
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        if rows > 0 {
            return Ok(());
        }

        Err(MailboxError::TokenNotFound(mailbox_id.to_string()))
    }

    /// Get mailbox keys by mailbox ID.
    ///
    /// Looks up keys from both `group_mailboxes` and `anon_group_mailboxes` tables.
    /// Returns (public_key, private_key, secret) if found; `secret` is true for
    /// an anonymous (secret-chat) group mailbox, whose traffic must go over Tor.
    pub fn get_mailbox_keys_by_id(
        &self,
        mailbox_id: &str,
    ) -> Result<([u8; 32], [u8; 64], bool), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        // Try server-managed group mailboxes first
        let result: Result<(Vec<u8>, Vec<u8>), _> = conn.query_row(
            "SELECT mailbox_public_key, mailbox_private_key FROM group_mailboxes WHERE id = ?1",
            rusqlite::params![mailbox_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        );

        if let Ok((public_key, private_key)) = result {
            let public_key_arr: [u8; 32] = public_key
                .try_into()
                .map_err(|_| MailboxError::Storage("Invalid public key length".into()))?;
            let private_key_arr: [u8; 64] = private_key
                .try_into()
                .map_err(|_| MailboxError::Storage("Invalid private key length".into()))?;
            return Ok((public_key_arr, private_key_arr, false));
        }

        // Try anonymous group mailboxes
        let result: Result<(Vec<u8>, Vec<u8>), _> = conn.query_row(
            "SELECT mailbox_public_key, mailbox_private_key FROM anon_group_mailboxes WHERE id = ?1",
            rusqlite::params![mailbox_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        );

        match result {
            Ok((public_key, private_key)) => {
                let public_key_arr: [u8; 32] = public_key
                    .try_into()
                    .map_err(|_| MailboxError::Storage("Invalid public key length".into()))?;
                let private_key_arr: [u8; 64] = private_key
                    .try_into()
                    .map_err(|_| MailboxError::Storage("Invalid private key length".into()))?;
                Ok((public_key_arr, private_key_arr, true))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                Err(MailboxError::TokenNotFound(mailbox_id.to_string()))
            }
            Err(e) => Err(MailboxError::Storage(e.to_string())),
        }
    }

    /// Get all group mailboxes for batch polling.
    ///
    /// Returns (group_id, public_key, private_key, mailbox_id, secret) for all
    /// active groups. Queries both server-managed groups (group_mailboxes) and
    /// anonymous groups (anon_group_mailboxes); `secret` marks the latter,
    /// whose traffic must go over Tor.
    #[allow(clippy::type_complexity)]
    pub fn get_all_group_mailboxes(
        &self,
    ) -> Result<Vec<(ChannelId, [u8; 32], [u8; 64], String, bool)>, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let now = Utc::now().to_rfc3339();

        // Query from both tables using UNION
        // Both group_mailboxes.group_id and anon_group_mailboxes.group_id are TEXT UUIDs.
        let mut stmt = conn
            .prepare(
                "SELECT group_id, mailbox_public_key, mailbox_private_key, id, expires_at, 'server' as source
                 FROM group_mailboxes WHERE expires_at > ?1
                 UNION ALL
                 SELECT group_id, mailbox_public_key, mailbox_private_key, id, expires_at, 'anon' as source
                 FROM anon_group_mailboxes WHERE expires_at > ?1
                   AND EXISTS (SELECT 1 FROM anon_groups g WHERE g.id = anon_group_mailboxes.group_id AND g.status != 3)",
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let mut rows = stmt
            .query(rusqlite::params![now])
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let mut results = Vec::new();

        while let Some(row) = rows
            .next()
            .map_err(|e| MailboxError::Storage(e.to_string()))?
        {
            let group_id_str: String = row
                .get(0)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;
            let public_key: Vec<u8> = row
                .get(1)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;
            let private_key: Vec<u8> = row
                .get(2)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;
            let mailbox_id: String = row
                .get(3)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;
            let source: String = row
                .get(5)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;

            tracing::debug!(
                source = %source,
                "[GROUP_MAILBOX] Found active mailbox"
            );

            // Parse group_id - server groups are UUID strings, anon groups are hex-encoded bytes
            let group_id: ChannelId = if group_id_str.contains('-') {
                // Standard UUID format: "8c3cbbe0-3342-4bd8-bbd5-ce669d986cbe"
                group_id_str.parse().map_err(|e: uuid::Error| {
                    MailboxError::Storage(format!("Invalid group_id: {}", e))
                })?
            } else {
                // Hex-encoded 16-byte UUID from anon_group_mailboxes: "8C3CBBE033424BD8BBD5CE669D986CBE"
                let bytes = hex::decode(&group_id_str)
                    .map_err(|e| MailboxError::Storage(format!("Invalid hex group_id: {}", e)))?;
                let bytes_arr: [u8; 16] = bytes
                    .try_into()
                    .map_err(|_| MailboxError::Storage("Invalid group_id length".into()))?;
                uuid::Uuid::from_bytes(bytes_arr).into()
            };

            let public_key_arr: [u8; 32] = public_key
                .try_into()
                .map_err(|_| MailboxError::Storage("Invalid public key length".into()))?;
            let private_key_arr: [u8; 64] = private_key
                .try_into()
                .map_err(|_| MailboxError::Storage("Invalid private key length".into()))?;

            results.push((
                group_id,
                public_key_arr,
                private_key_arr,
                mailbox_id,
                source == "anon",
            ));
        }

        if results.is_empty() {
            tracing::trace!("[GROUP_MAILBOX] No active group mailboxes for polling");
        } else {
            tracing::debug!(
                count = results.len(),
                "[GROUP_MAILBOX] Returning {} active group mailboxes for polling",
                results.len()
            );
        }

        Ok(results)
    }

    /// Get groups with mailboxes expiring soon.
    ///
    /// Returns group IDs of groups needing mailbox refresh.
    pub fn get_expiring_group_mailboxes(
        &self,
        within_hours: i64,
    ) -> Result<Vec<ChannelId>, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let threshold = (Utc::now() + Duration::hours(within_hours)).to_rfc3339();
        let now = Utc::now().to_rfc3339();

        // Re-rotation guard: a group is a candidate only if its NEWEST row is
        // expiring within the window. Once rotation has minted a fresh row
        // (expires_at > threshold) the group is excluded, so a lingering old
        // row in the overlap window can't re-trigger rotation every cycle and
        // drain the token budget (gate G5).
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT group_id FROM group_mailboxes
                 WHERE expires_at > ?1 AND expires_at <= ?2
                   AND group_id NOT IN (
                       SELECT group_id FROM group_mailboxes WHERE expires_at > ?2
                   )",
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let mut rows = stmt
            .query(rusqlite::params![now, threshold])
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let mut results = Vec::new();

        while let Some(row) = rows
            .next()
            .map_err(|e| MailboxError::Storage(e.to_string()))?
        {
            let group_id_str: String = row
                .get(0)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;

            let group_id: ChannelId = group_id_str.parse().map_err(|e: uuid::Error| {
                MailboxError::Storage(format!("Invalid group_id: {}", e))
            })?;

            results.push(group_id);
        }

        Ok(results)
    }

    /// Delete expired mailbox state.
    ///
    /// Returns the number of expired mailboxes cleaned up.
    pub fn cleanup_expired(&self) -> Result<u32, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let now = Utc::now().to_rfc3339();

        let rows = conn
            .execute(
                "DELETE FROM group_mailboxes WHERE expires_at <= ?1",
                rusqlite::params![now],
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        Ok(rows as u32)
    }

    /// List all active groups (groups where we have a non-expired mailbox).
    ///
    /// Returns list of group IDs.
    pub fn list_active_groups(&self) -> Result<Vec<ChannelId>, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let now = Utc::now().to_rfc3339();

        let mut stmt = conn
            .prepare("SELECT DISTINCT group_id FROM group_mailboxes WHERE expires_at > ?1")
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let mut rows = stmt
            .query(rusqlite::params![now])
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let mut results = Vec::new();

        while let Some(row) = rows
            .next()
            .map_err(|e| MailboxError::Storage(e.to_string()))?
        {
            let group_id_str: String = row
                .get(0)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;

            let group_id: ChannelId = group_id_str.parse().map_err(|e: uuid::Error| {
                MailboxError::Storage(format!("Invalid group_id: {}", e))
            })?;

            results.push(group_id);
        }

        Ok(results)
    }

    /// Delete all state for a group.
    ///
    /// Useful when user leaves a group.
    pub fn delete_group_state(&self, group_id: &ChannelId) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let group_id_str = group_id.to_string();

        // Delete from both tables
        conn.execute(
            "DELETE FROM group_mailboxes WHERE group_id = ?1",
            rusqlite::params![group_id_str],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;

        conn.execute(
            "DELETE FROM group_member_mailboxes WHERE group_id = ?1",
            rusqlite::params![group_id_str],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;

        // Also delete pending channel messages
        conn.execute(
            "DELETE FROM pending_channel_messages WHERE channel_id = ?1",
            rusqlite::params![group_id_str],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;

        Ok(())
    }
}
