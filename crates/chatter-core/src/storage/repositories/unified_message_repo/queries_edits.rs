use super::*;

impl UnifiedMessageRepository {
    // ========================================================================
    // Search
    // ========================================================================

    /// Search messages by text content within a conversation.
    ///
    /// Uses LIKE on `content_json` for basic text search.
    /// Results ordered by `sent_at DESC`.
    pub fn search(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
        query: &str,
        limit: u32,
    ) -> Result<Vec<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let pattern = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM messages \
             WHERE conversation_id = ?1 AND conversation_type = ?2 \
               AND content_json LIKE ?3 ESCAPE '\\' \
               AND is_deleted = 0 \
             ORDER BY sent_at DESC \
             LIMIT ?4",
            message_columns("messages")
        ))?;
        let mut rows = stmt.query(params![
            conversation_id.to_string(),
            conversation_type.as_str(),
            pattern,
            limit,
        ])?;
        collect_messages(&mut rows)
    }

    /// Search messages globally (across all conversations) by text.
    pub fn search_global(&self, query: &str, limit: u32) -> Result<Vec<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let pattern = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM messages \
             WHERE content_json LIKE ?1 ESCAPE '\\' AND is_deleted = 0 \
             ORDER BY sent_at DESC \
             LIMIT ?2",
            message_columns("messages")
        ))?;
        let mut rows = stmt.query(params![pattern, limit])?;
        collect_messages(&mut rows)
    }

    // ========================================================================
    // Conversation-level helpers
    // ========================================================================

    /// Count messages in a conversation.
    pub fn count(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<u64, StorageError> {
        let conn = self.db.read_connection()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM messages \
             WHERE conversation_id = ?1 AND conversation_type = ?2 AND is_deleted = 0",
            params![conversation_id.to_string(), conversation_type.as_str()],
            |row| row.get(0),
        )?;
        Ok(n as u64)
    }

    /// List messages in a thread.
    pub fn list_for_thread(
        &self,
        thread_id: &MessageId,
        before: Option<&MessageId>,
        limit: u32,
    ) -> Result<Vec<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        // Same (sent_at, sender_id) cursor as the main list: two members'
        // replies stamped the same millisecond must not straddle a page.
        let mut page = if let Some(before_id) = before {
            let mut stmt = conn.prepare(&format!(
                "SELECT {} FROM messages m \
                 WHERE m.thread_id = ?1 AND m.is_deleted = 0 \
                   AND (m.sent_at, m.sender_id) < (SELECT sent_at, sender_id FROM messages WHERE id = ?2) \
                 ORDER BY m.sent_at DESC, m.sender_id DESC LIMIT ?3",
                message_columns("m")
            ))?;
            let mut rows = stmt.query(params![thread_id.to_string(), before_id.to_string(), limit])?;
            collect_messages(&mut rows)?
        } else {
            let mut stmt = conn.prepare(&format!(
                "SELECT {} FROM messages \
                 WHERE thread_id = ?1 AND is_deleted = 0 \
                 ORDER BY sent_at DESC, sender_id DESC LIMIT ?2",
                message_columns("messages")
            ))?;
            let mut rows = stmt.query(params![thread_id.to_string(), limit])?;
            collect_messages(&mut rows)?
        };
        // Newest `limit` selected, returned oldest → newest: the order the
        // thread panel draws, and the order live replies are appended in.
        page.reverse();
        Ok(page)
    }

    /// Number of (non-deleted) replies in a thread — the total the thread
    /// panel pages towards.
    pub fn count_for_thread(&self, thread_id: &MessageId) -> Result<u32, StorageError> {
        let conn = self.db.read_connection()?;
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE thread_id = ?1 AND is_deleted = 0",
            params![thread_id.to_string()],
            |row| row.get(0),
        )?)
    }

    /// Whether `sender` has a live reply in the thread under `thread_id`.
    pub fn has_thread_reply_from(&self, thread_id: &MessageId, sender: &UserId) -> Result<bool, StorageError> {
        let conn = self.db.read_connection()?;
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE thread_id = ?1 AND sender_id = ?2 AND is_deleted = 0)",
            params![thread_id.to_string(), sender.to_string()],
            |row| row.get(0),
        )?)
    }

    /// Update delivery status by (conversation_id, sent_at).
    pub fn update_delivery_status(
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

    /// Soft-delete an AnonGroup row by exact (conversation, sender, timestamp).
    pub fn mark_deleted_anon_group_by_sender_timestamp(
        &self,
        conversation_id: &ChannelId,
        sender_id: &UserId,
        sent_at: u64,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE messages SET is_deleted = 1, updated_at = ?1
             WHERE conversation_id = ?2 AND sender_id = ?3 AND sent_at = ?4
             AND conversation_type = 'anon_group' AND is_deleted = 0",
            params![
                Utc::now().to_rfc3339(),
                conversation_id.to_string(),
                sender_id.to_string(),
                sent_at as i64,
            ],
        )?;
        Ok(rows > 0)
    }

    /// Pin or unpin the one row at this scope's Signal-style key
    /// (conversation, kind, author, sent clock), undeleted. One UPDATE of the
    /// pin columns only — `pinned_at` set on the transition to pinned and
    /// cleared on unpin — so a concurrent body edit of the same row is never
    /// overwritten by a stale copy of it. `Ok(false)`: no such row.
    pub fn set_pinned_scoped(
        &self,
        scope: &ReactionScope,
        author: &UserId,
        sent_at: u64,
        pinned: bool,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE messages SET pinned_at = CASE WHEN ?1 = 0 THEN NULL WHEN is_pinned = 0 THEN ?7 ELSE pinned_at END, is_pinned = ?1, updated_at = ?2
             WHERE conversation_id = ?3 AND conversation_type = ?4 AND sender_id = ?5 AND sent_at = ?6 AND is_deleted = 0",
            params![
                pinned as i32,
                Utc::now().to_rfc3339(),
                scope.id.to_string(),
                scope.kind.as_str(),
                author.to_string(),
                sent_at as i64,
                Utc::now().timestamp_nanos_opt().unwrap_or_default(),
            ],
        )?;
        Ok(rows > 0)
    }

    /// Order Saved body edits against the current row inside the write transaction.
    /// Local clocks advance beyond both creation and the last stored edit.
    /// Only the body columns change; every other row column remains intact.
    pub(crate) fn write_saved_body(
        &self,
        scope: &ReactionScope,
        author: &UserId,
        sent_at: u64,
        id: &MessageId,
        text: &str,
        clock: u64,
        local: bool,
    ) -> Result<Option<bool>, StorageError> {
        if scope.kind != ConversationType::SavedMessages
            || scope.id != ChannelId::from(*author)
            || sent_at > i64::MAX as u64
        {
            return Ok(None);
        }
        let conn = self.db.connection()?;
        let tx = conn.unchecked_transaction()?;
        let stored = tx.query_row(
            &format!("SELECT {} FROM messages WHERE id = ?1 AND conversation_id = ?2 AND conversation_type = ?3 AND sender_id = ?4 AND sent_at = ?5 AND is_deleted = 0 AND NOT EXISTS (SELECT 1 FROM deleted_messages WHERE id = ?1)", message_columns("messages")),
            params![id.to_string(), scope.id.to_string(), scope.kind.as_str(), author.to_string(), sent_at as i64],
            row_to_message,
        ).optional()?.transpose()?;
        let Some(mut stored) = stored else {
            return Ok(None);
        };
        let stored_clock = stored
            .edited_at
            .and_then(|t| t.timestamp_nanos_opt())
            .unwrap_or_default()
            .max(0) as u64;
        let clock =
            if local {
                clock.max(stored_clock.max(sent_at).checked_add(1).ok_or_else(|| {
                    StorageError::InvalidData("Saved edit clock exhausted".into())
                })?)
            } else {
                clock
            };
        if clock > i64::MAX as u64 {
            return Err(StorageError::InvalidData(
                "Saved edit clock out of range".into(),
            ));
        }
        let body_changed = !text.is_empty()
            && (local
                || clock > stored_clock
                || (clock != 0
                    && clock == stored_clock
                    && text > stored.as_text().unwrap_or_default()));
        if body_changed {
            stored.content = match &stored.content {
                MessageContent::Mixed(mixed) => {
                    MessageContent::Mixed(crate::domain::message::MixedContent {
                        text: Some(text.to_string()),
                        attachment_ids: mixed.attachment_ids.clone(),
                    })
                }
                _ => MessageContent::Text(text.to_string()),
            };
            stored.edited_at = Some(DateTime::from_timestamp_nanos(clock as i64));
        }
        if body_changed {
            let content_json = serde_json::to_string(&stored.content)
                .map_err(|e| StorageError::InvalidData(e.to_string()))?;
            tx.execute(
                "UPDATE messages SET content_type = ?1, content_json = ?2, is_edited = 1, edited_at = ?3, updated_at = ?4 WHERE id = ?5",
                params![content_type_discriminant(&stored.content), content_json, stored.edited_at.map(|t| t.to_rfc3339()), Utc::now().to_rfc3339(), id.to_string()],
            )?;
        }
        if local && body_changed {
            super::super::saved_sync_repo::SavedSyncRepository::mark_dirty_with(&tx, author, id)?;
        }
        tx.commit()?;
        Ok(Some(body_changed))
    }

    /// Replace the body of the one row at this scope's Signal-style key,
    /// undeleted, marking it edited at `edited_at`. One UPDATE of the body
    /// columns only — `content_json` with its `content_type` discriminant
    /// (an edit may change the variant: a Saved edit turns an `AI` body into
    /// `Text`) — so a concurrent pin of the same row keeps its pin.
    /// `Ok(false)`: no such row.
    pub fn set_body_scoped(
        &self,
        scope: &ReactionScope,
        author: &UserId,
        sent_at: u64,
        content: &MessageContent,
        edited_at: DateTime<Utc>,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let content_json = serde_json::to_string(content)
            .map_err(|e| StorageError::InvalidData(format!("serialize content: {}", e)))?;
        let rows = conn.execute(
            "UPDATE messages SET content_type = ?7, content_json = ?1, is_edited = 1, edited_at = ?2, updated_at = ?2
             WHERE conversation_id = ?3 AND conversation_type = ?4 AND sender_id = ?5 AND sent_at = ?6 AND is_deleted = 0",
            params![
                content_json,
                edited_at.to_rfc3339(),
                scope.id.to_string(),
                scope.kind.as_str(),
                author.to_string(),
                sent_at as i64,
                content_type_discriminant(content),
            ],
        )?;
        Ok(rows > 0)
    }

    /// Update message text by (conversation_id, sent_at) — for edits.
    pub fn update_text_by_timestamp(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
        sent_at: u64,
        new_text: &str,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let content = MessageContent::Text(new_text.to_string());
        let content_json = serde_json::to_string(&content)
            .map_err(|e| StorageError::InvalidData(format!("serialize content: {}", e)))?;
        let rows = conn.execute(
            "UPDATE messages SET content_json = ?1, is_edited = 1, updated_at = ?2
             WHERE conversation_id = ?3 AND conversation_type = ?4 AND sent_at = ?5",
            params![
                content_json,
                Utc::now().to_rfc3339(),
                conversation_id.to_string(),
                conversation_type.as_str(),
                sent_at as i64,
            ],
        )?;
        Ok(rows > 0)
    }

    /// Set pin state by (conversation_id, sent_at).
    pub fn set_pinned_by_timestamp(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
        sent_at: u64,
        pinned: bool,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE messages SET pinned_at = CASE WHEN ?1 = 0 THEN NULL WHEN is_pinned = 0 THEN ?6 ELSE pinned_at END, is_pinned = ?1, updated_at = ?2
             WHERE conversation_id = ?3 AND conversation_type = ?4 AND sent_at = ?5 AND is_deleted = 0",
            params![
                pinned as i32,
                Utc::now().to_rfc3339(),
                conversation_id.to_string(),
                conversation_type.as_str(),
                sent_at as i64,
                Utc::now().timestamp_nanos_opt().unwrap_or_default(),
            ],
        )?;
        Ok(rows > 0)
    }

}
