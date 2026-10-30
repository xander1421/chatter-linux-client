use super::*;

impl UnifiedMessageRepository {
    /// Advance the durable read position and clear messages already present.
    /// Called inside the same transaction as the triggering read or own send.
    pub(super) fn advance_read_horizon(
        conn: &rusqlite::Connection,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
        sent_at: u64,
    ) -> Result<usize, StorageError> {
        if sent_at > i64::MAX as u64 {
            return Err(StorageError::InvalidData(
                "read timestamp out of range".into(),
            ));
        }
        conn.execute(
            "INSERT INTO conversation_read_horizons (conversation_type, conversation_id, sent_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(conversation_type, conversation_id) DO UPDATE SET
                 sent_at = MAX(sent_at, excluded.sent_at)",
            params![
                conversation_type.as_str(),
                conversation_id.to_string(),
                sent_at as i64
            ],
        )?;
        let rows = conn.execute(
            "UPDATE messages SET is_read = 1, updated_at = ?1
             WHERE conversation_type = ?2 AND conversation_id = ?3 AND is_read = 0
               AND sent_at <= (SELECT sent_at FROM conversation_read_horizons
                               WHERE conversation_type = ?2 AND conversation_id = ?3)",
            params![
                Utc::now().to_rfc3339(),
                conversation_type.as_str(),
                conversation_id.to_string()
            ],
        )?;
        Ok(rows)
    }

    /// Apply own-device reads as conversation horizons. The sync may precede
    /// messages it covers; store its highest position so later inserts land
    /// read too. Never touch anonymous or mesh conversations.
    pub fn mark_read_synced(
        &self,
        reads: &[(ChannelId, UserId, u64)],
        self_user_id: &UserId,
    ) -> Result<Vec<ChannelId>, StorageError> {
        let conn = self.db.connection()?;
        let tx = conn.unchecked_transaction()?;
        let mut read_conversations = Vec::new();
        let mut horizons: HashMap<(ChannelId, ConversationType), u64> = HashMap::new();
        for (conversation_id, sender_id, sent_at) in reads {
            if sender_id == self_user_id || *sent_at > i64::MAX as u64 {
                continue;
            }
            let kind = if *conversation_id == ChannelId::from(*sender_id) {
                ConversationType::ServerDm
            } else {
                ConversationType::Channel
            };
            horizons
                .entry((*conversation_id, kind))
                .and_modify(|current| *current = (*current).max(*sent_at))
                .or_insert(*sent_at);
        }
        for ((conversation_id, kind), sent_at) in horizons {
            let previous: Option<i64> = tx
                .query_row(
                    "SELECT sent_at FROM conversation_read_horizons WHERE conversation_type = ?1 AND conversation_id = ?2",
                    params![kind.as_str(), conversation_id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            let rows = Self::advance_read_horizon(&tx, &conversation_id, &kind, sent_at)?;
            // The horizon can move before its messages arrive. A notification
            // for this conversation is stale even when no local row flipped.
            if (previous.is_none_or(|old| sent_at > old as u64) || rows > 0)
                && !read_conversations.contains(&conversation_id)
            {
                read_conversations.push(conversation_id);
            }
        }
        super::super::ephemeral_repo::start_read_countdowns(&tx)?;
        tx.commit()?;
        Ok(read_conversations)
    }

    /// Update delivery status by timestamp with automatic conversation lookup.
    ///
    /// For callers that lack conversation context (e.g. cross-channel receipts),
    /// looks up matching message IDs first, then performs scoped updates by
    /// primary key. Never does an unscoped `WHERE sent_at = ?` UPDATE.
    pub fn update_delivery_status_by_timestamp_with_lookup(
        &self,
        sent_at: u64,
        status: &str,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        // Step 1: Find message IDs matching this timestamp (read-only).
        // Identity-bearing server receipts use MessageReceiptRepository. Anonymous
        // control events cannot acknowledge a server message with the same time.
        let mut stmt = conn.prepare("SELECT id FROM messages WHERE sent_at = ?1 AND conversation_type IN ('anon_dm', 'anon_group')")?;
        let ids: Vec<String> = stmt
            .query_map(params![sent_at as i64], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        if ids.is_empty() {
            return Ok(false);
        }
        // Step 2: Update each by primary key (scoped, no cross-conversation risk).
        let now = Utc::now().to_rfc3339();
        let mut updated = false;
        for id in &ids {
            let rows = conn.execute(
                "UPDATE messages SET delivery_status = ?1, updated_at = ?2 WHERE id = ?3",
                params![status, now, id],
            )?;
            if rows > 0 {
                updated = true;
            }
        }
        Ok(updated)
    }

    /// Update delivery status by conversation + timestamps (batch).
    /// Remote receipts are authoritative: delayed local completion/failure
    /// cannot lower sent/delivered/read, and receipt updates remain monotonic.
    pub fn update_delivery_status_by_timestamps(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
        timestamps: &[u64],
        status: &str,
    ) -> Result<usize, StorageError> {
        if timestamps.is_empty() {
            return Ok(0);
        }
        let mut conn = self.db.connection()?;
        let now = Utc::now().to_rfc3339();
        let transaction = conn.transaction()?;
        let mut statement = transaction.prepare_cached(
            "UPDATE messages SET delivery_status = ?1, updated_at = ?2
             WHERE conversation_id = ?3 AND conversation_type = ?4 AND sent_at = ?5
               AND CASE delivery_status
                     WHEN 'read' THEN 4 WHEN 'delivered' THEN 3
                     WHEN 'sent' THEN 2 ELSE 0 END
                   <= CASE ?1
                     WHEN 'read' THEN 4 WHEN 'delivered' THEN 3
                     WHEN 'sent' THEN 2 ELSE 0 END",
        )?;
        let mut total = 0;
        for &ts in timestamps {
            let rows = statement.execute(params![
                    status,
                    now,
                    channel_id.to_string(),
                    conversation_type.as_str(),
                    ts as i64
                ])?;
            total += rows;
        }
        drop(statement);
        transaction.commit()?;
        Ok(total)
    }

    /// List all pinned messages for a conversation.
    /// Alias for `list_pinned()` matching old MessageRepository API.
    pub fn list_pinned_for_conversation(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<Vec<Message>, StorageError> {
        self.list_pinned(channel_id, conversation_type)
    }

    /// List all pinned messages in a channel (no conversation_type filter).
    pub fn list_pinned_all(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<Vec<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM messages \
             WHERE conversation_id = ?1 AND conversation_type = ?2 \
               AND is_pinned = 1 AND is_deleted = 0 \
             ORDER BY COALESCE(pinned_at, 0) DESC, id DESC",
            message_columns("messages"),
        ))?;
        let mut rows = stmt.query(params![channel_id.to_string(), conversation_type.as_str()])?;
        collect_messages(&mut rows).map(|messages| {
            messages
                .into_iter()
                .filter(|message| !crate::services::disappearing::is_timer_notice(message))
                .collect()
        })
    }

    /// List recent messages by sender.
    pub fn list_by_sender(
        &self,
        sender_id: &UserId,
        limit: u32,
    ) -> Result<Vec<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM messages WHERE sender_id = ?1 ORDER BY sent_at DESC LIMIT ?2",
            message_columns("messages"),
        ))?;
        let mut rows = stmt.query(params![sender_id.to_string(), limit])?;
        collect_messages(&mut rows)
    }

}
