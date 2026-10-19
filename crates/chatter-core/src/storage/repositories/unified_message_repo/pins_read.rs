use super::*;

impl UnifiedMessageRepository {
    // ========================================================================
    // Pin / Unpin
    // ========================================================================

    /// Pin a message by ID.
    pub fn pin(&self, id: &MessageId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE messages SET pinned_at = CASE WHEN is_pinned = 0 THEN ?3 ELSE pinned_at END, is_pinned = 1, updated_at = ?1 WHERE id = ?2 AND is_deleted = 0",
            params![Utc::now().to_rfc3339(), id.to_string(), Utc::now().timestamp_nanos_opt().unwrap_or_default()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("Message {}", id)));
        }
        Ok(())
    }

    /// Unpin a message by ID.
    pub fn unpin(&self, id: &MessageId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE messages SET is_pinned = 0, pinned_at = NULL, updated_at = ?1 WHERE id = ?2",
            params![Utc::now().to_rfc3339(), id.to_string()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("Message {}", id)));
        }
        Ok(())
    }

    /// List all pinned messages in a conversation.
    pub fn list_pinned(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<Vec<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM messages \
             WHERE conversation_id = ?1 AND conversation_type = ?2 \
               AND is_pinned = 1 AND is_deleted = 0 \
             ORDER BY COALESCE(pinned_at, 0) DESC, id DESC",
            message_columns("messages")
        ))?;
        let mut rows = stmt.query(params![
            conversation_id.to_string(),
            conversation_type.as_str(),
        ])?;
        collect_messages(&mut rows).map(|messages| {
            messages
                .into_iter()
                .filter(|message| !crate::services::disappearing::is_timer_notice(message))
                .collect()
        })
    }

    // ========================================================================
    // Read / Unread
    // ========================================================================

    /// Mark a single message as read.
    pub fn mark_read(&self, id: &MessageId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "UPDATE messages SET is_read = 1, updated_at = ?1 WHERE id = ?2 AND is_read = 0",
            params![Utc::now().to_rfc3339(), id.to_string()],
        )?;
        super::super::ephemeral_repo::start_read_countdowns(&conn)
    }

    /// Mark a single message as unread.
    pub fn mark_unread(&self, id: &MessageId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "UPDATE messages SET is_read = 0, updated_at = ?1 WHERE id = ?2 AND is_read = 1",
            params![Utc::now().to_rfc3339(), id.to_string()],
        )?;
        Ok(())
    }

    /// Mark all messages in a conversation as read, returning the boundary it
    /// just erased.
    ///
    /// The read *has* to happen here rather than in the caller: opening a
    /// conversation marks it read before the message page is loaded, so any
    /// caller that asks for the boundary afterwards gets an empty one. Both
    /// halves in one call means that ordering cannot be got wrong.
    pub fn mark_conversation_read(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<UnreadAnchor, StorageError> {
        Ok(self
            .mark_conversation_read_returning(conversation_id, conversation_type)?
            .0)
    }

    /// [`Self::mark_conversation_read`], also returning the `(sender, sent_at)`
    /// of every message it flipped — what a `ReadSync` names to our other
    /// devices. Read by the same UPDATE, so a message arriving meanwhile is
    /// either flipped and named, or left unread.
    pub fn mark_conversation_read_returning(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<(UnreadAnchor, Vec<(UserId, u64)>), StorageError> {
        let anchor = self.unread_anchor(conversation_id, conversation_type)?;
        let conn = self.db.connection()?;
        let tx = conn.unchecked_transaction()?;
        let flipped = tx
            .prepare(
                "UPDATE messages SET is_read = 1 WHERE conversation_id = ?1 AND conversation_type = ?2 AND is_read = 0
                 RETURNING sender_id, sent_at",
            )?
            .query_map(params![conversation_id.to_string(), conversation_type.as_str()], |row| {
                Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        if matches!(
            conversation_type,
            ConversationType::ServerDm | ConversationType::Channel
        ) {
            if let Some(sent_at) = flipped.iter().map(|(_, sent_at)| *sent_at).max() {
                Self::advance_read_horizon(
                    &tx,
                    conversation_id,
                    conversation_type,
                    sent_at as u64,
                )?;
            }
        }
        super::super::ephemeral_repo::start_read_countdowns(&tx)?;
        tx.commit()?;
        let read = flipped
            .into_iter()
            .filter_map(|(sender, sent_at)| Some((sender?.parse().ok()?, sent_at as u64)))
            .collect();
        Ok((anchor, read))
    }

    /// The unread boundary of a conversation, without clearing it.
    ///
    /// Main-list messages only: thread replies are not rows of the list the
    /// divider is drawn in, so one of them can neither be the first unread
    /// row nor count toward the pill under it, and as a growth target for
    /// `UnreadAnchor::opening_page` it would load the whole history looking
    /// for a row that is not there.
    ///
    /// One query, served by `idx_messages_unread`
    /// (conversation_type, conversation_id, is_read) WHERE is_read = 0.
    pub fn unread_anchor(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<UnreadAnchor, StorageError> {
        let conn = self.db.read_connection()?;
        let unread_sent_at = conn
            .prepare(
                "SELECT sent_at FROM messages \
                 WHERE conversation_id = ?1 AND conversation_type = ?2 \
                   AND is_read = 0 AND is_deleted = 0 AND thread_id IS NULL \
                 ORDER BY sent_at",
            )?
            .query_map(
                params![conversation_id.to_string(), conversation_type.as_str()],
                |row| row.get::<_, i64>(0).map(|v| v as u64),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(UnreadAnchor {
            unread_count: unread_sent_at.len() as u32,
            unread_sent_at,
        })
    }

    /// Get the count of unread messages in a conversation.
    pub fn get_unread_count(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<u64, StorageError> {
        let conn = self.db.read_connection()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM messages \
             WHERE conversation_id = ?1 AND conversation_type = ?2 \
               AND is_read = 0 AND is_deleted = 0",
            params![conversation_id.to_string(), conversation_type.as_str()],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

}
