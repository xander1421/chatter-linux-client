use super::*;

fn queue_message_attachments(
    conn: &rusqlite::Connection,
    id: &MessageId,
) -> Result<(), StorageError> {
    let blob: Option<Vec<u8>> = conn.query_row(
        "SELECT attachments_blob FROM messages WHERE id = ?1",
        params![id.to_string()],
        |row| row.get(0),
    ).optional()?;
    if let Some(blob) = blob {
        let attachments: Vec<AttachmentId> = db_deserialize(&blob)?;
        for attachment in attachments {
            conn.execute(
                "INSERT OR IGNORE INTO attachment_cleanup_queue (attachment_id) VALUES (?1)",
                params![attachment.to_string()],
            )?;
        }
    }
    Ok(())
}

impl UnifiedMessageRepository {
    /// Delete a message by ID (hard delete — CASCADE removes unified_reactions).
    /// Hard-delete every message of one conversation type.
    ///
    /// Exists for the mesh panic wipe, and it deletes by *type* rather than by
    /// walking open conversations on purpose: leaving a geohash cell keeps its
    /// history by design, so a wipe driven from the open-conversation list would
    /// silently leave everything said in every cell the user ever left. A panic
    /// wipe that leaves messages behind is worse than none, because the user
    /// believes it worked.
    ///
    /// Returns the number of rows removed.
    /// Every message of one conversation type.
    ///
    /// The sibling of [`delete_by_conversation_type`](Self::delete_by_conversation_type),
    /// and it exists for the same caller: a panic wipe has to know what it is
    /// about to destroy before it destroys it. Mesh messages can carry
    /// attachments whose bytes live *outside* the database, so deleting the rows
    /// first would orphan the files with no way left to find them.
    ///
    /// Unbounded on purpose. A caller that is about to delete all of these can
    /// afford to hold them, and a limit here would silently spare whatever fell
    /// past it — which for a panic wipe is the worst possible failure.
    pub fn list_by_conversation_type(
        &self,
        conversation_type: &crate::domain::message::ConversationType,
    ) -> Result<Vec<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM messages WHERE conversation_type = ?1",
            message_columns("messages")
        ))?;
        let mut rows = stmt.query(params![conversation_type.as_str()])?;
        collect_messages(&mut rows)
    }

    pub fn delete_by_conversation_type(
        &self,
        conversation_type: &crate::domain::message::ConversationType,
    ) -> Result<usize, StorageError> {
        let conn = self.db.connection()?;
        let tx = conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare("SELECT id FROM messages WHERE conversation_type = ?1")?;
            let ids: Vec<String> = stmt.query_map([conversation_type.as_str()], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            for id in ids {
                let id: MessageId = id.parse().map_err(|_| StorageError::InvalidData("Invalid message ID".into()))?;
                queue_message_attachments(&tx, &id)?;
            }
        }
        let rows = tx.execute(
            "DELETE FROM messages WHERE conversation_type = ?1",
            params![conversation_type.as_str()],
        )?;
        tx.commit()?;
        drop(conn);
        if let Err(error) = super::super::attachment_repo::AttachmentRepository::retry_pending_cleanup(&self.db) {
            tracing::warn!(%error, "attachment cleanup will be retried");
        }
        Ok(rows)
    }

    /// Commit an authenticated own Saved deletion, including before arrival.
    /// The tombstone is scoped to Saved + owner, never a global arbitrary ID.
    pub(crate) fn delete_own_saved_exact(
        &self,
        owner: &UserId,
        id: &MessageId,
        sent_at: u64,
    ) -> Result<(), StorageError> {
        self.write_own_saved_delete(owner, id, sent_at, false)
    }

    /// Persist intent and suppression atomically, retaining the row until ACK.
    pub(crate) fn prepare_own_saved_delete(
        &self,
        owner: &UserId,
        id: &MessageId,
        sent_at: u64,
    ) -> Result<(), StorageError> {
        crate::services::gateway::submission::check_send_session()?;
        self.write_own_saved_delete(owner, id, sent_at, true)
    }

    fn write_own_saved_delete(
        &self,
        owner: &UserId,
        id: &MessageId,
        sent_at: u64,
        enqueue: bool,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let tx = conn.unchecked_transaction()?;
        if enqueue {
            crate::services::gateway::submission::check_send_session()?;
        }
        let invalid = || StorageError::InvalidData("Saved deletion identity mismatch".into());
        let current: Option<String> = tx
            .query_row(
                "SELECT value FROM settings WHERE key = 'auth.current_user_id'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if current
            .and_then(|value| serde_json::from_str::<String>(&value).ok())
            .as_deref()
            != Some(owner.to_string().as_str())
            || sent_at == 0
            || sent_at > i64::MAX as u64
        {
            return Err(invalid());
        }
        let existing: Option<(String, String, Option<String>, i64)> = tx.query_row(
            "SELECT conversation_type, conversation_id, sender_id, sent_at FROM messages WHERE id = ?1",
            params![id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        ).optional()?;
        if existing.is_some_and(|(kind, conversation, sender, clock)| {
            kind != "saved_messages"
                || conversation != owner.to_string()
                || sender.as_deref() != Some(owner.to_string().as_str())
                || clock != sent_at as i64
        }) {
            return Err(invalid());
        }
        tx.execute(
            "INSERT OR IGNORE INTO saved_delete_tombstones (owner_id, message_id) VALUES (?1, ?2)",
            params![owner.to_string(), id.to_string()],
        )?;
        if enqueue {
            let pending_clock: Option<i64> = tx.query_row(
                "SELECT sent_at FROM saved_delete_outbox WHERE owner_id = ?1 AND message_id = ?2",
                params![owner.to_string(), id.to_string()], |r| r.get(0),
            ).optional()?;
            if pending_clock.is_some_and(|clock| clock != sent_at as i64) {
                return Err(invalid());
            }
            tx.execute("UPDATE saved_delete_tombstones SET sent_at = ?3 WHERE owner_id = ?1 AND message_id = ?2",
                params![owner.to_string(), id.to_string(), sent_at as i64])?;
            let count: i64 = tx.query_row(
                "SELECT COUNT(*) FROM saved_delete_outbox WHERE owner_id = ?1",
                params![owner.to_string()],
                |r| r.get(0),
            )?;
            let pending: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM saved_delete_outbox WHERE owner_id = ?1 AND message_id = ?2)",
                params![owner.to_string(), id.to_string()], |r| r.get(0),
            )?;
            if count >= 4096 && !pending {
                return Err(StorageError::InvalidData(
                    "Saved deletion outbox is full; retry after linked devices reconnect".into(),
                ));
            }
            tx.execute(
                "INSERT INTO saved_delete_outbox(owner_id, message_id, sent_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(owner_id, message_id) DO UPDATE SET generation = generation + 1,
                   attempts = 0, next_attempt_at = 0 WHERE sent_at = excluded.sent_at",
                params![owner.to_string(), id.to_string(), sent_at as i64],
            )?;
        } else {
            queue_message_attachments(&tx, id)?;
            tx.execute(
                "DELETE FROM messages WHERE id = ?1 AND conversation_type = 'saved_messages'
                 AND conversation_id = ?2 AND sender_id = ?2 AND sent_at = ?3",
                params![id.to_string(), owner.to_string(), sent_at as i64],
            )?;
        }
        tx.execute(
            "DELETE FROM saved_sync_outbox WHERE owner_id = ?1 AND message_id = ?2",
            params![owner.to_string(), id.to_string()],
        )?;
        if enqueue {
            crate::services::gateway::submission::check_send_session()?;
        }
        tx.commit()?;
        drop(conn);
        if !enqueue {
            if let Err(error) = super::super::attachment_repo::AttachmentRepository::retry_pending_cleanup(&self.db) {
                tracing::warn!(%error, "attachment cleanup will be retried");
            }
        }
        Ok(())
    }

    pub(crate) fn is_saved_tombstoned(
        &self,
        owner: &UserId,
        id: &MessageId,
    ) -> Result<bool, StorageError> {
        Ok(self.db.read_connection()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM saved_delete_tombstones WHERE owner_id = ?1 AND message_id = ?2)",
            params![owner.to_string(), id.to_string()], |r| r.get(0),
        )?)
    }

    /// Hard-delete one message and tombstone its id, so a server re-fetch of
    /// the same message (channel catch-up sync) cannot store it again. The
    /// tombstone is written even when the row is absent: a delete that races
    /// ahead of the message itself must still win.
    pub fn delete(&self, id: &MessageId) -> Result<(), StorageError> {
        self.delete_admitted(id, None).map(|_| ())
    }

    pub(crate) fn delete_received(&self, id: &MessageId, channel: ChannelId, session: &crate::state::receive_session::ReceiveSession) -> Result<Option<Message>, StorageError> {
        self.delete_admitted(id, Some((channel, session)))
    }

    fn delete_admitted(&self, id: &MessageId, receive: Option<(ChannelId, &crate::state::receive_session::ReceiveSession)>) -> Result<Option<Message>, StorageError> {
        let conn = self.db.connection()?;
        let tx = conn.unchecked_transaction()?;
        let existing = if let Some((channel, session)) = receive {
            session.check_owner(&tx)?;
            let existing = self.get_with_connection(&tx, id)?;
            if existing.as_ref().is_some_and(|row| row.conversation_id != channel) {
                return Err(StorageError::InvalidData("delete channel mismatch".into()));
            }
            existing
        } else { None };
        queue_message_attachments(&tx, id)?;
        tx.execute(
            "INSERT OR IGNORE INTO deleted_messages (id) VALUES (?1)",
            params![id.to_string()],
        )?;
        tombstone_dm_receipt(&tx, id)?;
        let rows = tx.execute(
            "DELETE FROM messages WHERE id = ?1",
            params![id.to_string()],
        )?;
        // An unseen target still needs its tombstone. Preserve NotFound for
        // callers while committing suppression of a delayed receive.
        if let Some((_, session)) = receive { session.check()?; }
        tx.commit()?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("Message {}", id)));
        }
        drop(conn);
        if let Err(error) = super::super::attachment_repo::AttachmentRepository::retry_pending_cleanup(&self.db) {
            tracing::warn!(%error, "attachment cleanup will be retried");
        }
        Ok(existing)
    }

    /// Clear retained reply snapshots before their disappearing parent is
    /// hard-deleted. The reply itself remains available in history.
    pub fn redact_expired_reply_quotes(&self, expired: &[Message]) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        for parent in expired {
            // Anonymous replies use a timestamp-derived id instead of the
            // parent's stored id. Keep the author and conversation in that
            // match so simultaneous messages in other chats stay untouched.
            let synthetic = MessageId::from_timestamp(parent.sent_at);
            conn.execute(
                "UPDATE messages SET
                    reply_to_text = NULL,
                    reply_to_thumbnail = NULL,
                    reply_to_thumbnail_mime = NULL,
                    content_json = CASE
                        WHEN json_valid(content_json)
                             AND json_type(content_json, '$.quote') = 'object'
                        THEN json_set(content_json, '$.quote.preview', '')
                        ELSE content_json END
                 WHERE reply_to = ?1
                    OR (reply_to = ?2 AND reply_to_author_id = ?3 AND conversation_id = ?4)",
                params![
                    parent.id.to_string(),
                    synthetic.to_string(),
                    parent.sender_id.map(|id| id.to_string()),
                    parent.conversation_id.to_string(),
                ],
            )?;
        }
        Ok(())
    }

    /// Hard-delete multiple messages by ID (purge — row + content removed,
    /// CASCADE clears dependents like `ephemeral_messages`/`unified_reactions`).
    ///
    /// Used by the disappearing-message reaper: a "disappeared" message must be
    /// truly GONE from disk, not merely tombstoned (a soft delete leaves the
    /// plaintext `content_json` in the DB and `list_for_channel` still returns
    /// the row). This mirrors `delete_message_local` (the app's own local
    /// delete), just batched — including the tombstones. Returns the number
    /// of rows removed.
    pub fn delete_batch(&self, ids: &[MessageId]) -> Result<usize, StorageError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.db.connection()?;
        let tx = conn.unchecked_transaction()?;
        for id in ids {
            queue_message_attachments(&tx, id)?;
            tx.execute(
                "INSERT OR IGNORE INTO deleted_messages (id) VALUES (?1)",
                params![id.to_string()],
            )?;
            tombstone_dm_receipt(&tx, id)?;
        }
        let placeholders: Vec<String> = (1..=ids.len()).map(|i| format!("?{}", i)).collect();
        let query = format!(
            "DELETE FROM messages WHERE id IN ({})",
            placeholders.join(", ")
        );
        let params_vec: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
        let rows = tx.execute(&query, rusqlite::params_from_iter(params_vec.iter()))?;
        tx.commit()?;
        drop(conn);
        if let Err(error) = super::super::attachment_repo::AttachmentRepository::retry_pending_cleanup(&self.db) {
            tracing::warn!(%error, "attachment cleanup will be retried");
        }
        Ok(rows)
    }

    /// Clear conversation-scoped state that is not a child of a message row.
    /// The membership ordering tombstones live elsewhere and must survive.
    pub fn purge_anon_group_orphans(
        &self,
        group: &ChannelId,
        ids: &[MessageId],
    ) -> Result<(), StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let group = group.to_string();
        for table in [
            "pending_scoped_reactions",
            "reaction_action_versions",
            "unified_reactions",
        ] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE conversation_type='anon_group' AND conversation_id=?1"),
                [&group],
            )?;
        }
        tx.execute(
            "DELETE FROM pending_message_modifiers WHERE conversation_id=?1",
            [&group],
        )?;
        tx.execute(
            "DELETE FROM pending_anon_group_messages WHERE group_id=?1",
            [&group],
        )?;
        tx.execute(
            "DELETE FROM settings WHERE key LIKE 'send.group.completion.v1:%'
             AND json_valid(value) AND json_extract(value,'$.held_group')=?1",
            [&group],
        )?;
        for id in ids {
            tx.execute(
                "DELETE FROM settings WHERE key LIKE 'send.group.completion.v1:%'
                 AND json_valid(value) AND json_extract(value,'$.message_id')=?1",
                [id.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Whether this message id was deleted locally (see `delete`). Ingest
    /// checks it so a re-fetched copy stays deleted.
    // tombstones are never pruned; one short row per deleted message.
    pub fn is_tombstoned(&self, id: &MessageId) -> Result<bool, StorageError> {
        let conn = self.db.read_connection()?;
        let found = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM deleted_messages WHERE id = ?1)",
            params![id.to_string()],
            |row| row.get::<_, bool>(0),
        )?;
        Ok(found)
    }

    /// Whether this account deleted its copy of the server DM `sender` sent
    /// at `sent_at` in `conversation` (`tombstone_dm_receipt`). Receive
    /// checks it so another copy of that message stays deleted.
    pub fn is_dm_receipt_tombstoned(
        &self,
        conversation: &ChannelId,
        sender: &UserId,
        sent_at: u64,
    ) -> Result<bool, StorageError> {
        Ok(self.db.read_connection()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM dm_receive_tombstones WHERE owner_id = (SELECT json_extract(value, '$') FROM settings WHERE key = ?1) AND conversation_id = ?2 AND sender_id = ?3 AND sent_at = ?4)",
            params![CURRENT_USER_KEY, conversation.to_string(), sender.to_string(), sent_at as i64],
            |r| r.get(0),
        )?)
    }

    /// Soft-delete multiple messages by ID.
    pub fn soft_delete_batch(&self, ids: &[MessageId]) -> Result<usize, StorageError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.db.connection()?;
        let now = Utc::now().to_rfc3339();
        let placeholders: Vec<String> = (1..=ids.len()).map(|i| format!("?{}", i)).collect();
        let query = format!(
            "UPDATE messages SET is_deleted = 1, updated_at = ?{} WHERE id IN ({})",
            ids.len() + 1,
            placeholders.join(", ")
        );
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = ids
            .iter()
            .map(|id| Box::new(id.to_string()) as Box<dyn rusqlite::ToSql>)
            .collect();
        params_vec.push(Box::new(now));
        let rows = conn.execute(
            &query,
            rusqlite::params_from_iter(params_vec.iter().map(|b| b.as_ref())),
        )?;
        Ok(rows)
    }

}
