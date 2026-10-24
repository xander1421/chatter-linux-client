use super::reactions::accept_reaction_action;
use super::*;

impl UnifiedMessageRepository {
    /// Insert the local echo of a message THIS device just sent.
    ///
    /// Same row as [`insert`](Self::insert) except it lands read. `is_read` is
    /// "the local user has seen this" (the peer's side of a conversation lives
    /// in `delivery_status`), and the unread badge is a `COUNT(is_read = 0)` —
    /// so an echo stored unread badges the conversation the user is typing in,
    /// and keeps it badged across restarts, since the badge is rebuilt from
    /// this table.
    pub fn insert_outgoing(&self, message: &Message) -> Result<(), StorageError> {
        let mut sent = message.clone();
        sent.is_read = true;
        self.insert(&sent)
    }

    /// Read the row and its durable pin clock from one consistent DB snapshot.
    pub(crate) fn saved_sync_snapshot_row(
        &self,
        owner: &UserId,
        id: &MessageId,
    ) -> Result<Option<(Message, u64)>, StorageError> {
        let conn = self.db.read_connection()?;
        let tx = conn.unchecked_transaction()?;
        let row = tx.query_row(
            &format!("SELECT {} FROM messages WHERE id = ?1 AND sender_id = ?2 AND conversation_id = ?2 AND conversation_type = 'saved_messages' AND is_deleted = 0 AND NOT EXISTS (SELECT 1 FROM deleted_messages WHERE id = ?1) AND NOT EXISTS (SELECT 1 FROM saved_delete_tombstones WHERE owner_id = ?2 AND message_id = ?1)", message_columns("messages")),
            params![id.to_string(), owner.to_string()], row_to_message,
        ).optional()?.transpose()?;
        let clock: Option<i64> = tx
            .query_row(
                "SELECT changed_at FROM saved_pin_watermarks WHERE message_id = ?1",
                params![id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        tx.commit()?;
        Ok(row.map(|row| (row, clock.unwrap_or_default() as u64)))
    }

    /// Store a local Saved creation and its sync obligation in one transaction.
    pub(crate) fn insert_saved_outgoing(&self, message: &Message) -> Result<(), StorageError> {
        let mut sent = message.clone();
        sent.is_read = true;
        self.store(&sent, false, None, true).map(|_| ())
    }

    /// Insert a new message.
    ///
    /// An edit, delete-for-everyone, reaction or pin that arrived first is
    /// applied here (`message_modifier_repo::take_on_arrival`); a message
    /// deleted before it arrived is tombstoned and not stored. A caller that
    /// announces the message uses [`insert_received`](Self::insert_received).
    pub fn insert(&self, message: &Message) -> Result<(), StorageError> {
        self.store(message, false, None, false).map(|_| ())
    }

    /// [`insert`](Self::insert), returning the message as stored — with any
    /// early modifier applied — or `None` when an early delete-for-everyone
    /// kept it out. What a receive path must announce, not what it decoded.
    pub fn insert_received(&self, message: &Message) -> Result<Option<Message>, StorageError> {
        if !self.store(message, false, None, false)? {
            return Ok(None);
        }
        self.get(&message.id)
    }

    /// Insert a message, ignoring duplicates (Signal dedup on primary key + sender_id/sent_at).
    ///
    /// Returns `true` if inserted, `false` if skipped.
    /// `false` too for a message its author deleted for everyone before it
    /// arrived (see [`insert`](Self::insert)).
    pub fn insert_or_ignore(&self, message: &Message) -> Result<bool, StorageError> {
        self.store(message, true, None, false)
    }

    /// [`insert_or_ignore`](Self::insert_or_ignore), returning the message as
    /// stored when it was inserted (see [`insert_received`](Self::insert_received)).
    pub fn insert_or_ignore_received(
        &self,
        message: &Message,
    ) -> Result<Option<Message>, StorageError> {
        if !self.store(message, true, None, false)? {
            return Ok(None);
        }
        self.get(&message.id)
    }

    /// Store an own Saved snapshot and its initial pin clock atomically.
    pub(crate) fn insert_saved_snapshot(
        &self,
        message: &Message,
        pin_changed_at: u64,
    ) -> Result<bool, StorageError> {
        use crate::services::current_user::CurrentUserProvider;
        let own = super::super::settings_repo::SettingsRepository::new(self.db.clone())
            .current_user_id()
            .map_err(|e| StorageError::InvalidData(format!("Saved identity: {e}")))?;
        if message.conversation_type != ConversationType::SavedMessages
            || message.conversation_id != ChannelId::from(own)
            || message.sender_id != Some(own)
            || pin_changed_at > i64::MAX as u64
            || (message.is_pinned && pin_changed_at == 0)
        {
            return Err(StorageError::InvalidData(
                "Invalid own Saved snapshot".into(),
            ));
        }
        self.store(message, false, Some(pin_changed_at), false)
    }

    /// The one write behind every insert: `true` when a row was written.
    fn store(
        &self,
        message: &Message,
        or_ignore: bool,
        saved_pin_clock: Option<u64>,
        local_saved: bool,
    ) -> Result<bool, StorageError> {
        let own_server_message = matches!(
            message.conversation_type,
            ConversationType::ServerDm | ConversationType::Channel
        ) && !matches!(
            message.content,
            MessageContent::System(_) | MessageContent::Call(_)
        ) && message.sender_id.is_some_and(|sender| {
            use crate::services::current_user::CurrentUserProvider;
            super::super::settings_repo::SettingsRepository::new(self.db.clone())
                .current_user_id()
                .is_ok_and(|me| me == sender)
        });
        let is_read = self.stored_is_read(message);
        let conn = self.db.connection()?;
        let tx = conn.unchecked_transaction()?;
        let is_read = if !is_read
            && matches!(
                message.conversation_type,
                ConversationType::ServerDm | ConversationType::Channel
            ) {
            let horizon: Option<i64> = tx
                .query_row(
                    "SELECT sent_at FROM conversation_read_horizons
                     WHERE conversation_type = ?1 AND conversation_id = ?2",
                    params![
                        message.conversation_type.as_str(),
                        message.conversation_id.to_string()
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            horizon.is_some_and(|sent_at| message.sent_at <= sent_at as u64)
        } else {
            is_read
        };
        if (saved_pin_clock.is_some() || local_saved)
            && tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM deleted_messages WHERE id = ?1)",
                params![message.id.to_string()],
                |r| r.get::<_, bool>(0),
            )?
        {
            return Ok(false);
        }
        // Check under the same write transaction as insertion: a pre-arrival
        // delete must win even if the caller's earlier read saw no tombstone.
        if message.conversation_type == ConversationType::SavedMessages
            && tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM saved_delete_tombstones
                 WHERE owner_id = ?1 AND message_id = ?2)",
                params![message.conversation_id.to_string(), message.id.to_string()],
                |r| r.get::<_, bool>(0),
            )?
        {
            return Ok(false);
        }
        let initial_saved_pin = saved_pin_clock.map(|_| message.is_pinned);
        let (mut message, reactions) =
            match super::super::message_modifier_repo::take_on_arrival(&tx, message)? {
                super::super::message_modifier_repo::Arrival::Store { message, reactions } => {
                    (message, reactions)
                }
                super::super::message_modifier_repo::Arrival::CallDeleted => {
                    tx.commit()?;
                    return Ok(false);
                }
                super::super::message_modifier_repo::Arrival::Deleted => {
                    tx.execute(
                        "INSERT OR IGNORE INTO deleted_messages (id) VALUES (?1)",
                        params![message.id.to_string()],
                    )?;
                    tx.commit()?;
                    return Ok(false);
                }
            };
        if let Some(pinned) = initial_saved_pin {
            // Saved snapshots have their own durable clock, rather than the
            // ordinary pin modifier's receipt-time ordering.
            message.to_mut().is_pinned = pinned;
        }
        let message = &*message;
        let content_json = serde_json::to_string(&message.content)
            .map_err(|e| StorageError::InvalidData(format!("serialize content: {}", e)))?;
        let attachments_blob = db_serialize(&message.attachments)?;
        let reaction_counts_blob = db_serialize(&message.reaction_counts)?;
        let current_user_reactions_blob = db_serialize(&message.current_user_reactions)?;
        let mentioned_integrations_blob = db_serialize(&message.mentioned_integrations)?;

        let rows = tx.execute(
            &format!(
                "INSERT {} INTO messages (
                id, conversation_id, conversation_type, sender_id, device_id,
                content_type, content_json, encrypted_content,
                thread_id, reply_to, attachments_blob,
                is_pinned, is_edited, is_deleted, is_read,
                sent_at, created_at, updated_at, edited_at,
                reaction_counts_blob, current_user_reactions_blob,
                mentioned_integrations_blob, targets_ai_bot, delivery_status,
                reply_to_author_id, reply_to_text,
                reply_to_thumbnail, reply_to_thumbnail_mime, failure_reason
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19,
                ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29
            )",
                if or_ignore { "OR IGNORE" } else { "" }
            ),
            params![
                message.id.to_string(),
                message.conversation_id.to_string(),
                message.conversation_type.as_str(),
                message.sender_id.map(|id| id.to_string()),
                message.device_id.map(|d| d as i64),
                content_type_discriminant(&message.content),
                content_json,
                message.encrypted_content.as_deref(),
                message.thread_id.map(|id| id.to_string()),
                message.reply_to.map(|id| id.to_string()),
                attachments_blob,
                message.is_pinned as i32,
                message.is_edited as i32,
                message.is_deleted as i32,
                is_read as i32,
                message.sent_at as i64,
                message.created_at.to_rfc3339(),
                message.updated_at.to_rfc3339(),
                message.edited_at.map(|dt| dt.to_rfc3339()),
                reaction_counts_blob,
                current_user_reactions_blob,
                mentioned_integrations_blob,
                message.targets_ai_bot as i32,
                &message.delivery_status,
                message.reply_to_author_id.map(|id| id.to_string()),
                message.reply_to_text.as_deref(),
                message.reply_to_thumbnail.as_deref(),
                message.reply_to_thumbnail_mime.as_deref(),
                message.failure_reason.as_deref(),
            ],
        )?;
        if rows > 0 {
            // A transcript of our own send is proof that every earlier incoming
            // message was read on that device, even if this device receives the
            // transcript before those incoming messages.
            if own_server_message {
                Self::advance_read_horizon(
                    &tx,
                    &message.conversation_id,
                    &message.conversation_type,
                    message.sent_at,
                )?;
            }
            // Includes a held pin applied by take_on_arrival: timestamp when
            // this client first observes it, not an invented remote pin clock.
            if message.is_pinned {
                tx.execute(
                    "UPDATE messages SET pinned_at=?1 WHERE id=?2",
                    params![
                        saved_pin_clock
                            .map(|clock| clock as i64)
                            .unwrap_or_else(|| Utc::now()
                                .timestamp_nanos_opt()
                                .unwrap_or_default()),
                        message.id.to_string()
                    ],
                )?;
            }
            if let Some(clock) = saved_pin_clock {
                tx.execute(
                    "INSERT INTO saved_pin_watermarks (message_id, changed_at) VALUES (?1, ?2)
                     ON CONFLICT(message_id) DO UPDATE SET changed_at = excluded.changed_at",
                    params![message.id.to_string(), clock as i64],
                )?;
            }
            if local_saved {
                let owner = message
                    .sender_id
                    .ok_or_else(|| StorageError::InvalidData("Saved sender missing".into()))?;
                if message.conversation_type != ConversationType::SavedMessages
                    || message.conversation_id != ChannelId::from(owner)
                {
                    return Err(StorageError::InvalidData("Invalid own Saved row".into()));
                }
                super::super::saved_sync_repo::SavedSyncRepository::mark_dirty_with(
                    &tx,
                    &owner,
                    &message.id,
                )?;
            }
            index_shared_content(&tx, message);
            self.apply_held_reactions(&tx, message, &reactions)?;
        }
        tx.commit()?;
        if rows > 0 {
            self.db.announce_message_store(message);
        }
        Ok(rows > 0)
    }

    /// Record the reactions that waited for `message` (now stored).
    pub(super) fn apply_held_reactions(
        &self,
        conn: &rusqlite::Connection,
        message: &Message,
        reactions: &[super::super::message_modifier_repo::PendingReaction],
    ) -> Result<bool, StorageError> {
        let (Some(author), false) = (message.sender_id, reactions.is_empty()) else {
            return Ok(false);
        };
        let own: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key='auth.current_user_id'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let me: UserId = own
            .and_then(|v| serde_json::from_str::<String>(&v).ok())
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| {
                StorageError::InvalidData("Held reaction requires current-user identity".into())
            })?;
        let count_votes = || {
            conn.query_row("SELECT COUNT(*) FROM unified_reactions WHERE conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at_ns=?4",params![message.conversation_id.to_string(),message.conversation_type.as_str(),author.to_string(),message.sent_at as i64],|r|r.get::<_,i64>(0))
        };
        let before = count_votes()?;
        let mut changed = false;
        for r in reactions {
            if !accept_reaction_action(
                conn,
                &ReactionScope::of(message),
                &author,
                message.sent_at,
                &r.reactor,
                &r.emoji,
                r.action_ts,
                r.remove,
            )? {
                continue;
            }
            if r.remove {
                changed |= conn.execute(
                    "DELETE FROM unified_reactions WHERE sender_id = ?1 AND sent_at_ns = ?2 AND user_id = ?3 AND emoji = ?4 AND conversation_id=?5 AND conversation_type=?6",
                    params![author.to_string(), message.sent_at as i64, r.reactor.to_string(), r.emoji, message.conversation_id.to_string(), message.conversation_type.as_str()],
                )?>0;
            } else {
                changed |= conn.execute(
                    "INSERT OR IGNORE INTO unified_reactions (id, sender_id, sent_at_ns, user_id, emoji, created_at,conversation_id,conversation_type)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6,?7,?8)",
                    params![
                        ReactionId::new().to_string(),
                        author.to_string(),
                        message.sent_at as i64,
                        r.reactor.to_string(),
                        r.emoji,
                        Utc::now().to_rfc3339(),
                        message.conversation_id.to_string(), message.conversation_type.as_str(),
                    ],
                )?>0;
            }
        }
        self.rebuild_reaction_blobs_with_conn(
            conn,
            &ReactionScope::of(message),
            &author,
            message.sent_at,
            &me,
        )?;
        Ok(changed || count_votes()? != before)
    }

    /// Get a message by ID.
    pub fn get(&self, id: &MessageId) -> Result<Option<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        self.get_with_connection(&conn, id)
    }

    pub(in crate::storage::repositories) fn get_with_connection(
        &self,
        conn: &rusqlite::Connection,
        id: &MessageId,
    ) -> Result<Option<Message>, StorageError> {
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM messages WHERE id = ?1",
            message_columns("messages")
        ))?;
        match stmt.query_row(params![id.to_string()], row_to_message) {
            Ok(msg) => Ok(Some(msg?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Hydrate a bounded search result page with one database read.
    pub fn get_many(&self, ids: &[MessageId]) -> Result<Vec<Message>, StorageError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.db.read_connection()?;
        let placeholders = vec!["?"; ids.len()].join(",");
        let sql = format!(
            "SELECT {} FROM messages WHERE id IN ({placeholders})",
            message_columns("messages")
        );
        let mut stmt = conn.prepare(&sql)?;
        let values = ids.iter().map(ToString::to_string).collect::<Vec<_>>();
        let mut rows = stmt.query(rusqlite::params_from_iter(values.iter()))?;
        collect_messages(&mut rows)
    }

    pub fn update_reply_parent(
        &self,
        id: &MessageId,
        parent: &MessageId,
    ) -> Result<(), StorageError> {
        self.db.connection()?.execute(
            "UPDATE messages SET reply_to = ?1 WHERE id = ?2",
            params![parent.to_string(), id.to_string()],
        )?;
        Ok(())
    }

    /// Resolve a stored quote reference, including a timestamp-derived reference
    /// written before its parent arrived. Scope the lookup to author and conversation.
    pub fn resolve_reply_parent(
        &self,
        message: &Message,
    ) -> Result<Option<MessageId>, StorageError> {
        let (Some(parent), Some(author)) = (message.reply_to, message.reply_to_author_id) else {
            return Ok(message.reply_to);
        };
        if message.conversation_type.is_anon() || self.get(&parent)?.is_some() {
            return Ok(Some(parent));
        }
        let conn = self.db.read_connection()?;
        resolve_timestamp_quote(
            &conn,
            &parent,
            &author,
            &message.conversation_id,
            &message.conversation_type,
        )
    }

    /// Get a message by (sender_id, sent_at) — Signal-style dedup key.
    pub fn get_by_sender_timestamp(
        &self,
        sender_id: &UserId,
        sent_at: u64,
    ) -> Result<Option<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM messages WHERE sender_id = ?1 AND sent_at = ?2",
            message_columns("messages")
        ))?;
        match stmt.query_row(
            params![sender_id.to_string(), sent_at as i64],
            row_to_message,
        ) {
            Ok(msg) => Ok(Some(msg?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// List messages for a conversation with cursor pagination (newest first).
    ///
    /// Pass `before_sent_at` for cursor-based paging.
    pub fn list(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
        before_sent_at: Option<u64>,
        limit: u32,
    ) -> Result<Vec<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        if let Some(before) = before_sent_at {
            let mut stmt = conn.prepare(&format!(
                "SELECT {} FROM messages \
                 WHERE conversation_id = ?1 AND conversation_type = ?2 \
                   AND sent_at < ?3 AND is_deleted = 0 \
                 ORDER BY sent_at DESC, sender_id DESC \
                 LIMIT ?4",
                message_columns("messages")
            ))?;
            let mut rows = stmt.query(params![
                conversation_id.to_string(),
                conversation_type.as_str(),
                before as i64,
                limit,
            ])?;
            collect_messages(&mut rows)
        } else {
            let mut stmt = conn.prepare(&format!(
                "SELECT {} FROM messages \
                 WHERE conversation_id = ?1 AND conversation_type = ?2 AND is_deleted = 0 \
                 ORDER BY sent_at DESC, sender_id DESC \
                 LIMIT ?3",
                message_columns("messages")
            ))?;
            let mut rows = stmt.query(params![
                conversation_id.to_string(),
                conversation_type.as_str(),
                limit,
            ])?;
            collect_messages(&mut rows)
        }
    }

    /// Get the latest message in a conversation (for preview in conversation list).
    pub fn get_latest(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<Option<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM messages \
             WHERE conversation_id = ?1 AND conversation_type = ?2 AND is_deleted = 0 \
             ORDER BY sent_at DESC",
            message_columns("messages")
        ))?;
        let mut rows = stmt.query(params![
            conversation_id.to_string(),
            conversation_type.as_str()
        ])?;
        // A preview is a list query, not a lookup of a particular row. Walk
        // past malformed newest rows so a sidebar entry still has a preview.
        while let Some(row) = rows.next()? {
            match row_to_message(row)
                .map_err(StorageError::Database)
                .and_then(|message| message)
            {
                Ok(message) => return Ok(Some(message)),
                Err(error) => super::super::repo_utils::log_invalid_row(row, "message", &error),
            }
        }
        Ok(None)
    }

    /// Update an existing message (full replace of mutable fields).
    ///
    /// Reply context (`reply_to*`) is the exception: it is fixed when the
    /// message is written and only ever filled in, never cleared. Server-proto
    /// upserts (channel backfill, thread/pin/saved fetches) rebuild the
    /// message without it, and replacing it here wiped the quote from a
    /// stored reply after restart (OPEN-ITEMS §112l).
    ///
    /// The reaction projections (`reaction_counts_blob`,
    /// `current_user_reactions_blob`) are not written here either: they are a
    /// cache of `unified_reactions`, rebuilt only by the reaction writers
    /// (`rebuild_reaction_blobs_with_conn`). Writing them from `message` put
    /// back whatever the caller held — a row read before a reaction landed,
    /// or a server proto with no reactions at all — and that drift then
    /// survived every reload (OPEN-ITEMS §112w).
    pub fn update(&self, message: &Message) -> Result<(), StorageError> {
        let is_read = self.stored_is_read(message);
        let conn = self.db.connection()?;
        let content_json = serde_json::to_string(&message.content)
            .map_err(|e| StorageError::InvalidData(format!("serialize content: {}", e)))?;
        let attachments_blob = db_serialize(&message.attachments)?;
        let mentioned_integrations_blob = db_serialize(&message.mentioned_integrations)?;

        let rows = conn.execute(
            "UPDATE messages SET
                content_type = ?1, content_json = ?2, encrypted_content = ?3,
                thread_id = ?4, reply_to = COALESCE(?5, reply_to), attachments_blob = ?6,
                pinned_at = CASE WHEN ?7 = 0 THEN NULL WHEN is_pinned = 0 THEN ?22 ELSE pinned_at END,
                is_pinned = ?7, is_edited = ?8, is_deleted = ?9, is_read = ?10,
                updated_at = ?11, edited_at = ?12,
                mentioned_integrations_blob = ?13, targets_ai_bot = ?14,
                delivery_status = ?15,
                reply_to_author_id = COALESCE(?16, reply_to_author_id),
                reply_to_text = COALESCE(?17, reply_to_text),
                reply_to_thumbnail = COALESCE(?18, reply_to_thumbnail),
                reply_to_thumbnail_mime = COALESCE(?19, reply_to_thumbnail_mime),
                failure_reason = ?20
             WHERE id = ?21",
            params![
                content_type_discriminant(&message.content),
                content_json,
                message.encrypted_content.as_deref(),
                message.thread_id.map(|id| id.to_string()),
                message.reply_to.map(|id| id.to_string()),
                attachments_blob,
                message.is_pinned as i32,
                message.is_edited as i32,
                message.is_deleted as i32,
                is_read as i32,
                message.updated_at.to_rfc3339(),
                message.edited_at.map(|dt| dt.to_rfc3339()),
                mentioned_integrations_blob,
                message.targets_ai_bot as i32,
                &message.delivery_status,
                message.reply_to_author_id.map(|id| id.to_string()),
                message.reply_to_text.as_deref(),
                message.reply_to_thumbnail.as_deref(),
                message.reply_to_thumbnail_mime.as_deref(),
                message.failure_reason.as_deref(),
                message.id.to_string(),
                Utc::now().timestamp_nanos_opt().unwrap_or_default(),
            ],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("Message {}", message.id)));
        }
        if is_read {
            super::super::ephemeral_repo::start_read_countdowns(&conn)?;
        }
        Ok(())
    }
}
