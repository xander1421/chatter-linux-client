use super::*;

impl GroupMailboxManager {
    // =========================================================================
    // Pending Channel Message Queue (Bootstrap Support)
    // =========================================================================

    /// Queue a pending message for a member whose mailbox isn't available yet.
    ///
    /// Messages are queued during channel bootstrap when member mailboxes
    /// haven't been exchanged yet. They're flushed when the member's mailbox
    /// address is received via GroupMailboxAddress.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel the message is for
    /// * `member_id` - Target member who we don't have a mailbox for yet
    /// * `plaintext` - Encrypted message content (will be re-encrypted when flushed)
    /// * `timestamp` - Message timestamp (for ordering and TTL)
    pub fn queue_pending_message(
        &self,
        channel_id: &ChannelId,
        member_id: &UserId,
        plaintext: &[u8],
        timestamp: u64,
    ) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let channel_id_str = channel_id.to_string();
        let member_id_str = member_id.to_string();
        let now = Utc::now().to_rfc3339();

        conn.execute(
            "INSERT INTO pending_channel_messages (channel_id, member_id, plaintext, timestamp, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![channel_id_str, member_id_str, plaintext, timestamp as i64, now],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;

        tracing::info!(
            timestamp = timestamp,
            "[PENDING_CHANNEL_MSG] Queued message for member (mailbox not yet available)"
        );

        Ok(())
    }

    /// Drain all pending messages for a specific channel/member.
    ///
    /// Called when we receive a member's mailbox address. Returns all queued
    /// messages and deletes them from the queue.
    ///
    /// # Returns
    ///
    /// Vector of (plaintext, timestamp) tuples for messages to send.
    pub fn drain_pending_messages(
        &self,
        channel_id: &ChannelId,
        member_id: &UserId,
    ) -> Result<Vec<(Vec<u8>, u64)>, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let channel_id_str = channel_id.to_string();
        let member_id_str = member_id.to_string();

        // Load pending messages
        let mut stmt = conn
            .prepare(
                "SELECT plaintext, timestamp FROM pending_channel_messages
                 WHERE channel_id = ?1 AND member_id = ?2
                 ORDER BY timestamp ASC",
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let mut rows = stmt
            .query(rusqlite::params![channel_id_str, member_id_str])
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let mut messages = Vec::new();

        while let Some(row) = rows
            .next()
            .map_err(|e| MailboxError::Storage(e.to_string()))?
        {
            let plaintext: Vec<u8> = row
                .get(0)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;
            let timestamp: i64 = row
                .get(1)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;

            messages.push((plaintext, timestamp as u64));
        }

        // Drop the rows before executing delete
        drop(rows);
        drop(stmt);

        // Delete the messages
        if !messages.is_empty() {
            conn.execute(
                "DELETE FROM pending_channel_messages WHERE channel_id = ?1 AND member_id = ?2",
                rusqlite::params![channel_id_str, member_id_str],
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

            tracing::info!(
                count = messages.len(),
                "[PENDING_CHANNEL_MSG] Drained {} pending message(s) for member",
                messages.len()
            );
        }

        Ok(messages)
    }

    /// Get count of pending messages for a channel.
    ///
    /// Useful for debugging and monitoring bootstrap progress.
    pub fn pending_message_count(&self, channel_id: &ChannelId) -> Result<usize, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let channel_id_str = channel_id.to_string();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pending_channel_messages WHERE channel_id = ?1",
                rusqlite::params![channel_id_str],
                |row| row.get(0),
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        Ok(count as usize)
    }

    /// Cleanup old pending messages (TTL expired).
    ///
    /// Removes pending messages older than the specified TTL.
    /// Default TTL is 1 hour (same as DM pending messages).
    ///
    /// # Arguments
    ///
    /// * `max_age_secs` - Maximum age in seconds (messages older than this are deleted)
    ///
    /// # Returns
    ///
    /// Number of messages cleaned up.
    pub fn cleanup_old_pending_messages(&self, max_age_secs: i64) -> Result<u32, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        // Calculate cutoff timestamp (now - max_age in nanoseconds)
        let cutoff_nanos = (Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64)
            .saturating_sub((max_age_secs as u64) * 1_000_000_000);

        let rows = conn
            .execute(
                "DELETE FROM pending_channel_messages WHERE timestamp < ?1",
                rusqlite::params![cutoff_nanos as i64],
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        if rows > 0 {
            tracing::info!(
                count = rows,
                max_age_secs = max_age_secs,
                "[PENDING_CHANNEL_MSG] Cleaned up {} expired pending message(s)",
                rows
            );
        }

        Ok(rows as u32)
    }
}
