use super::*;

impl UnifiedMessageRepository {
    // ========================================================================
    // Thread operations
    // ========================================================================

    /// Get thread metadata.
    pub fn get_thread(&self, root_message_id: &MessageId) -> Result<Option<Thread>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT root_message_id, conversation_id, reply_count, last_reply_at, participant_ids_blob
             FROM threads WHERE root_message_id = ?1",
        )?;
        match stmt.query_row(params![root_message_id.to_string()], row_to_thread) {
            Ok(thread) => Ok(Some(thread?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Upsert thread metadata.
    pub fn upsert_thread(&self, thread: &Thread) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let participant_ids_blob = db_serialize(&thread.participant_ids)?;
        conn.execute(
            "INSERT INTO threads (root_message_id, conversation_id, reply_count, last_reply_at, participant_ids_blob)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(root_message_id) DO UPDATE SET
                reply_count = excluded.reply_count,
                last_reply_at = excluded.last_reply_at,
                participant_ids_blob = excluded.participant_ids_blob",
            params![
                thread.root_message_id.to_string(),
                thread.conversation_id.to_string(),
                thread.reply_count,
                thread.last_reply_at.map(|dt| dt.to_rfc3339()),
                participant_ids_blob,
            ],
        )?;
        Ok(())
    }

    // ========================================================================
    // Compatibility methods (matching MessageRepository API)
    // ========================================================================

    /// List messages for a channel with cursor-based pagination.
    ///
    /// Returns messages in reverse chronological order (newest first).
    /// Use `before` to paginate - pass the last message's ID to get the next page.
    ///
    /// Thread replies (`thread_id IS NOT NULL`) are excluded — they belong to
    /// the thread panel (`list_for_thread`), not the main conversation, and
    /// showing them in both places double-renders every reply.
    pub fn list_for_channel(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
        before: Option<&MessageId>,
        limit: u32,
    ) -> Result<Vec<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        if let Some(before_id) = before {
            let aliased = message_columns("m");
            let mut stmt = conn.prepare(&format!(
                "SELECT {} FROM messages m \
                 WHERE m.conversation_id = ?1 AND m.conversation_type = ?2 \
                   AND m.thread_id IS NULL \
                   AND (m.sent_at, m.sender_id) < (SELECT sent_at, sender_id FROM messages WHERE id = ?3) \
                 ORDER BY m.sent_at DESC, m.sender_id DESC \
                 LIMIT ?4",
                aliased,
            ))?;
            let mut rows = stmt.query(params![
                channel_id.to_string(),
                conversation_type.as_str(),
                before_id.to_string(),
                limit
            ])?;
            collect_messages(&mut rows)
        } else {
            let mut stmt = conn.prepare(&format!(
                "SELECT {} FROM messages \
                 WHERE conversation_id = ?1 AND conversation_type = ?2 \
                   AND thread_id IS NULL \
                 ORDER BY sent_at DESC, sender_id DESC \
                 LIMIT ?3",
                message_columns("messages"),
            ))?;
            let mut rows = stmt.query(params![
                channel_id.to_string(),
                conversation_type.as_str(),
                limit
            ])?;
            collect_messages(&mut rows)
        }
    }

    /// Include thread replies and locally deleted rows for a full local purge.
    pub fn list_all_for_conversation(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<Vec<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM messages WHERE conversation_id=?1 AND conversation_type=?2",
            message_columns("messages")
        ))?;
        let mut rows = stmt.query(params![channel_id.to_string(), conversation_type.as_str()])?;
        collect_messages(&mut rows)
    }

    /// Reply counts per thread root for one conversation.
    ///
    /// Feeds the "N replies" indicator on the root bubble. Derived from the
    /// messages table (the `threads` table is never written).
    pub fn reply_counts_for_channel(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<HashMap<MessageId, u32>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT thread_id, COUNT(*) FROM messages \
             WHERE conversation_id = ?1 AND conversation_type = ?2 \
               AND thread_id IS NOT NULL AND is_deleted = 0 \
             GROUP BY thread_id",
        )?;
        let rows = stmt.query_map(
            params![channel_id.to_string(), conversation_type.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )?;
        let mut counts = HashMap::new();
        for row in rows {
            let (root, count) = row?;
            if let Ok(root_id) = root.parse::<MessageId>() {
                counts.insert(root_id, count as u32);
            }
        }
        Ok(counts)
    }

    /// List messages for a conversation by channel_id and conversation_type.
    /// Alias for `list()` matching old MessageRepository API.
    pub fn list_for_conversation(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
        before_sent_at: Option<u64>,
        limit: u32,
    ) -> Result<Vec<Message>, StorageError> {
        self.list(channel_id, conversation_type, before_sent_at, limit)
    }

    /// List messages in chronological order (oldest first).
    ///
    /// Fetches the N most recent messages before `before_sent_at`, returned
    /// in ascending timestamp order — ready for chat display without reversal.
    pub fn list_asc(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
        before_sent_at: Option<u64>,
        limit: u32,
    ) -> Result<Vec<Message>, StorageError> {
        let mut messages = self.list(conversation_id, conversation_type, before_sent_at, limit)?;
        messages.reverse();
        Ok(messages)
    }

    /// List messages for a channel in chronological order (oldest first).
    ///
    /// Same as `list_for_channel` but returns ASC order for direct UI display.
    pub fn list_for_channel_asc(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
        before: Option<&MessageId>,
        limit: u32,
    ) -> Result<Vec<Message>, StorageError> {
        let mut messages = self.list_for_channel(channel_id, conversation_type, before, limit)?;
        messages.reverse();
        Ok(messages)
    }

    /// The conversation's newest live thread reply. Replies stay out of the
    /// main list, yet they order the chat list and count as unread, so the
    /// row's preview and time must be able to show one.
    pub fn latest_thread_reply(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<Option<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM messages \
             WHERE conversation_id = ?1 AND conversation_type = ?2 \
               AND thread_id IS NOT NULL AND is_deleted = 0 \
             ORDER BY sent_at DESC, sender_id DESC LIMIT 1",
            message_columns("messages"),
        ))?;
        let mut rows = stmt.query(params![conversation_id.to_string(), conversation_type.as_str()])?;
        Ok(collect_messages(&mut rows)?.pop())
    }

    /// List messages for a conversation in chronological order (oldest first).
    /// Alias for `list_asc()` matching old MessageRepository API.
    pub fn list_for_conversation_asc(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
        before_sent_at: Option<u64>,
        limit: u32,
    ) -> Result<Vec<Message>, StorageError> {
        self.list_asc(channel_id, conversation_type, before_sent_at, limit)
    }

    /// Get sent_at timestamps of messages from a specific sender in a conversation.
    ///
    /// Used for read receipts — returns timestamps of peer messages for acknowledgement.
    pub fn get_timestamps_from_sender(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
        sender_id: &UserId,
        limit: u32,
    ) -> Result<Vec<u64>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT sent_at FROM messages \
             WHERE conversation_id = ?1 AND conversation_type = ?2 \
               AND sender_id = ?3 AND is_deleted = 0 \
             ORDER BY sent_at DESC \
             LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![
                channel_id.to_string(),
                conversation_type.as_str(),
                sender_id.to_string(),
                limit,
            ],
            |row| row.get::<_, i64>(0).map(|v| v as u64),
        )?;
        let mut timestamps = Vec::new();
        for row in rows {
            timestamps.push(row?);
        }
        Ok(timestamps)
    }

    /// Get the latest message in a conversation (for preview).
    /// Alias for `get_latest()` matching old MessageRepository API.
    pub fn get_latest_for_conversation(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<Option<Message>, StorageError> {
        self.get_latest(channel_id, conversation_type)
    }

    /// Get a message by conversation + timestamp (any sender).
    pub fn get_by_conversation_timestamp(
        &self,
        channel_id: &ChannelId,
        timestamp_ns: u64,
    ) -> Result<Option<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM messages \
             WHERE conversation_id = ?1 AND sent_at = ?2 AND is_deleted = 0 \
             LIMIT 1",
            message_columns("messages"),
        ))?;
        match stmt.query_row(
            params![channel_id.to_string(), timestamp_ns as i64],
            row_to_message,
        ) {
            Ok(msg) => Ok(Some(msg?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Get a message by (channel_id, sender_id, sent_at) tuple.
    pub fn get_by_conversation_sender_timestamp(
        &self,
        channel_id: &ChannelId,
        sender_id: &UserId,
        timestamp_ns: u64,
    ) -> Result<Option<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM messages \
             WHERE conversation_id = ?1 AND sender_id = ?2 AND sent_at = ?3 AND is_deleted = 0 \
             LIMIT 1",
            message_columns("messages"),
        ))?;
        match stmt.query_row(
            params![
                channel_id.to_string(),
                sender_id.to_string(),
                timestamp_ns as i64
            ],
            row_to_message,
        ) {
            Ok(msg) => Ok(Some(msg?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Count messages in a conversation (returns i64 for compat).
    pub fn count_in_conversation(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<i64, StorageError> {
        Ok(self.count(channel_id, conversation_type)? as i64)
    }

    /// Count messages in a channel (no conversation_type filter).
    /// Update delivery status by conversation + timestamp (scoped to one conversation).
    pub fn update_delivery_status_by_timestamp(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
        sent_at: u64,
        status: &str,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE messages SET delivery_status = ?1, updated_at = ?2
             WHERE conversation_id = ?3 AND conversation_type = ?4 AND sent_at = ?5",
            params![
                status,
                Utc::now().to_rfc3339(),
                conversation_id.to_string(),
                conversation_type.as_str(),
                sent_at as i64,
            ],
        )?;
        Ok(rows > 0)
    }

}
