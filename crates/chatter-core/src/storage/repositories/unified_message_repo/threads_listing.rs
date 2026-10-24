use super::*;

impl UnifiedMessageRepository {
    /// Sidebar activity for all mesh conversations in one message-table read.
    pub fn mesh_sidebar_stats(
        &self,
    ) -> Result<HashMap<(String, ConversationType), (Option<DateTime<Utc>>, u32)>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "WITH ranked AS (SELECT conversation_id, conversation_type, created_at,
                    is_read, is_deleted,
                    ROW_NUMBER() OVER (PARTITION BY conversation_id, conversation_type
                        ORDER BY is_deleted ASC, sent_at DESC) AS latest_rank
                FROM messages WHERE conversation_type IN ('mesh', 'mesh_dm'))
             SELECT conversation_id, conversation_type,
                    MAX(CASE WHEN latest_rank = 1 AND is_deleted = 0 THEN created_at END),
                    SUM(CASE WHEN is_read = 0 AND is_deleted = 0 THEN 1 ELSE 0 END)
             FROM ranked GROUP BY conversation_id, conversation_type",
        )?;
        let mut rows = stmt.query([])?;
        let mut stats = HashMap::new();
        while let Some(row) = rows.next()? {
            let kind: String = row.get(1)?;
            let kind = ConversationType::from_str_value(&kind)?;
            let last: Option<String> = row.get(2)?;
            let unread: i64 = row.get(3)?;
            stats.insert(
                (row.get(0)?, kind),
                (
                    last.as_deref().map(parse_datetime),
                    u32::try_from(unread.max(0)).unwrap_or(u32::MAX),
                ),
            );
        }
        Ok(stats)
    }

    /// Latest visible activity for many conversations in one reader checkout.
    /// The main-message bound matches the sidebar's eight 32-row pages.
    /// Rank narrow rows first, then hydrate only the chosen preview per chat.
    pub fn sidebar_latest_batch(
        &self,
        ids: &[ChannelId],
    ) -> Result<HashMap<(String, ConversationType), Message>, StorageError> {
        let mut latest = HashMap::new();
        if ids.is_empty() {
            return Ok(latest);
        }
        let conn = self.db.read_connection()?;
        let mut message_stmt = conn.prepare(&format!(
            "SELECT {} FROM messages m WHERE m.id = ?1",
            message_columns("m"),
        ))?;
        for chunk in ids.chunks(400) {
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let sql = format!(
                "WITH ranked AS (SELECT m.id, m.conversation_id, m.conversation_type, \
                 m.sent_at, m.sender_id, \
                 ROW_NUMBER() OVER (PARTITION BY m.conversation_id, m.conversation_type \
                 ORDER BY m.sent_at DESC, m.sender_id DESC) AS rank \
                 FROM messages m WHERE m.conversation_id IN ({placeholders}) \
                 AND m.thread_id IS NULL \
                 AND (m.conversation_type NOT IN ('anon_dm', 'anon_group') OR m.is_deleted = 0)), \
                 candidates AS (SELECT r.id, r.conversation_id, r.conversation_type, r.sent_at, r.sender_id, \
                 ROW_NUMBER() OVER (PARTITION BY r.conversation_id, r.conversation_type \
                 ORDER BY r.sent_at DESC, r.sender_id DESC) AS preview_rank \
                 FROM ranked r JOIN messages candidate ON candidate.id = r.id \
                 WHERE r.rank <= 256 \
                   AND (r.conversation_type IN ('anon_dm', 'anon_group') \
                     OR (CASE WHEN json_valid(candidate.content_json) \
                       THEN json_type(candidate.content_json, '$.System.DisappearingTimerChanged') IS NULL \
                       ELSE 0 END))) \
                 SELECT c.id, c.conversation_id, c.conversation_type \
                 FROM candidates c \
                 ORDER BY c.conversation_id, c.conversation_type, c.preview_rank",
            );
            let mut stmt = conn.prepare(&sql)?;
            let params = chunk.iter().map(ToString::to_string).collect::<Vec<_>>();
            let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
            let mut resolved = std::collections::HashSet::new();
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                let conversation_id: String = row.get(1)?;
                let kind = ConversationType::from_str_value(&row.get::<_, String>(2)?)?;
                let candidate_key = (conversation_id, kind);
                if resolved.contains(&candidate_key) {
                    continue;
                }
                let mut full_rows = message_stmt.query([id])?;
                let Some(message) = collect_messages(&mut full_rows)?.into_iter().next() else {
                    continue;
                };
                resolved.insert(candidate_key);
                let key = (message.conversation_id.to_string(), message.conversation_type.clone());
                latest.insert(key, message);
            }
        }
        Ok(latest)
    }

    /// Unread messages containing a possible mention, hydrated in one read.
    pub fn sidebar_unread_mention_candidates(
        &self,
        ids: &[ChannelId],
    ) -> Result<Vec<Message>, StorageError> {
        let mut out = Vec::new();
        if ids.is_empty() {
            return Ok(out);
        }
        let conn = self.db.read_connection()?;
        for chunk in ids.chunks(400) {
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let sql = format!(
                "SELECT {} FROM messages m WHERE m.conversation_id IN ({placeholders}) \
                 AND m.is_read = 0 AND m.is_deleted = 0 AND m.content_json LIKE '%@%'",
                message_columns("m"),
            );
            let mut stmt = conn.prepare(&sql)?;
            let params = chunk.iter().map(ToString::to_string).collect::<Vec<_>>();
            let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
            out.extend(collect_messages(&mut rows)?);
        }
        Ok(out)
    }

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
