//! Unified message repository.
//!
//! Single repository for all message types (channel, DM, anon DM, anon group,
//! saved messages). All queries use the `conversation_type` discriminant.
//!
//! Also owns reaction operations via the `unified_reactions` table, keeping
//! `reaction_counts_blob` and `current_user_reactions_blob` on message rows
//! in sync.

use crate::domain::ids::{AttachmentId, ChannelId, IntegrationId, MessageId, ReactionId, UserId};
use crate::domain::message::{
    ConversationType, Message, MessageContent, ReactionCount, Thread, UnreadAnchor,
};
use crate::domain::reaction::{Emoji, Reaction, ReactionScope};
use crate::error::StorageError;
use crate::storage::Database;
use crate::storage::serialization::{db_deserialize, db_serialize};
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use std::collections::HashMap;
use std::sync::Arc;

/// Unified message repository for all conversation types.
///
/// Replaces the three separate message repos (`message_repo`, `anon_dm_repo` messages,
/// `anon_group_repo` messages) with a single implementation that uses the
/// `conversation_type` discriminant.
pub struct UnifiedMessageRepository {
    db: Arc<Database>,
}

/// Visible projection changes and exact outgoing completion are separate contracts.
pub(crate) struct ReactionWriteOutcome {
    pub changed: bool,
    pub completed: bool,
}

/// Add a freshly-inserted message's attachments and links to the
/// shared-content index (the profile screen's media drawer).
///
/// Best-effort: a failure here costs one tile in a media tab, and must never
/// take the message insert down with it.
fn index_shared_content(conn: &rusqlite::Connection, message: &Message) {
    if let Err(e) = super::shared_content_repo::index_message(conn, message) {
        tracing::warn!(message_id = %message.id, error = %e, "shared-content index failed");
    }
}

#[path = "channel_message_clocks.rs"]
mod channel_message_clocks;

#[path = "reaction_clock.rs"]
mod reaction_clock;

#[path = "reaction_normalization.rs"]
pub(crate) mod reaction_normalization;

#[cfg(test)]
#[path = "reaction_storage_regressions.rs"]
mod reaction_storage_regressions;

impl UnifiedMessageRepository {
    /// Create a new UnifiedMessageRepository.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// `is_read` as written. The local user's own messages — a send from
    /// another of their devices, a SyncSent copy, a backfill, a group message
    /// healed after its SKDM — are never unread to them. Every writer routes
    /// through `insert` / `insert_or_ignore` / `update`, so the rule lives here
    /// once instead of in each receive path (F41 put it in one, and the rest
    /// still badged the user's own messages).
    ///
    /// Call records and system notices are never unread either: they are
    /// history, not messages, and badging them lit the Chats tab for a
    /// conversation that showed nothing new (Signal counts neither).
    fn stored_is_read(&self, message: &Message) -> bool {
        use crate::services::current_user::CurrentUserProvider;
        message.is_read
            || matches!(
                message.content,
                MessageContent::System(_) | MessageContent::Call(_)
            )
            || message.sender_id.is_some_and(|sender| {
                super::settings_repo::SettingsRepository::new(self.db.clone())
                    .current_user_id()
                    .is_ok_and(|me| me == sender)
            })
    }

    // ========================================================================
    // CRUD
    // ========================================================================

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

    /// Insert a new message.
    ///
    /// An edit, delete-for-everyone, reaction or pin that arrived first is
    /// applied here (`message_modifier_repo::take_on_arrival`); a message
    /// deleted before it arrived is tombstoned and not stored. A caller that
    /// announces the message uses [`insert_received`](Self::insert_received).
    pub fn insert(&self, message: &Message) -> Result<(), StorageError> {
        self.store(message, false).map(|_| ())
    }

    /// [`insert`](Self::insert), returning the message as stored — with any
    /// early modifier applied — or `None` when an early delete-for-everyone
    /// kept it out. What a receive path must announce, not what it decoded.
    pub fn insert_received(&self, message: &Message) -> Result<Option<Message>, StorageError> {
        if !self.store(message, false)? {
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
        self.store(message, true)
    }

    /// [`insert_or_ignore`](Self::insert_or_ignore), returning the message as
    /// stored when it was inserted (see [`insert_received`](Self::insert_received)).
    pub fn insert_or_ignore_received(
        &self,
        message: &Message,
    ) -> Result<Option<Message>, StorageError> {
        if !self.store(message, true)? {
            return Ok(None);
        }
        self.get(&message.id)
    }

    /// The one write behind every insert: `true` when a row was written.
    fn store(&self, message: &Message, or_ignore: bool) -> Result<bool, StorageError> {
        let is_read = self.stored_is_read(message);
        let conn = self.db.connection()?;
        let tx = conn.unchecked_transaction()?;
        let (message, reactions) =
            match super::message_modifier_repo::take_on_arrival(&tx, message)? {
                super::message_modifier_repo::Arrival::Store { message, reactions } => {
                    (message, reactions)
                }
                super::message_modifier_repo::Arrival::CallDeleted => {
                    tx.commit()?;
                    return Ok(false);
                }
                super::message_modifier_repo::Arrival::Deleted => {
                    tx.execute(
                        "INSERT OR IGNORE INTO deleted_messages (id) VALUES (?1)",
                        params![message.id.to_string()],
                    )?;
                    tx.commit()?;
                    return Ok(false);
                }
            };
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
            // Includes a held pin applied by take_on_arrival: timestamp when
            // this client first observes it, not an invented remote pin clock.
            if message.is_pinned {
                tx.execute(
                    "UPDATE messages SET pinned_at=?1 WHERE id=?2",
                    params![
                        Utc::now().timestamp_nanos_opt().unwrap_or_default(),
                        message.id.to_string()
                    ],
                )?;
            }
            index_shared_content(&tx, message);
            self.apply_held_reactions(&tx, message, &reactions)?;
        }
        tx.commit()?;
        Ok(rows > 0)
    }

    /// Record the reactions that waited for `message` (now stored).
    fn apply_held_reactions(
        &self,
        conn: &rusqlite::Connection,
        message: &Message,
        reactions: &[super::message_modifier_repo::PendingReaction],
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
        Self::ensure_reaction_scope_resolved_with_conn(
            conn,
            &ReactionScope::of(message),
            &author,
            message.sent_at,
        )?;
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

    pub(super) fn get_with_connection(
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

    /// Store the sealed-sender deletion token of a message we sent.
    pub fn set_deletion_token(&self, id: &MessageId, token: &[u8; 32]) -> Result<(), StorageError> {
        let rows = self.db.connection()?.execute(
            "UPDATE messages SET deletion_token = ?1 WHERE id = ?2",
            params![&token[..], id.to_string()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("Message {}", id)));
        }
        Ok(())
    }

    /// The sealed-sender deletion token of a message we sent, if it has one.
    pub fn deletion_token(&self, id: &MessageId) -> Result<Option<[u8; 32]>, StorageError> {
        let conn = self.db.connection()?;
        let token: Option<Vec<u8>> = match conn.query_row(
            "SELECT deletion_token FROM messages WHERE id = ?1",
            params![id.to_string()],
            |row| row.get(0),
        ) {
            Ok(t) => t,
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => return Err(StorageError::Database(e)),
        };
        Ok(token.and_then(|t| <[u8; 32]>::try_from(t.as_slice()).ok()))
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
             ORDER BY sent_at DESC LIMIT 1",
            message_columns("messages")
        ))?;
        match stmt.query_row(
            params![conversation_id.to_string(), conversation_type.as_str()],
            row_to_message,
        ) {
            Ok(msg) => Ok(Some(msg?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Update an existing message (full replace of mutable fields).
    ///
    /// Reply context (`reply_to*`) is the exception: it is fixed when the
    /// message is written and only ever filled in, never cleared. Server-proto
    /// upserts (channel backfill, thread/pin/saved fetches) rebuild the
    /// message without it, and replacing it here wiped the quote from a
    /// stored reply after restart (OPEN-ITEMS §112l).
    pub fn update(&self, message: &Message) -> Result<(), StorageError> {
        let is_read = self.stored_is_read(message);
        let conn = self.db.connection()?;
        let content_json = serde_json::to_string(&message.content)
            .map_err(|e| StorageError::InvalidData(format!("serialize content: {}", e)))?;
        let attachments_blob = db_serialize(&message.attachments)?;
        let reaction_counts_blob = db_serialize(&message.reaction_counts)?;
        let current_user_reactions_blob = db_serialize(&message.current_user_reactions)?;
        let mentioned_integrations_blob = db_serialize(&message.mentioned_integrations)?;

        let rows = conn.execute(
            "UPDATE messages SET
                content_type = ?1, content_json = ?2, encrypted_content = ?3,
                thread_id = ?4, reply_to = COALESCE(?5, reply_to), attachments_blob = ?6,
                pinned_at = CASE WHEN ?7 = 0 THEN NULL WHEN is_pinned = 0 THEN ?24 ELSE pinned_at END,
                is_pinned = ?7, is_edited = ?8, is_deleted = ?9, is_read = ?10,
                updated_at = ?11, edited_at = ?12,
                reaction_counts_blob = CASE WHEN EXISTS(SELECT 1 FROM reaction_scope_unresolved r WHERE r.conversation_id=messages.conversation_id AND r.conversation_type=messages.conversation_type AND r.sender_id=messages.sender_id AND r.sent_at_ns=messages.sent_at) THEN reaction_counts_blob ELSE ?13 END,
                current_user_reactions_blob = CASE WHEN EXISTS(SELECT 1 FROM reaction_scope_unresolved r WHERE r.conversation_id=messages.conversation_id AND r.conversation_type=messages.conversation_type AND r.sender_id=messages.sender_id AND r.sent_at_ns=messages.sent_at) THEN current_user_reactions_blob ELSE ?14 END,
                mentioned_integrations_blob = ?15, targets_ai_bot = ?16,
                delivery_status = ?17,
                reply_to_author_id = COALESCE(?18, reply_to_author_id),
                reply_to_text = COALESCE(?19, reply_to_text),
                reply_to_thumbnail = COALESCE(?20, reply_to_thumbnail),
                reply_to_thumbnail_mime = COALESCE(?21, reply_to_thumbnail_mime),
                failure_reason = ?22
             WHERE id = ?23",
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
                message.id.to_string(),
                Utc::now().timestamp_nanos_opt().unwrap_or_default(),
            ],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("Message {}", message.id)));
        }
        if is_read {
            super::ephemeral_repo::start_read_countdowns(&conn)?;
        }
        Ok(())
    }

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
        let rows = stmt.query_map(params![conversation_type.as_str()], row_to_message)?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row??);
        }
        Ok(out)
    }

    pub fn delete_by_conversation_type(
        &self,
        conversation_type: &crate::domain::message::ConversationType,
    ) -> Result<usize, StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "DELETE FROM messages WHERE conversation_type = ?1",
            params![conversation_type.as_str()],
        )?;
        Ok(rows)
    }

    /// Hard-delete one message and tombstone its id, so a server re-fetch of
    /// the same message (channel catch-up sync) cannot store it again. The
    /// tombstone is written even when the row is absent: a delete that races
    /// ahead of the message itself must still win.
    pub fn delete(&self, id: &MessageId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT OR IGNORE INTO deleted_messages (id) VALUES (?1)",
            params![id.to_string()],
        )?;
        let rows = conn.execute(
            "DELETE FROM messages WHERE id = ?1",
            params![id.to_string()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("Message {}", id)));
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
        for id in ids {
            conn.execute(
                "INSERT OR IGNORE INTO deleted_messages (id) VALUES (?1)",
                params![id.to_string()],
            )?;
        }
        let placeholders: Vec<String> = (1..=ids.len()).map(|i| format!("?{}", i)).collect();
        let query = format!(
            "DELETE FROM messages WHERE id IN ({})",
            placeholders.join(", ")
        );
        let params_vec: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
        let rows = conn.execute(&query, rusqlite::params_from_iter(params_vec.iter()))?;
        Ok(rows)
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
        super::ephemeral_repo::start_read_countdowns(&conn)
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
        let flipped = conn
            .prepare(
                "UPDATE messages SET is_read = 1 WHERE conversation_id = ?1 AND conversation_type = ?2 AND is_read = 0
                 RETURNING sender_id, sent_at",
            )?
            .query_map(params![conversation_id.to_string(), conversation_type.as_str()], |row| {
                Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        super::ephemeral_repo::start_read_countdowns(&conn)?;
        let read = flipped
            .into_iter()
            .filter_map(|(sender, sent_at)| Some((sender?.parse().ok()?, sent_at as u64)))
            .collect();
        Ok((anchor, read))
    }

    /// The unread boundary of a conversation, without clearing it.
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
                   AND is_read = 0 AND is_deleted = 0 \
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

    // ========================================================================
    // Reactions (unified_reactions table)
    // ========================================================================

    /// Save only decoded authenticated reaction intent before acknowledging transport.
    #[allow(clippy::too_many_arguments)]
    pub fn hold_received_reaction(
        &self,
        scope: &ReactionScope,
        author: &UserId,
        target_ts: u64,
        actor: &UserId,
        emoji: &str,
        remove: bool,
        action_ts: u64,
    ) -> Result<(), StorageError> {
        super::message_modifier_repo::MessageModifierRepository::new(self.db.clone())
            .queue_reaction(
                author,
                target_ts,
                actor,
                emoji,
                remove,
                scope,
                action_ts,
                crate::util::time::TimestampNanos::now().as_u64(),
            )
    }

    /// Consume a durable handoff only after target arrival and roster authority.
    /// Unresolved legacy ownership keeps its intent without changing history.
    pub fn apply_held_reaction_scope(
        &self,
        scope: &ReactionScope,
        author: &UserId,
        target_ts: u64,
    ) -> Result<bool, StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let changed = self.apply_held_reaction_scope_with_conn(&tx, scope, author, target_ts)?;
        tx.commit()?;
        Ok(changed)
    }

    fn apply_held_reaction_scope_with_conn(
        &self,
        tx: &rusqlite::Connection,
        scope: &ReactionScope,
        author: &UserId,
        target_ts: u64,
    ) -> Result<bool, StorageError> {
        let retained_ts =
            Self::resolve_channel_reaction_clock_with_conn(tx, scope, author, target_ts)?;
        match Self::ensure_reaction_scope_resolved_with_conn(tx, scope, author, retained_ts) {
            Ok(()) => {}
            Err(StorageError::InvalidData(_)) => return Ok(false),
            Err(error) => return Err(error),
        }
        let target=tx.query_row(&format!("SELECT {} FROM messages WHERE conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at=?4 AND is_deleted=0",message_columns("messages")),params![scope.id.to_string(),scope.kind.as_str(),author.to_string(),retained_ts as i64],row_to_message).optional()?.transpose()?;
        let Some(target) = target else {
            return Ok(false);
        };
        let own: Option<String> = tx
            .query_row(
                "SELECT value FROM settings WHERE key='auth.current_user_id'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let own = own
            .map(|v| {
                serde_json::from_str::<String>(&v).map_err(|_| {
                    StorageError::InvalidData("Invalid reaction account identity".into())
                })
            })
            .transpose()?;
        let mut stmt=tx.prepare("SELECT actor_id,emoji,remove,action_ts FROM pending_scoped_reactions WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND target_sent_at=?4 UNION ALL SELECT actor_id,emoji,0,action_ts FROM pending_reaction_add_selections WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND target_sent_at=?4 ORDER BY action_ts,remove,emoji")?;
        let rows = stmt
            .query_map(
                params![
                    scope.id.to_string(),
                    scope.kind.as_str(),
                    author.to_string(),
                    target_ts as i64
                ],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, bool>(2)?,
                        r.get::<_, i64>(3)? as u64,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        let mut accepted = Vec::new();
        for (actor, emoji, remove, action_ts) in rows {
            if super::message_modifier_repo::may_modify(
                &tx,
                &target,
                &actor,
                own.as_deref() == Some(actor.as_str()),
            )? {
                let reactor = actor
                    .parse()
                    .map_err(|_| StorageError::InvalidData("Invalid held reaction actor".into()))?;
                tx.execute("DELETE FROM pending_scoped_reactions WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND target_sent_at=?4 AND actor_id=?5 AND emoji=?6",params![scope.id.to_string(),scope.kind.as_str(),author.to_string(),target_ts as i64,actor,emoji])?;
                tx.execute("DELETE FROM pending_reaction_add_selections WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND target_sent_at=?4 AND actor_id=?5",params![scope.id.to_string(),scope.kind.as_str(),author.to_string(),target_ts as i64,actor])?;
                accepted.push(super::message_modifier_repo::PendingReaction {
                    reactor,
                    emoji,
                    remove,
                    action_ts,
                });
            }
        }
        self.apply_held_reactions(tx, &target, &accepted)
    }

    /// Bind this profile and exact mailbox before staged Signal receive.
    pub(crate) fn reaction_receive_key(
        &self,
        transport: &str,
        scope: ChannelId,
        envelope: &str,
        timestamp: u64,
    ) -> Result<crate::storage::receive_artifact::ReceiveKey, StorageError> {
        let conn = self.db.read_connection()?;
        let (account, device) = reaction_receive_binding(&conn)?;
        let key = crate::storage::receive_artifact::ReceiveKey {
            local_account: account,
            local_device: device,
            transport: transport.into(),
            scope: scope.to_string(),
            envelope_id: envelope.into(),
            server_timestamp_ns: timestamp,
        };
        key.validate()?;
        Ok(key)
    }

    /// Transaction B: authenticated exact artifact, scoped intent/projections,
    /// and applied flag commit together. Replays never reapply an applied vote.
    pub(crate) fn apply_reaction_artifact(
        &self,
        artifact: &crate::storage::receive_artifact::ReceiveArtifact,
    ) -> Result<Option<(ReactionScope, UserId, u64)>, StorageError> {
        use crate::storage::receive_artifact;
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let (account, device) = reaction_receive_binding(&tx)?;
        if account != artifact.key.local_account || device != artifact.key.local_device {
            return Err(StorageError::InvalidData(
                "Reaction receive account changed".into(),
            ));
        }
        let stored =
            receive_artifact::load(&tx, &artifact.key, &artifact.digest)?.ok_or_else(|| {
                StorageError::InvalidData("Authenticated receive artifact missing".into())
            })?;
        let mut expected = artifact.clone();
        expected.applied = stored.applied;
        expected.acknowledged = stored.acknowledged;
        if stored.acknowledged {
            expected.content.clear();
        }
        if stored != expected {
            return Err(StorageError::InvalidData(
                "Authenticated receive artifact mismatch".into(),
            ));
        }
        if stored.applied {
            return Ok(None);
        }
        let mut scope = ReactionScope::new(
            stored
                .key
                .scope
                .parse()
                .map_err(|_| StorageError::InvalidData("Invalid receive scope".into()))?,
            match stored.key.transport.as_str() {
                "anon_dm" => ConversationType::AnonDm,
                "anon_group" => ConversationType::AnonGroup,
                _ => {
                    return Err(StorageError::InvalidData(
                        "Invalid receive transport".into(),
                    ));
                }
            },
        );
        // Invalid payload destination is authenticated rejected input: preserve
        // its journal evidence but never let it choose a foreign target.
        let intent = crate::inbox::reaction_handoff::decode_artifact_reaction(&stored, &scope);
        let mut applied = None;
        if let Some((author, reaction)) = intent {
            if reaction.target_author_aci.starts_with("channel:") {
                let encrypted: Option<bool> = tx
                    .query_row(
                        "SELECT require_encryption FROM channels WHERE id=?1",
                        [scope.id.to_string()],
                        |r| r.get(0),
                    )
                    .optional()?;
                let encrypted = encrypted.ok_or_else(|| {
                    StorageError::InvalidData(
                        "Reaction channel metadata not ready; authenticated artifact retained"
                            .into(),
                    )
                })?;
                let mut stmt=tx.prepare("SELECT DISTINCT m.conversation_type FROM messages m WHERE m.conversation_id=?1 AND m.sender_id=?2 AND (m.sent_at=?3 OR EXISTS(SELECT 1 FROM channel_message_clocks a WHERE a.message_id=m.id AND a.conversation_id=m.conversation_id AND a.conversation_type=m.conversation_type AND a.author_id=m.sender_id AND a.authenticated_sent_at=?3)) AND m.conversation_type IN ('channel','anon_group') AND m.is_deleted=0")?;
                let kinds = stmt
                    .query_map(
                        params![
                            scope.id.to_string(),
                            author.to_string(),
                            reaction.target_sent_timestamp as i64
                        ],
                        |r| r.get::<_, String>(0),
                    )?
                    .collect::<Result<Vec<_>, _>>()?;
                drop(stmt);
                if kinds.len() > 1 {
                    return Err(StorageError::InvalidData(
                        "Ambiguous channel receive storage kind".into(),
                    ));
                }
                scope.kind = if let Some(kind) = kinds.first() {
                    ConversationType::from_str_value(kind)?
                } else if encrypted {
                    ConversationType::AnonGroup
                } else {
                    ConversationType::Channel
                };
            }
            let actor: UserId = stored.sender.parse().map_err(|_| {
                StorageError::InvalidData("Invalid certified reaction sender".into())
            })?;
            let own = actor.to_string() == account;
            // Same oriented durable friendship block used by Gateway; this
            // artifact path bypasses its nonreaction router, not its block policy.
            let blocked:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM friendships WHERE user_id=?1 AND friend_id=?2 AND status=2)",params![account,actor.to_string()],|r|r.get(0))?;
            let allowed=!blocked && (own || match scope.kind {
                ConversationType::AnonDm=>tx.query_row("SELECT EXISTS(SELECT 1 FROM anon_dm_conversations WHERE id=?1 AND peer_id=?2 AND status=1 AND length(peer_mailbox_pk)=32) AND NOT EXISTS(SELECT 1 FROM anon_dm_blocked_users WHERE user_id=?2)",params![scope.id.to_string(),actor.to_string()],|r|r.get::<_,bool>(0))?,
                ConversationType::Channel=>tx.query_row("SELECT EXISTS(SELECT 1 FROM memberships WHERE channel_id=?1 AND user_id=?2)",params![scope.id.to_string(),actor.to_string()],|r|r.get::<_,bool>(0))?,
                ConversationType::AnonGroup=>tx.query_row("SELECT EXISTS(SELECT 1 FROM memberships WHERE channel_id=?1 AND user_id=?2) OR (EXISTS(SELECT 1 FROM anon_group_members g JOIN anon_groups a ON a.id=g.group_id WHERE g.group_id=?1 AND g.user_id=?2 AND a.status<>2) AND NOT EXISTS(SELECT 1 FROM anon_group_blocked_users WHERE group_id=?1 AND user_id=?2))",params![scope.id.to_string(),actor.to_string()],|r|r.get::<_,bool>(0))?,
                _=>false,
            });
            if !allowed && !blocked {
                let refused:bool=match scope.kind {
                    ConversationType::AnonDm=>tx.query_row("SELECT EXISTS(SELECT 1 FROM anon_dm_blocked_users WHERE user_id=?2) OR EXISTS(SELECT 1 FROM anon_dm_conversations WHERE id=?1 AND (status IN (2,3) OR peer_id<>?2))",params![scope.id.to_string(),actor.to_string()],|r|r.get(0))?,
                    ConversationType::AnonGroup=>tx.query_row("SELECT EXISTS(SELECT 1 FROM anon_group_blocked_users WHERE group_id=?1 AND user_id=?2) OR EXISTS(SELECT 1 FROM anon_groups WHERE id=?1 AND status=2)",params![scope.id.to_string(),actor.to_string()],|r|r.get(0))?,
                    _=>false,
                };
                if !refused {
                    return Err(StorageError::InvalidData(
                        "Reaction roster/readiness unavailable; authenticated artifact retained"
                            .into(),
                    ));
                }
            }
            if allowed {
                let action = if reaction.timestamp > 0 {
                    reaction.timestamp
                } else {
                    stored.key.server_timestamp_ns
                };
                super::message_modifier_repo::queue_reaction_with_conn(
                    &tx,
                    &author,
                    reaction.target_sent_timestamp,
                    &actor,
                    &reaction.emoji,
                    reaction.remove,
                    &scope,
                    action,
                    crate::util::time::TimestampNanos::now().as_u64(),
                )?;
                if self.apply_held_reaction_scope_with_conn(
                    &tx,
                    &scope,
                    &author,
                    reaction.target_sent_timestamp,
                )? {
                    applied = Some((scope, author, reaction.target_sent_timestamp));
                }
            }
        }
        receive_artifact::mark_applied(&tx, &stored)?;
        tx.commit()?;
        Ok(applied)
    }

    /// Called only for an exact envelope whose upstream acknowledgement succeeded.
    pub(crate) fn acknowledge_reaction_artifact(
        &self,
        key: &crate::storage::receive_artifact::ReceiveKey,
        digest: &[u8],
    ) -> Result<(), StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        crate::storage::receive_artifact::mark_acknowledged(&tx, key, digest)?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn pending_reaction_artifacts_after(
        &self,
        cursor: Option<&(u64, String)>,
    ) -> Result<Vec<crate::storage::receive_artifact::ReceiveArtifact>, StorageError> {
        use crate::storage::receive_artifact;
        let conn = self.db.read_connection()?;
        let (account, device) = reaction_receive_binding(&conn)?;
        let mut stmt=conn.prepare("SELECT id FROM reaction_receive_artifacts WHERE local_account=?1 AND local_device=?2 AND state='pending' AND (?3 IS NULL OR (server_timestamp_ns,id)>(?3,?4)) ORDER BY server_timestamp_ns,id LIMIT ?5")?;
        let ids = stmt
            .query_map(
                params![
                    account,
                    device,
                    cursor.map(|v| v.0 as i64),
                    cursor.map(|v| v.1.as_str()),
                    receive_artifact::MAX_RECOVERY_BATCH as i64
                ],
                |r| r.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let mut result = Vec::new();
        for id in ids {
            result.push(receive_artifact::load_by_id(&conn, &id)?.ok_or_else(|| {
                StorageError::InvalidData("Receive artifact vanished during recovery read".into())
            })?);
        }
        Ok(result)
    }

    /// Bounded restart-safe replay for handoffs whose target is already stored.
    pub fn replay_held_reactions(&self) -> Result<Vec<(ReactionScope, UserId, u64)>, StorageError> {
        let targets = {
            let conn = self.db.read_connection()?;
            let own: Option<String> = conn
                .query_row(
                    "SELECT value FROM settings WHERE key='auth.current_user_id'",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            let own = own
                .map(|v| {
                    serde_json::from_str::<String>(&v).map_err(|_| {
                        StorageError::InvalidData("Invalid reaction replay account".into())
                    })
                })
                .transpose()?;
            // Exact eligibility excludes unresolved/readiness heads before the
            // work limit. Such rows remain durable and qualify after metadata arrives.
            let mut stmt=conn.prepare("SELECT DISTINCT p.conversation_id,p.conversation_type,p.author_id,p.target_sent_at
                FROM pending_scoped_reactions p JOIN messages m ON m.conversation_id=p.conversation_id AND m.conversation_type=p.conversation_type AND m.sender_id=p.author_id AND m.sent_at=p.target_sent_at
                WHERE m.is_deleted=0 AND NOT EXISTS(SELECT 1 FROM reaction_scope_unresolved u WHERE u.conversation_id=p.conversation_id AND u.conversation_type=p.conversation_type AND u.sender_id=p.author_id AND u.sent_at_ns=p.target_sent_at)
                AND NOT EXISTS(SELECT 1 FROM friendships f WHERE f.user_id=?1 AND f.friend_id=p.actor_id AND f.status=2)
                AND (p.actor_id=?1
                    OR (p.conversation_type='server_dm' AND p.conversation_id=p.actor_id)
                    OR (p.conversation_type IN ('channel','anon_group') AND EXISTS(SELECT 1 FROM memberships v WHERE v.channel_id=p.conversation_id AND v.user_id=p.actor_id))
                    OR (p.conversation_type='anon_dm' AND EXISTS(SELECT 1 FROM anon_dm_conversations d WHERE d.id=p.conversation_id AND d.peer_id=p.actor_id AND d.status=1 AND length(d.peer_mailbox_pk)=32) AND NOT EXISTS(SELECT 1 FROM anon_dm_blocked_users b WHERE b.user_id=p.actor_id))
                    OR (p.conversation_type='anon_group' AND EXISTS(SELECT 1 FROM anon_group_members g JOIN anon_groups a ON a.id=g.group_id WHERE g.group_id=p.conversation_id AND g.user_id=p.actor_id AND a.status<>2) AND NOT EXISTS(SELECT 1 FROM anon_group_blocked_users b WHERE b.group_id=p.conversation_id AND b.user_id=p.actor_id)))
                ORDER BY p.conversation_type,p.conversation_id,p.author_id,p.target_sent_at LIMIT 1000")?;
            stmt.query_map(params![own], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        let mut applied = Vec::new();
        for (id, kind, author, ts) in targets {
            let scope = ReactionScope::new(
                id.parse().map_err(|_| {
                    StorageError::InvalidData("Invalid handoff conversation".into())
                })?,
                ConversationType::from_str_value(&kind)?,
            );
            let author = author
                .parse()
                .map_err(|_| StorageError::InvalidData("Invalid handoff author".into()))?;
            match self.apply_held_reaction_scope(&scope, &author, ts as u64) {
                Ok(true) => applied.push((scope, author, ts as u64)),
                Ok(false) => {}
                Err(_) => {
                    tracing::warn!("Independent pending reaction target awaits storage recovery")
                }
            }
        }
        Ok(applied)
    }

    pub fn get_by_reaction_scope(
        &self,
        scope: &ReactionScope,
        author: &UserId,
        sent_at: u64,
    ) -> Result<Option<Message>, StorageError> {
        let sent_at = self.resolve_channel_reaction_clock(scope, author, sent_at)?;
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(&format!("SELECT {} FROM messages WHERE conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at=?4 AND is_deleted=0", message_columns("messages")))?;
        let row = stmt
            .query_row(
                params![
                    scope.id.to_string(),
                    scope.kind.as_str(),
                    author.to_string(),
                    sent_at as i64
                ],
                row_to_message,
            )
            .optional()?;
        row.transpose()
    }

    pub fn ensure_reaction_scope_resolved(
        &self,
        scope: &ReactionScope,
        author: &UserId,
        sent_at: u64,
    ) -> Result<(), StorageError> {
        let sent_at = self.resolve_channel_reaction_clock(scope, author, sent_at)?;
        let conn = self.db.read_connection()?;
        Self::ensure_reaction_scope_resolved_with_conn(&conn, scope, author, sent_at)
    }
    fn ensure_reaction_scope_resolved_with_conn(
        conn: &rusqlite::Connection,
        scope: &ReactionScope,
        author: &UserId,
        sent_at: u64,
    ) -> Result<(), StorageError> {
        let unresolved:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM reaction_scope_unresolved WHERE conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at_ns=?4)",params![scope.id.to_string(),scope.kind.as_str(),author.to_string(),sent_at as i64],|row|row.get(0))?;
        if unresolved {
            return Err(StorageError::InvalidData(
                "Historical reaction ownership requires a complete authoritative snapshot".into(),
            ));
        }
        Ok(())
    }

    /// Replace a verified complete server snapshot and both projections atomically.
    /// A nonempty clockless snapshot cannot create ordered actor evidence: each
    /// desired vote must agree with retained add/selection authority.
    pub fn replace_reaction_snapshot(
        &self,
        scope: &ReactionScope,
        author: &UserId,
        sent_at: u64,
        current: &UserId,
        desired: &[(UserId, String)],
    ) -> Result<(), StorageError> {
        let sent_at = self.resolve_channel_reaction_clock(scope, author, sent_at)?;
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at=?4 AND is_deleted=0)",params![scope.id.to_string(),scope.kind.as_str(),author.to_string(),sent_at as i64],|r|r.get(0))?;
        if !exists {
            return Err(StorageError::InvalidData(
                "Reaction snapshot target is no longer stored".into(),
            ));
        }
        let mut actors = std::collections::HashSet::new();
        for (actor, emoji) in desired {
            if !actors.insert(actor) {
                return Err(StorageError::InvalidData(
                    "Reaction snapshot contains several votes for one actor".into(),
                ));
            }
            let ordered: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM reaction_actor_selections s JOIN reaction_action_versions v ON v.conversation_id=s.conversation_id AND v.conversation_type=s.conversation_type AND v.author_id=s.author_id AND v.target_sent_at=s.target_sent_at AND v.actor_id=s.actor_id AND v.emoji=s.emoji WHERE s.conversation_id=?1 AND s.conversation_type=?2 AND s.author_id=?3 AND s.target_sent_at=?4 AND s.actor_id=?5 AND s.emoji=?6 AND v.remove=0 AND v.action_ts=s.action_ts)",params![scope.id.to_string(),scope.kind.as_str(),author.to_string(),sent_at as i64,actor.to_string(),emoji],|r|r.get(0))?;
            if !ordered {
                return Err(StorageError::InvalidData(
                    "Reaction snapshot lacks compatible ordered actor evidence".into(),
                ));
            }
        }
        tx.execute("DELETE FROM unified_reactions WHERE conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at_ns=?4",params![scope.id.to_string(),scope.kind.as_str(),author.to_string(),sent_at as i64])?;
        for (user, emoji) in desired {
            tx.execute("INSERT OR IGNORE INTO unified_reactions(id,conversation_id,conversation_type,sender_id,sent_at_ns,user_id,emoji,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",params![ReactionId::new().to_string(),scope.id.to_string(),scope.kind.as_str(),author.to_string(),sent_at as i64,user.to_string(),emoji,Utc::now().to_rfc3339()])?;
        }
        self.rebuild_reaction_blobs_with_conn(&tx, scope, author, sent_at, current)?;
        tx.execute("DELETE FROM reaction_scope_unresolved WHERE conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at_ns=?4",params![scope.id.to_string(),scope.kind.as_str(),author.to_string(),sent_at as i64])?;
        tx.commit()?;
        Ok(())
    }

    /// Add a reaction using Signal-style (sender_id, sent_at_ns) message identification.
    /// Inserts into `unified_reactions` and rebuilds the per-user blob cache on
    /// the message row using `current_user_id` (the locally-logged-in user) so
    /// the blob remains a view of *this device's* user, regardless of who the
    /// reactor was.
    ///
    /// Returns `true` if a new reaction was added.
    pub fn add_reaction(
        &self,
        scope: &ReactionScope,
        sender_id: &UserId,
        sent_at_ns: u64,
        reactor_id: &UserId,
        current_user_id: &UserId,
        emoji: &str,
    ) -> Result<bool, StorageError> {
        self.add_reaction_at(
            scope,
            sender_id,
            sent_at_ns,
            reactor_id,
            current_user_id,
            emoji,
            self.allocate_reaction_clock(reactor_id)?,
        )
    }

    pub fn add_reaction_at(
        &self,
        scope: &ReactionScope,
        sender_id: &UserId,
        sent_at_ns: u64,
        reactor_id: &UserId,
        current_user_id: &UserId,
        emoji: &str,
        action_ts: u64,
    ) -> Result<bool, StorageError> {
        self.add_reaction_outcome_at(
            scope,
            sender_id,
            sent_at_ns,
            reactor_id,
            current_user_id,
            emoji,
            action_ts,
        )
        .map(|outcome| outcome.changed)
    }

    pub(crate) fn add_reaction_outcome_at(
        &self,
        scope: &ReactionScope,
        sender_id: &UserId,
        sent_at_ns: u64,
        reactor_id: &UserId,
        current_user_id: &UserId,
        emoji: &str,
        action_ts: u64,
    ) -> Result<ReactionWriteOutcome, StorageError> {
        let sent_at_ns = self.resolve_channel_reaction_clock(scope, sender_id, sent_at_ns)?;
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let conn = &tx;
        Self::ensure_reaction_scope_resolved_with_conn(conn, scope, sender_id, sent_at_ns)?;
        let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at=?4 AND is_deleted=0)",params![scope.id.to_string(),scope.kind.as_str(),sender_id.to_string(),sent_at_ns as i64],|row|row.get(0))?;
        if !exists {
            return Err(StorageError::InvalidData(
                "Reaction target no longer stored in this scope".into(),
            ));
        }
        if !accept_reaction_action(
            conn, scope, sender_id, sent_at_ns, reactor_id, emoji, action_ts, false,
        )? {
            self.rebuild_reaction_blobs_with_conn(
                conn,
                scope,
                sender_id,
                sent_at_ns,
                current_user_id,
            )?;
            let completed = reaction_action_completed(
                conn, scope, sender_id, sent_at_ns, reactor_id, emoji, action_ts, false,
            )?;
            tx.commit()?;
            return Ok(ReactionWriteOutcome {
                changed: false,
                completed,
            });
        }
        let reaction_id = ReactionId::new();
        let now = Utc::now().to_rfc3339();

        let inserted = conn.execute(
            "INSERT OR IGNORE INTO unified_reactions (id, sender_id, sent_at_ns, user_id, emoji, created_at, conversation_id, conversation_type)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8 WHERE EXISTS (SELECT 1 FROM messages WHERE conversation_id=?7 AND conversation_type=?8 AND sender_id=?2 AND sent_at=?3 AND is_deleted=0)",
            params![
                reaction_id.to_string(),
                sender_id.to_string(),
                sent_at_ns as i64,
                reactor_id.to_string(),
                emoji,
                now,
                scope.id.to_string(), scope.kind.as_str(),
            ],
        )?;

        self.rebuild_reaction_blobs_with_conn(conn, scope, sender_id, sent_at_ns, current_user_id)?;
        let completed = reaction_action_completed(
            conn, scope, sender_id, sent_at_ns, reactor_id, emoji, action_ts, false,
        )?;
        tx.commit()?;
        Ok(ReactionWriteOutcome {
            changed: inserted > 0,
            completed,
        })
    }

    /// Remove a reaction by (sender_id, sent_at_ns, reactor_id, emoji).
    /// Rebuilds the per-user blob for `current_user_id` (see `add_reaction`).
    ///
    /// Returns `true` if a reaction was actually removed.
    pub fn remove_reaction(
        &self,
        scope: &ReactionScope,
        sender_id: &UserId,
        sent_at_ns: u64,
        reactor_id: &UserId,
        current_user_id: &UserId,
        emoji: &str,
    ) -> Result<bool, StorageError> {
        self.remove_reaction_at(
            scope,
            sender_id,
            sent_at_ns,
            reactor_id,
            current_user_id,
            emoji,
            self.allocate_reaction_clock(reactor_id)?,
        )
    }

    pub fn remove_reaction_at(
        &self,
        scope: &ReactionScope,
        sender_id: &UserId,
        sent_at_ns: u64,
        reactor_id: &UserId,
        current_user_id: &UserId,
        emoji: &str,
        action_ts: u64,
    ) -> Result<bool, StorageError> {
        self.remove_reaction_outcome_at(
            scope,
            sender_id,
            sent_at_ns,
            reactor_id,
            current_user_id,
            emoji,
            action_ts,
        )
        .map(|outcome| outcome.changed)
    }

    pub(crate) fn remove_reaction_outcome_at(
        &self,
        scope: &ReactionScope,
        sender_id: &UserId,
        sent_at_ns: u64,
        reactor_id: &UserId,
        current_user_id: &UserId,
        emoji: &str,
        action_ts: u64,
    ) -> Result<ReactionWriteOutcome, StorageError> {
        let sent_at_ns = self.resolve_channel_reaction_clock(scope, sender_id, sent_at_ns)?;
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let conn = &tx;
        Self::ensure_reaction_scope_resolved_with_conn(conn, scope, sender_id, sent_at_ns)?;
        let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at=?4 AND is_deleted=0)",params![scope.id.to_string(),scope.kind.as_str(),sender_id.to_string(),sent_at_ns as i64],|row|row.get(0))?;
        if !exists {
            return Err(StorageError::InvalidData(
                "Reaction target no longer stored in this scope".into(),
            ));
        }
        if !accept_reaction_action(
            conn, scope, sender_id, sent_at_ns, reactor_id, emoji, action_ts, true,
        )? {
            self.rebuild_reaction_blobs_with_conn(
                conn,
                scope,
                sender_id,
                sent_at_ns,
                current_user_id,
            )?;
            let completed = reaction_action_completed(
                conn, scope, sender_id, sent_at_ns, reactor_id, emoji, action_ts, true,
            )?;
            tx.commit()?;
            return Ok(ReactionWriteOutcome {
                changed: false,
                completed,
            });
        }
        let deleted = conn.execute(
            "DELETE FROM unified_reactions
             WHERE sender_id = ?1 AND sent_at_ns = ?2 AND user_id = ?3 AND emoji = ?4 AND conversation_id=?5 AND conversation_type=?6",
            params![
                sender_id.to_string(),
                sent_at_ns as i64,
                reactor_id.to_string(),
                emoji, scope.id.to_string(), scope.kind.as_str()
            ],
        )?;

        self.rebuild_reaction_blobs_with_conn(conn, scope, sender_id, sent_at_ns, current_user_id)?;
        let completed = reaction_action_completed(
            conn, scope, sender_id, sent_at_ns, reactor_id, emoji, action_ts, true,
        )?;
        tx.commit()?;
        Ok(ReactionWriteOutcome {
            changed: deleted > 0,
            completed,
        })
    }

    /// Check whether the current user has reacted with a given emoji on a message.
    ///
    /// Reads the per-user cache on the message row (`current_user_reactions_blob`).
    /// The blob is maintained to reflect the locally-logged-in user's reactions
    /// across all conversation types (anon DM/group rebuild the blob for the
    /// logged-in user; regular DM/channel receive it from the server via proto).
    pub fn user_has_reacted(
        &self,
        scope: &ReactionScope,
        sender_id: &UserId,
        sent_at_ns: u64,
        emoji: &str,
    ) -> Result<bool, StorageError> {
        let sent_at_ns = self.resolve_channel_reaction_clock(scope, sender_id, sent_at_ns)?;
        self.ensure_reaction_scope_resolved(scope, sender_id, sent_at_ns)?;
        let conn = self.db.read_connection()?;
        let blob: Option<Vec<u8>> = conn
            .query_row(
                "SELECT current_user_reactions_blob FROM messages
                 WHERE sender_id = ?1 AND sent_at = ?2 AND conversation_id=?3 AND conversation_type=?4",
                params![sender_id.to_string(), sent_at_ns as i64, scope.id.to_string(), scope.kind.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        let reactions: Vec<String> = blob
            .map(|b| db_deserialize(&b))
            .transpose()?
            .unwrap_or_default();
        Ok(reactions.iter().any(|e| e == emoji))
    }

    /// Get all reactions on a message identified by (sender_id, sent_at_ns).
    pub fn get_reactions(
        &self,
        scope: &ReactionScope,
        sender_id: &UserId,
        sent_at_ns: u64,
    ) -> Result<Vec<Reaction>, StorageError> {
        let sent_at_ns = self.resolve_channel_reaction_clock(scope, sender_id, sent_at_ns)?;
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, sender_id, sent_at_ns, user_id, emoji, created_at
             FROM unified_reactions WHERE sender_id = ?1 AND sent_at_ns = ?2 AND conversation_id=?3 AND conversation_type=?4
             ORDER BY created_at ASC",
        )?;
        let mut rows = stmt.query(params![
            sender_id.to_string(),
            sent_at_ns as i64,
            scope.id.to_string(),
            scope.kind.as_str()
        ])?;
        let mut reactions = Vec::new();
        while let Some(row) = rows.next()? {
            reactions.push(row_to_reaction(row)?);
        }
        Ok(reactions)
    }

    /// Get reaction counts per emoji for a message (from unified_reactions, not blob).
    pub fn get_reaction_counts(
        &self,
        scope: &ReactionScope,
        sender_id: &UserId,
        sent_at_ns: u64,
    ) -> Result<Vec<ReactionCount>, StorageError> {
        let sent_at_ns = self.resolve_channel_reaction_clock(scope, sender_id, sent_at_ns)?;
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT emoji, COUNT(*) FROM unified_reactions
             WHERE sender_id = ?1 AND sent_at_ns = ?2 AND conversation_id=?3 AND conversation_type=?4 GROUP BY emoji ORDER BY COUNT(*) DESC",
        )?;
        let mut rows = stmt.query(params![
            sender_id.to_string(),
            sent_at_ns as i64,
            scope.id.to_string(),
            scope.kind.as_str()
        ])?;
        let mut counts = Vec::new();
        while let Some(row) = rows.next()? {
            let emoji_str: String = row.get(0)?;
            let count: u32 = row.get(1)?;
            counts.push(ReactionCount::new(emoji_str, count));
        }
        Ok(counts)
    }

    /// Get reaction counts for all messages in a conversation.
    ///
    /// Returns a map of `sent_at` -> `Vec<(emoji, count)>` for messages that have reactions.
    pub fn get_reaction_counts_for_conversation(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<std::collections::HashMap<u64, Vec<(String, u32)>>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT r.sent_at_ns, r.emoji, COUNT(*) as cnt
             FROM unified_reactions r
             INNER JOIN messages m ON r.sender_id = m.sender_id AND r.sent_at_ns = m.sent_at AND r.conversation_id=m.conversation_id AND r.conversation_type=m.conversation_type
             WHERE m.conversation_id = ?1 AND m.conversation_type = ?2 AND m.is_deleted = 0
               AND NOT EXISTS(SELECT 1 FROM reaction_scope_unresolved u WHERE u.conversation_id=m.conversation_id AND u.conversation_type=m.conversation_type AND u.sender_id=m.sender_id AND u.sent_at_ns=m.sent_at)
             GROUP BY r.sent_at_ns, r.emoji
             ORDER BY r.sent_at_ns, cnt DESC",
        )?;
        let mut rows = stmt.query(params![
            conversation_id.to_string(),
            conversation_type.as_str()
        ])?;
        let mut map: std::collections::HashMap<u64, Vec<(String, u32)>> =
            std::collections::HashMap::new();
        while let Some(row) = rows.next()? {
            let sent_at: i64 = row.get(0)?;
            let emoji: String = row.get(1)?;
            let count: u32 = row.get(2)?;
            map.entry(sent_at as u64).or_default().push((emoji, count));
        }
        Ok(map)
    }

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
        before_sent_at: Option<u64>,
        limit: u32,
    ) -> Result<Vec<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut page = if let Some(before) = before_sent_at {
            let mut stmt = conn.prepare(&format!(
                "SELECT {} FROM messages \
                 WHERE thread_id = ?1 AND sent_at < ?2 AND is_deleted = 0 \
                 ORDER BY sent_at DESC LIMIT ?3",
                message_columns("messages")
            ))?;
            let mut rows = stmt.query(params![thread_id.to_string(), before as i64, limit])?;
            collect_messages(&mut rows)?
        } else {
            let mut stmt = conn.prepare(&format!(
                "SELECT {} FROM messages \
                 WHERE thread_id = ?1 AND is_deleted = 0 \
                 ORDER BY sent_at DESC LIMIT ?2",
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

    /// Apply a `ReadSync` from another of our devices: mark each named message
    /// read. Every entry is scoped to its conversation, author and sent time —
    /// never a bare `sent_at` match (INFRA-25: mesh timestamps collide) — and
    /// to server conversations, the only ones a `ReadSync` is sent for. Our own
    /// messages are never "unread" and are skipped. Returns the conversations
    /// in which something was newly read.
    pub fn mark_read_synced(
        &self,
        reads: &[(ChannelId, UserId, u64)],
        self_user_id: &UserId,
    ) -> Result<Vec<ChannelId>, StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now().to_rfc3339();
        let mut changed = Vec::new();
        for (conversation_id, sender_id, sent_at) in reads {
            if sender_id == self_user_id {
                continue;
            }
            let rows = conn.execute(
                "UPDATE messages SET is_read = 1, updated_at = ?1
                 WHERE conversation_id = ?2 AND sender_id = ?3 AND sent_at = ?4
                   AND is_read = 0
                   AND conversation_type IN ('server_dm', 'channel')",
                params![
                    now,
                    conversation_id.to_string(),
                    sender_id.to_string(),
                    *sent_at as i64
                ],
            )?;
            if rows > 0 && !changed.contains(conversation_id) {
                changed.push(*conversation_id);
            }
        }
        super::ephemeral_repo::start_read_countdowns(&conn)?;
        Ok(changed)
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
        let conn = self.db.connection()?;
        let now = Utc::now().to_rfc3339();
        let mut total = 0;
        for &ts in timestamps {
            let rows = conn.execute(
                "UPDATE messages SET delivery_status = ?1, updated_at = ?2
                 WHERE conversation_id = ?3 AND conversation_type = ?4 AND sent_at = ?5
                   AND CASE delivery_status
                         WHEN 'read' THEN 4 WHEN 'delivered' THEN 3
                         WHEN 'sent' THEN 2 ELSE 0 END
                       <= CASE ?1
                         WHEN 'read' THEN 4 WHEN 'delivered' THEN 3
                         WHEN 'sent' THEN 2 ELSE 0 END",
                params![
                    status,
                    now,
                    channel_id.to_string(),
                    conversation_type.as_str(),
                    ts as i64
                ],
            )?;
            total += rows;
        }
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

    // ========================================================================
    // Internal helpers
    // ========================================================================

    /// Rebuild `reaction_counts_blob` and `current_user_reactions_blob` on the
    /// message row from the `unified_reactions` table (source of truth).
    /// Resolves the message row via (sender_id, sent_at) in the messages table.
    fn rebuild_reaction_blobs_with_conn(
        &self,
        conn: &rusqlite::Connection,
        scope: &ReactionScope,
        sender_id: &UserId,
        sent_at_ns: u64,
        current_user_id: &UserId,
    ) -> Result<(), StorageError> {
        let sid = sender_id.to_string();
        let uid = current_user_id.to_string();

        // Reaction counts
        let mut stmt = conn.prepare(
            "SELECT emoji, COUNT(*) FROM unified_reactions
             WHERE sender_id = ?1 AND sent_at_ns = ?2 AND conversation_id=?3 AND conversation_type=?4 GROUP BY emoji",
        )?;
        let mut rows = stmt.query(params![
            &sid,
            sent_at_ns as i64,
            scope.id.to_string(),
            scope.kind.as_str()
        ])?;
        let mut counts: Vec<ReactionCount> = Vec::new();
        while let Some(row) = rows.next()? {
            let emoji_str: String = row.get(0)?;
            let count: u32 = row.get(1)?;
            counts.push(ReactionCount::new(emoji_str, count));
        }
        drop(rows);
        drop(stmt);

        // Current user's reactions
        let mut stmt2 = conn.prepare(
            "SELECT emoji FROM unified_reactions
             WHERE sender_id = ?1 AND sent_at_ns = ?2 AND user_id = ?3 AND conversation_id=?4 AND conversation_type=?5",
        )?;
        let mut rows2 = stmt2.query(params![
            &sid,
            sent_at_ns as i64,
            &uid,
            scope.id.to_string(),
            scope.kind.as_str()
        ])?;
        let mut user_reactions: Vec<String> = Vec::new();
        while let Some(row) = rows2.next()? {
            user_reactions.push(row.get(0)?);
        }
        drop(rows2);
        drop(stmt2);

        let counts_blob = db_serialize(&counts)?;
        let user_blob = db_serialize(&user_reactions)?;

        // Update the message row, resolving via (sender_id, sent_at)
        conn.execute(
            "UPDATE messages SET reaction_counts_blob = ?1, current_user_reactions_blob = ?2, updated_at = ?3
             WHERE sender_id = ?4 AND sent_at = ?5 AND is_deleted = 0 AND conversation_id=?6 AND conversation_type=?7",
            params![counts_blob, user_blob, Utc::now().to_rfc3339(), &sid, sent_at_ns as i64, scope.id.to_string(), scope.kind.as_str()],
        )?;

        Ok(())
    }
}

// ============================================================================
// Column list & row mappers
// ============================================================================

// Timestamp-only quote IDs are hashes, so recover the local ID from the
// author's timestamps rather than trying to decode the UUID or match text.
fn resolve_timestamp_quote(
    conn: &rusqlite::Connection,
    parent: &MessageId,
    author: &UserId,
    conversation: &ChannelId,
    kind: &ConversationType,
) -> Result<Option<MessageId>, StorageError> {
    let mut stmt = conn.prepare(
        "SELECT id, sent_at FROM messages WHERE sender_id = ?1 AND conversation_id = ?2 AND conversation_type = ?3",
    )?;
    let mut rows = stmt.query(params![
        author.to_string(),
        conversation.to_string(),
        kind.as_str()
    ])?;
    while let Some(row) = rows.next()? {
        let sent_at = row.get::<_, i64>(1)? as u64;
        if MessageId::from_timestamp(sent_at) == *parent {
            let id: String = row.get(0)?;
            return Ok(id.parse().ok());
        }
    }
    Ok(None)
}

/// SELECT column list for unified messages — must match `row_to_message` column indices.
const MSG_COLS: &str = "id, conversation_id, conversation_type, sender_id, device_id, \
     content_json, encrypted_content, \
     thread_id, reply_to, attachments_blob, \
     is_pinned, is_edited, is_deleted, is_read, \
     sent_at, created_at, updated_at, edited_at, \
     reaction_counts_blob, current_user_reactions_blob, \
     mentioned_integrations_blob, targets_ai_bot, delivery_status, \
     reply_to_author_id, reply_to_text, \
     reply_to_thumbnail, reply_to_thumbnail_mime, failure_reason";

/// Every ordinary message reader masks unresolved legacy reaction projections.
/// Raw history stays stored and is never exposed as a scoped authoritative vote.
fn message_columns(alias: &str) -> String {
    let base = format!("{alias}.{}", MSG_COLS.replace(", ", &format!(", {alias}.")));
    format!(
        "{base}, EXISTS(SELECT 1 FROM reaction_scope_unresolved r WHERE r.conversation_id={alias}.conversation_id AND r.conversation_type={alias}.conversation_type AND r.sender_id={alias}.sender_id AND r.sent_at_ns={alias}.sent_at)"
    )
}

/// Parse an RFC3339 datetime, falling back to now.
fn parse_datetime(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

/// Content type → integer discriminant.
fn content_type_discriminant(content: &MessageContent) -> i32 {
    match content {
        MessageContent::Text(_) => 0,
        MessageContent::Encrypted(_) => 1,
        MessageContent::SealedSender(_) => 2,
        MessageContent::System(_) => 3,
        MessageContent::Call(_) => 4,
        MessageContent::AI(_) => 5,
        MessageContent::Mixed(_) => 6,
    }
}

/// Map a row to Message.
///
/// Column order: id(0), conversation_id(1), conversation_type(2), sender_id(3),
/// device_id(4), content_json(5), encrypted_content(6), thread_id(7), reply_to(8),
/// attachments_blob(9), is_pinned(10), is_edited(11), is_deleted(12), is_read(13),
/// sent_at(14), created_at(15), updated_at(16), edited_at(17),
/// reaction_counts_blob(18), current_user_reactions_blob(19),
/// mentioned_integrations_blob(20), targets_ai_bot(21), delivery_status(22),
/// reply_to_author_id(23), reply_to_text(24)
fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Message, StorageError>> {
    let content_json: String = row.get(5)?;
    let attachments_blob: Vec<u8> = row.get(9)?;
    let reaction_counts_blob: Vec<u8> = row.get(18)?;
    let current_user_reactions_blob: Vec<u8> = row.get(19)?;
    let mentioned_integrations_blob: Vec<u8> = row.get(20)?;

    let conversation_type_str: String = row.get(2)?;
    let sender_id: Option<String> = row.get(3)?;
    let device_id: Option<i64> = row.get(4)?;
    let encrypted_content: Option<Vec<u8>> = row.get(6)?;
    let thread_id: Option<String> = row.get(7)?;
    let reply_to: Option<String> = row.get(8)?;
    let edited_at: Option<String> = row.get(17)?;

    let content: MessageContent = match serde_json::from_str(&content_json) {
        Ok(c) => c,
        Err(e) => {
            return Ok(Err(StorageError::InvalidData(format!(
                "Invalid content JSON: {}",
                e
            ))));
        }
    };

    let attachments: Vec<AttachmentId> = match db_deserialize(&attachments_blob) {
        Ok(a) => a,
        Err(e) => return Ok(Err(e)),
    };
    let unresolved: bool = row.get(28)?;
    let reaction_counts: Vec<ReactionCount> = if unresolved {
        Vec::new()
    } else {
        match db_deserialize(&reaction_counts_blob) {
            Ok(r) => r,
            Err(e) => return Ok(Err(e)),
        }
    };
    let current_user_reactions: Vec<String> = if unresolved {
        Vec::new()
    } else {
        match db_deserialize(&current_user_reactions_blob) {
            Ok(r) => r,
            Err(e) => return Ok(Err(e)),
        }
    };
    let mentioned_integrations: Vec<IntegrationId> =
        match db_deserialize(&mentioned_integrations_blob) {
            Ok(m) => m,
            Err(e) => return Ok(Err(e)),
        };

    Ok(Ok(Message {
        id: row
            .get::<_, String>(0)?
            .parse()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        conversation_id: row
            .get::<_, String>(1)?
            .parse()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        conversation_type: match ConversationType::from_str_value(&conversation_type_str) {
            Ok(ct) => ct,
            Err(e) => return Ok(Err(e)),
        },
        sender_id: sender_id
            .map(|s| s.parse())
            .transpose()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        device_id: device_id.map(|d| d as u32),
        content,
        encrypted_content,
        thread_id: thread_id
            .map(|s| s.parse())
            .transpose()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        reply_to: reply_to
            .map(|s| s.parse())
            .transpose()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        reply_to_author_id: row
            .get::<_, Option<String>>(23)?
            .and_then(|s| s.parse().ok()),
        reply_to_text: row.get::<_, Option<String>>(24)?,
        reply_to_thumbnail: row.get::<_, Option<Vec<u8>>>(25)?,
        reply_to_thumbnail_mime: row.get::<_, Option<String>>(26)?,
        attachments,
        is_pinned: row.get::<_, i32>(10)? != 0,
        is_edited: row.get::<_, i32>(11)? != 0,
        is_deleted: row.get::<_, i32>(12)? != 0,
        is_read: row.get::<_, i32>(13)? != 0,
        sent_at: row.get::<_, i64>(14)? as u64,
        created_at: parse_datetime(&row.get::<_, String>(15)?),
        updated_at: parse_datetime(&row.get::<_, String>(16)?),
        edited_at: edited_at.map(|s| parse_datetime(&s)),
        reaction_counts,
        current_user_reactions,
        mentioned_integrations,
        targets_ai_bot: row.get::<_, i32>(21)? != 0,
        delivery_status: row.get::<_, String>(22).unwrap_or_default(),
        failure_reason: row.get::<_, Option<String>>(27)?,
    }))
}

/// Map a unified_reactions row to Reaction.
/// Expected columns: id, sender_id, sent_at_ns, user_id, emoji, created_at
fn row_to_reaction(row: &rusqlite::Row<'_>) -> Result<Reaction, StorageError> {
    let id_str: String = row.get(0).map_err(StorageError::Database)?;
    let sender_id_str: String = row.get(1).map_err(StorageError::Database)?;
    let sent_at_ns: i64 = row.get(2).map_err(StorageError::Database)?;
    let user_id_str: String = row.get(3).map_err(StorageError::Database)?;
    let emoji_str: String = row.get(4).map_err(StorageError::Database)?;
    let created_at_str: String = row.get(5).map_err(StorageError::Database)?;

    Ok(Reaction {
        id: id_str
            .parse()
            .map_err(|e| StorageError::InvalidData(format!("Invalid reaction ID: {}", e)))?,
        sender_id: sender_id_str
            .parse()
            .map_err(|e| StorageError::InvalidData(format!("Invalid sender ID: {}", e)))?,
        sent_at_ns: sent_at_ns as u64,
        user_id: user_id_str
            .parse()
            .map_err(|e| StorageError::InvalidData(format!("Invalid user ID: {}", e)))?,
        emoji: Emoji::unicode(&emoji_str),
        created_at: parse_datetime(&created_at_str),
    })
}

/// Map a row to Thread.
fn row_to_thread(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Thread, StorageError>> {
    let participant_ids_blob: Vec<u8> = row.get(4)?;
    let last_reply_at: Option<String> = row.get(3)?;

    let participant_ids: Vec<UserId> = match db_deserialize(&participant_ids_blob) {
        Ok(p) => p,
        Err(e) => return Ok(Err(e)),
    };

    Ok(Ok(Thread {
        root_message_id: row
            .get::<_, String>(0)?
            .parse()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        conversation_id: row
            .get::<_, String>(1)?
            .parse()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        reply_count: row.get(2)?,
        last_reply_at: last_reply_at.map(|s| parse_datetime(&s)),
        participant_ids,
    }))
}

/// Collect messages from a Rows iterator.
fn collect_messages(rows: &mut rusqlite::Rows<'_>) -> Result<Vec<Message>, StorageError> {
    let mut messages = Vec::new();
    while let Some(row) = rows.next()? {
        let msg = row_to_message(row)?;
        messages.push(msg?);
    }
    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::message::{EncryptedContent, EncryptedMessageType};

    #[test]
    fn pin_added_order_survives_duplicate_pin_and_message_edits() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open(&dir.path().join("pins.db"), "fixture-only").unwrap());
        db.run_migrations().unwrap();
        let repo = UnifiedMessageRepository::new(db.clone());
        for kind in [
            ConversationType::Channel,
            ConversationType::ServerDm,
            ConversationType::AnonDm,
            ConversationType::AnonGroup,
            ConversationType::SavedMessages,
        ] {
            let conversation = ChannelId::new();
            let newer_message = Message::new_text(
                conversation,
                kind.clone(),
                Some(UserId::new()),
                "newer original".into(),
                200,
            );
            let older_message = Message::new_text(
                conversation,
                kind.clone(),
                Some(UserId::new()),
                "older original".into(),
                100,
            );
            repo.insert(&newer_message).unwrap();
            repo.insert(&older_message).unwrap();
            repo.pin(&newer_message.id).unwrap();
            repo.set_pinned_by_timestamp(&conversation, &kind, 100, true)
                .unwrap();
            let order = || {
                repo.list_pinned(&conversation, &kind)
                    .unwrap()
                    .into_iter()
                    .map(|m| m.id)
                    .collect::<Vec<_>>()
            };
            assert_eq!(order(), vec![older_message.id, newer_message.id]);
            let clock = |id: MessageId| {
                db.read_connection()
                    .unwrap()
                    .query_row(
                        "SELECT pinned_at FROM messages WHERE id=?1",
                        [id.to_string()],
                        |r| r.get::<_, i64>(0),
                    )
                    .unwrap()
            };
            let original_clock = clock(newer_message.id);
            repo.pin(&newer_message.id).unwrap();
            let pinned = repo.get(&newer_message.id).unwrap().unwrap();
            repo.update(&pinned).unwrap();
            assert_eq!(clock(newer_message.id), original_clock);
            assert_eq!(order(), vec![older_message.id, newer_message.id]);
            repo.unpin(&newer_message.id).unwrap();
            repo.pin(&newer_message.id).unwrap();
            assert_eq!(order(), vec![newer_message.id, older_message.id]);
        }
    }

    #[test]
    fn stored_timestamp_quote_resolves_when_parent_arrives_in_same_conversation() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE messages (id TEXT, sender_id TEXT, conversation_id TEXT, conversation_type TEXT, sent_at INTEGER)").unwrap();
        let author = UserId::new();
        let other_author = UserId::new();
        let conversation = ChannelId::new();
        let other_conversation = ChannelId::new();
        let parent = MessageId::new();
        let reference = MessageId::from_timestamp(123456789);
        let kind = ConversationType::Channel;
        let resolve =
            || resolve_timestamp_quote(&conn, &reference, &author, &conversation, &kind).unwrap();
        assert_eq!(resolve(), None);
        for (sender, channel) in [(other_author, conversation), (author, other_conversation)] {
            conn.execute(
                "INSERT INTO messages VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    MessageId::new().to_string(),
                    sender.to_string(),
                    channel.to_string(),
                    kind.as_str(),
                    123456789i64
                ],
            )
            .unwrap();
        }
        assert_eq!(resolve(), None);
        conn.execute(
            "INSERT INTO messages VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                parent.to_string(),
                author.to_string(),
                conversation.to_string(),
                kind.as_str(),
                123456789i64
            ],
        )
        .unwrap();
        assert_eq!(resolve(), Some(parent));
        assert_eq!(
            resolve_timestamp_quote(
                &conn,
                &MessageId::from_timestamp(99),
                &author,
                &conversation,
                &kind
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn test_content_type_discriminant_covers_all_variants() {
        // Ensure every MessageContent variant has a discriminant
        let text = MessageContent::Text("hello".to_string());
        assert_eq!(content_type_discriminant(&text), 0);

        let encrypted = MessageContent::Encrypted(EncryptedContent {
            ciphertext: vec![1, 2, 3],
            sender_key_id: None,
            message_type: EncryptedMessageType::Signal,
        });
        assert_eq!(content_type_discriminant(&encrypted), 1);
    }

    #[test]
    fn deletion_token_round_trips_through_the_message_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(Database::open(&dir.path().join("m.db"), "testpass").expect("open"));
        db.run_migrations().expect("migrations");
        let repo = UnifiedMessageRepository::new(db);
        let msg = Message::new_text(
            ChannelId::new(),
            ConversationType::ServerDm,
            Some(UserId::new()),
            "hi".into(),
            1,
        );
        repo.insert(&msg).unwrap();

        assert_eq!(
            repo.deletion_token(&msg.id).unwrap(),
            None,
            "no token until one is set"
        );
        repo.set_deletion_token(&msg.id, &[3u8; 32]).unwrap();
        assert_eq!(repo.deletion_token(&msg.id).unwrap(), Some([3u8; 32]));
        // A full-row update must not clobber it.
        repo.update(&msg).unwrap();
        assert_eq!(repo.deletion_token(&msg.id).unwrap(), Some([3u8; 32]));
        assert!(
            repo.set_deletion_token(&MessageId::new(), &[1u8; 32])
                .is_err()
        );
        assert_eq!(repo.deletion_token(&MessageId::new()).unwrap(), None);
    }

    #[test]
    fn single_reaction_replaces_and_rejects_delayed_events_in_every_scope() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open(&dir.path().join("one-reaction.db"), "testpass").unwrap());
        db.run_migrations().unwrap();
        let repo = UnifiedMessageRepository::new(db);
        let actor = UserId::new();
        let peer = UserId::new();
        let author = UserId::new();
        for kind in [
            ConversationType::ServerDm,
            ConversationType::Channel,
            ConversationType::AnonDm,
            ConversationType::AnonGroup,
        ] {
            let msg = Message::new_text(ChannelId::new(), kind, Some(author), "keep".into(), 100);
            repo.insert(&msg).unwrap();
            let scope = ReactionScope::of(&msg);
            assert!(
                repo.add_reaction_at(&scope, &author, msg.sent_at, &actor, &actor, "👍", 10)
                    .unwrap()
            );
            assert!(
                repo.add_reaction_at(&scope, &author, msg.sent_at, &peer, &actor, "👍", 11)
                    .unwrap()
            );
            assert!(
                repo.add_reaction_at(&scope, &author, msg.sent_at, &actor, &actor, "❤️", 20)
                    .unwrap()
            );
            let saved = repo
                .get_by_reaction_scope(&scope, &author, msg.sent_at)
                .unwrap()
                .unwrap();
            assert_eq!(saved.current_user_reactions, vec!["❤️"]);
            assert_eq!(
                saved.reaction_counts.iter().map(|r| r.count).sum::<u32>(),
                2
            );
            assert!(
                !repo
                    .add_reaction_at(&scope, &author, msg.sent_at, &actor, &actor, "👍", 15)
                    .unwrap()
            );
            assert!(
                !repo
                    .remove_reaction_at(&scope, &author, msg.sent_at, &actor, &actor, "👍", 21)
                    .unwrap()
            );
            assert_eq!(
                repo.get_by_reaction_scope(&scope, &author, msg.sent_at)
                    .unwrap()
                    .unwrap()
                    .current_user_reactions,
                vec!["❤️"]
            );
            assert!(
                repo.remove_reaction_at(&scope, &author, msg.sent_at, &actor, &actor, "❤️", 30)
                    .unwrap()
            );
            assert!(
                !repo
                    .add_reaction_at(&scope, &author, msg.sent_at, &actor, &actor, "😂", 15)
                    .unwrap()
            );
            assert!(
                repo.add_reaction_at(&scope, &author, msg.sent_at, &actor, &actor, "😂", 40)
                    .unwrap()
            );
            let saved = repo
                .get_by_reaction_scope(&scope, &author, msg.sent_at)
                .unwrap()
                .unwrap();
            assert_eq!(saved.current_user_reactions, vec!["😂"]);
            assert_eq!(
                saved.reaction_counts.iter().map(|r| r.count).sum::<u32>(),
                2
            );
        }
    }

    #[test]
    fn single_reaction_delayed_remove_of_previous_emoji_preserves_new_selection() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open(&dir.path().join("reordered.db"), "testpass").unwrap());
        db.run_migrations().unwrap();
        let repo = UnifiedMessageRepository::new(db);
        let actor = UserId::new();
        for remove_first in [true, false] {
            let msg = Message::new_text(
                ChannelId::new(),
                ConversationType::ServerDm,
                Some(actor),
                "order".into(),
                100,
            );
            repo.insert(&msg).unwrap();
            let scope = ReactionScope::of(&msg);
            repo.add_reaction_at(&scope, &actor, msg.sent_at, &actor, &actor, "👍", 10)
                .unwrap();
            if remove_first {
                repo.remove_reaction_at(&scope, &actor, msg.sent_at, &actor, &actor, "👍", 30)
                    .unwrap();
            }
            assert!(
                repo.add_reaction_at(&scope, &actor, msg.sent_at, &actor, &actor, "❤️", 20)
                    .unwrap()
            );
            if !remove_first {
                repo.remove_reaction_at(&scope, &actor, msg.sent_at, &actor, &actor, "👍", 30)
                    .unwrap();
            }
            assert_eq!(
                repo.get_by_reaction_scope(&scope, &actor, msg.sent_at)
                    .unwrap()
                    .unwrap()
                    .current_user_reactions,
                vec!["❤️"]
            );
            repo.remove_reaction_at(&scope, &actor, msg.sent_at, &actor, &actor, "❤️", 40)
                .unwrap();
            assert!(
                !repo
                    .add_reaction_at(&scope, &actor, msg.sent_at, &actor, &actor, "❤️", 35)
                    .unwrap()
            );
            assert!(
                !repo
                    .add_reaction_at(&scope, &actor, msg.sent_at, &actor, &actor, "👍", 15)
                    .unwrap()
            );
        }
    }

    #[test]
    fn test_like_pattern_escaping() {
        // LIKE special chars are escaped so they match literally
        let query = "100% done_now";
        let pattern = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
        assert_eq!(pattern, "%100\\% done\\_now%");
    }
}

fn accept_reaction_action(
    conn: &rusqlite::Connection,
    scope: &ReactionScope,
    author: &UserId,
    target: u64,
    actor: &UserId,
    emoji: &str,
    action: u64,
    remove: bool,
) -> Result<bool, StorageError> {
    if action == 0 || action > i64::MAX as u64 {
        return Err(StorageError::InvalidData(
            "Invalid reaction action clock".into(),
        ));
    }
    // Preserve per-emoji evidence even when a newer cross-emoji add owns the
    // projection. At equal clocks a removal wins, independent of delivery order.
    let accepted = conn.execute("INSERT INTO reaction_action_versions(conversation_id,conversation_type,author_id,target_sent_at,actor_id,emoji,action_ts,remove) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(conversation_type,conversation_id,author_id,target_sent_at,actor_id,emoji) DO UPDATE SET action_ts=excluded.action_ts,remove=excluded.remove WHERE excluded.action_ts>reaction_action_versions.action_ts OR (excluded.action_ts=reaction_action_versions.action_ts AND excluded.remove>reaction_action_versions.remove)",params![scope.id.to_string(),scope.kind.as_str(),author.to_string(),target as i64,actor.to_string(),emoji,action as i64,remove])?>0;
    if !remove {
        // Record the latest observed add even if its same-emoji removal arrived
        // first. This retains the actor's selection through removal/replay and
        // prevents the previous emoji reappearing in the opposite delivery order.
        let selected = conn.execute("INSERT INTO reaction_actor_selections(conversation_id,conversation_type,author_id,target_sent_at,actor_id,emoji,action_ts) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(conversation_type,conversation_id,author_id,target_sent_at,actor_id) DO UPDATE SET emoji=excluded.emoji,action_ts=excluded.action_ts WHERE excluded.action_ts>reaction_actor_selections.action_ts OR (excluded.action_ts=reaction_actor_selections.action_ts AND excluded.emoji>reaction_actor_selections.emoji)", params![scope.id.to_string(),scope.kind.as_str(),author.to_string(),target as i64,actor.to_string(),emoji,action as i64])?>0;
        if selected {
            conn.execute("DELETE FROM unified_reactions WHERE conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at_ns=?4 AND user_id=?5 AND emoji<>?6", params![scope.id.to_string(),scope.kind.as_str(),author.to_string(),target as i64,actor.to_string(),emoji])?;
        }
        return Ok(accepted && selected);
    }
    Ok(accepted)
}

fn reaction_receive_binding(conn: &rusqlite::Connection) -> Result<(String, u32), StorageError> {
    use super::settings_repo::keys;
    let account: String = conn.query_row(
        "SELECT value FROM settings WHERE key=?1",
        [keys::CURRENT_USER_ID],
        |r| r.get(0),
    )?;
    let device: String = conn.query_row(
        "SELECT value FROM settings WHERE key=?1",
        [keys::CURRENT_DEVICE_ID],
        |r| r.get(0),
    )?;
    Ok((
        serde_json::from_str(&account)
            .map_err(|_| StorageError::InvalidData("Invalid receive account".into()))?,
        serde_json::from_str(&device)
            .map_err(|_| StorageError::InvalidData("Invalid receive device".into()))?,
    ))
}

/// Exact retained evidence, read in the writer transaction: a replay is complete,
/// but a newer clock or a different winning selection cannot complete this action.
fn reaction_action_completed(
    conn: &rusqlite::Connection,
    scope: &ReactionScope,
    author: &UserId,
    target: u64,
    actor: &UserId,
    emoji: &str,
    action: u64,
    remove: bool,
) -> Result<bool, StorageError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM reaction_action_versions v
         WHERE v.conversation_id=?1 AND v.conversation_type=?2 AND v.author_id=?3
         AND v.target_sent_at=?4 AND v.actor_id=?5 AND v.emoji=?6
         AND v.action_ts=?7 AND v.remove=?8)
         AND CASE WHEN ?8 THEN NOT EXISTS(SELECT 1 FROM reaction_actor_selections s
             WHERE s.conversation_id=?1 AND s.conversation_type=?2 AND s.author_id=?3
             AND s.target_sent_at=?4 AND s.actor_id=?5 AND s.action_ts>?7)
         ELSE EXISTS(SELECT 1 FROM reaction_actor_selections s
             WHERE s.conversation_id=?1 AND s.conversation_type=?2 AND s.author_id=?3
             AND s.target_sent_at=?4 AND s.actor_id=?5 AND s.emoji=?6 AND s.action_ts=?7) END",
        params![
            scope.id.to_string(),
            scope.kind.as_str(),
            author.to_string(),
            target as i64,
            actor.to_string(),
            emoji,
            action as i64,
            remove
        ],
        |row| row.get(0),
    )?)
}
