//! Saved pin ordering (v092) and durable local sync intent (v093).
//!
//! A `SavedMessageSync` snapshot carries `pin_changed_at`; a receiver applies
//! its `pinned` only when that clock is newer than the last pin change it
//! applied. `messages.pinned_at` cannot serve as that clock: unpin clears it,
//! so a reordered old pin would re-pin after a newer unpin. The watermark here
//! outlives unpin, and every pin write — local or synced — moves it in the
//! same transaction as the row, so a crash cannot leave the two apart.

use std::sync::Arc;

use rusqlite::{OptionalExtension, params};

use crate::domain::ids::{MessageId, UserId};
use crate::domain::reaction::ReactionScope;
use crate::error::StorageError;
use crate::storage::Database;

/// What one pin write did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinWrite {
    /// Row and watermark written; `changed_at` is the clock now recorded.
    Applied { changed_at: u64 },
    /// The record already saw a pin change at or after this clock.
    Stale,
    /// No undeleted own Saved row at that key.
    NoRow,
}

/// The reply context a snapshot gives a Saved row (`set_reply_once`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedReply {
    /// The quoted record's real id, held here or not.
    pub parent: MessageId,
    pub text: Option<String>,
    pub thumbnail: Option<Vec<u8>>,
    pub thumbnail_mime: Option<String>,
}

pub struct SavedSyncRepository {
    db: Arc<Database>,
}

impl SavedSyncRepository {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Called inside the local row mutation transaction, never on receive.
    pub(crate) fn mark_dirty_with(
        conn: &rusqlite::Connection,
        owner: &UserId,
        id: &MessageId,
    ) -> Result<(), StorageError> {
        crate::services::gateway::submission::check_send_session()?;
        let current: Option<String> = conn
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
        {
            return Err(StorageError::InvalidData(
                "Saved mutation owner changed".into(),
            ));
        }
        conn.execute(
            "INSERT INTO saved_sync_outbox (owner_id, message_id) VALUES (?1, ?2)
             ON CONFLICT(owner_id, message_id) DO UPDATE SET generation = generation + 1,
                 attempts = 0, next_attempt_at = 0",
            params![owner.to_string(), id.to_string()],
        )?;
        Ok(())
    }

    pub(crate) fn pending(
        &self,
        owner: &UserId,
        id: &MessageId,
    ) -> Result<Option<i64>, StorageError> {
        Ok(self
            .db
            .read_connection()?
            .query_row(
                "SELECT generation FROM saved_sync_outbox WHERE owner_id = ?1 AND message_id = ?2",
                params![owner.to_string(), id.to_string()],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Only acknowledge the generation actually sent; a concurrent edit survives.
    pub(crate) fn finish(
        &self,
        owner: &UserId,
        id: &MessageId,
        generation: i64,
        acknowledged: bool,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        if acknowledged {
            conn.execute("DELETE FROM saved_sync_outbox WHERE owner_id = ?1 AND message_id = ?2 AND generation = ?3", params![owner.to_string(), id.to_string(), generation])?;
        } else {
            conn.execute(
                "UPDATE saved_sync_outbox SET attempts = MIN(attempts + 1, 10),
                   next_attempt_at = ?4 + MIN(5 * (1 << MIN(attempts, 6)), 300)
                 WHERE owner_id = ?1 AND message_id = ?2 AND generation = ?3",
                params![
                    owner.to_string(),
                    id.to_string(),
                    generation,
                    chrono::Utc::now().timestamp()
                ],
            )?;
        }
        Ok(())
    }

    pub(crate) fn due(
        &self,
        owner: &UserId,
        reconnect: bool,
    ) -> Result<Vec<MessageId>, StorageError> {
        let conn = self.db.connection()?;
        // A deleted or tombstoned record must never be resurrected by recovery.
        conn.execute("DELETE FROM saved_sync_outbox WHERE owner_id = ?1 AND NOT EXISTS (
            SELECT 1 FROM messages WHERE id = saved_sync_outbox.message_id
              AND sender_id = ?1 AND conversation_id = ?1 AND conversation_type = 'saved_messages'
              AND is_deleted = 0 AND NOT EXISTS (SELECT 1 FROM deleted_messages WHERE id = messages.id))", params![owner.to_string()])?;
        if reconnect {
            conn.execute(
                "UPDATE saved_sync_outbox SET next_attempt_at = 0 WHERE owner_id = ?1",
                params![owner.to_string()],
            )?;
        }
        let mut stmt = conn.prepare("SELECT message_id FROM saved_sync_outbox WHERE owner_id = ?1 AND next_attempt_at <= ?2 ORDER BY next_attempt_at, message_id LIMIT 4")?;
        let ids = stmt
            .query_map(
                params![owner.to_string(), chrono::Utc::now().timestamp()],
                |r| r.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let mut due = Vec::new();
        for id in ids {
            match id.parse() {
                Ok(id) => due.push(id),
                Err(_) => {
                    tracing::warn!(%id, "Discarding malformed Saved outbox identity");
                    conn.execute(
                        "DELETE FROM saved_sync_outbox WHERE owner_id = ?1 AND message_id = ?2",
                        params![owner.to_string(), id],
                    )?;
                }
            }
        }
        Ok(due)
    }

    /// Clock of the last pin change applied to this record; 0 when none.
    pub fn pin_watermark(&self, id: &MessageId) -> Result<u64, StorageError> {
        let conn = self.db.read_connection()?;
        Ok(Self::watermark_with(&conn, id)?)
    }

    fn watermark_with(conn: &rusqlite::Connection, id: &MessageId) -> Result<u64, StorageError> {
        let found: Option<i64> = conn
            .query_row(
                "SELECT changed_at FROM saved_pin_watermarks WHERE message_id = ?1",
                params![id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.unwrap_or_default() as u64)
    }

    /// Apply a pin change made on a sibling device: `pinned` as of
    /// `changed_at`, to the undeleted own Saved row `id` at the scope's
    /// Signal-style key. A clock older than the watermark writes nothing.
    /// At equal clocks unpin wins, independent of arrival order. Row columns
    /// and watermark move in one transaction; initial insertion is atomic too.
    pub fn apply_synced_pin(
        &self,
        scope: &ReactionScope,
        author: &UserId,
        sent_at: u64,
        id: &MessageId,
        pinned: bool,
        changed_at: u64,
    ) -> Result<PinWrite, StorageError> {
        self.write_pin(
            scope,
            author,
            sent_at,
            id,
            pinned,
            false,
            |watermark, stored| {
                (changed_at > watermark || (changed_at == watermark && stored && !pinned))
                    .then_some(changed_at)
            },
        )
    }

    /// Apply a pin change made here, dated now — or one tick past the
    /// watermark if this clock reads earlier than a change already applied,
    /// and never before the message was sent,
    /// so the snapshot we then send is ordered after everything we hold.
    pub fn apply_local_pin(
        &self,
        scope: &ReactionScope,
        author: &UserId,
        sent_at: u64,
        id: &MessageId,
        pinned: bool,
        now_ns: u64,
    ) -> Result<PinWrite, StorageError> {
        self.write_pin(scope, author, sent_at, id, pinned, true, |watermark, _| {
            Some(now_ns.max(sent_at).max(watermark.saturating_add(1)))
        })
    }

    /// Give the own Saved row `id` at this key its reply context, once: the
    /// parent's id (held here or not — a reply keeps naming its parent, as
    /// an ordinary message does), our account, the preview and thumbnail
    /// the snapshot carried. A row that already has a parent is left alone.
    /// `Ok(false)`: no such undeleted row without a parent.
    pub fn set_reply_once(
        &self,
        scope: &ReactionScope,
        author: &UserId,
        sent_at: u64,
        id: &MessageId,
        reply: &SavedReply,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE messages SET reply_to = ?1, reply_to_author_id = ?2, reply_to_text = ?3,
                    reply_to_thumbnail = ?4, reply_to_thumbnail_mime = ?5, updated_at = ?6
             WHERE id = ?7 AND conversation_id = ?8 AND conversation_type = ?9 AND sender_id = ?10
               AND sent_at = ?11 AND is_deleted = 0 AND reply_to IS NULL",
            params![
                reply.parent.to_string(),
                author.to_string(),
                reply.text.as_deref(),
                reply.thumbnail.as_deref(),
                reply.thumbnail_mime.as_deref(),
                chrono::Utc::now().to_rfc3339(),
                id.to_string(),
                scope.id.to_string(),
                scope.kind.as_str(),
                author.to_string(),
                sent_at as i64,
            ],
        )?;
        Ok(rows > 0)
    }

    /// Low-level watermark helper retained for storage regression fixtures.
    /// Production initial insertion uses `insert_saved_snapshot` atomically.
    #[cfg(test)]
    pub fn record_pin_watermark(
        &self,
        id: &MessageId,
        changed_at: u64,
    ) -> Result<(), StorageError> {
        if changed_at == 0 {
            return Ok(());
        }
        if changed_at > i64::MAX as u64 {
            return Err(StorageError::InvalidData(
                "pin clock does not fit the column".into(),
            ));
        }
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT INTO saved_pin_watermarks (message_id, changed_at) VALUES (?1, ?2)
             ON CONFLICT(message_id) DO UPDATE SET changed_at = MAX(changed_at, excluded.changed_at)",
            params![id.to_string(), changed_at as i64],
        )?;
        Ok(())
    }

    /// `clock` sees the watermark and the row's stored pin state and names
    /// the clock to record, or `None` to write nothing.
    fn write_pin(
        &self,
        scope: &ReactionScope,
        author: &UserId,
        sent_at: u64,
        id: &MessageId,
        pinned: bool,
        local: bool,
        clock: impl FnOnce(u64, bool) -> Option<u64>,
    ) -> Result<PinWrite, StorageError> {
        if scope.kind != crate::domain::message::ConversationType::SavedMessages
            || scope.id != crate::domain::ids::ChannelId::from(*author)
            || sent_at > i64::MAX as u64
        {
            return Ok(PinWrite::NoRow);
        }
        let conn = self.db.connection()?;
        let tx = conn.unchecked_transaction()?;
        let watermark = Self::watermark_with(&tx, id)?;
        let stored: Option<bool> = tx
            .query_row(
                "SELECT is_pinned FROM messages
                 WHERE id = ?1 AND conversation_id = ?2 AND conversation_type = ?3
                   AND sender_id = ?4 AND sent_at = ?5 AND is_deleted = 0
                   AND NOT EXISTS (SELECT 1 FROM deleted_messages WHERE id = ?1)",
                params![
                    id.to_string(),
                    scope.id.to_string(),
                    scope.kind.as_str(),
                    author.to_string(),
                    sent_at as i64,
                ],
                |row| row.get(0),
            )
            .optional()?;
        let Some(stored) = stored else {
            return Ok(PinWrite::NoRow);
        };
        let Some(changed_at) = clock(watermark, stored) else {
            return Ok(PinWrite::Stale);
        };
        if changed_at > i64::MAX as u64 {
            return Err(StorageError::InvalidData(
                "pin clock does not fit the column".into(),
            ));
        }
        // The pin columns of `UnifiedMessageRepository::set_pinned_scoped`,
        // narrowed to this row id. `pinned_at` orders the pinned list: on the
        // transition to pinned it is the pin's own clock, the same on every
        // device that applies this change, and unpin clears it.
        let rows = tx.execute(
            "UPDATE messages SET pinned_at = CASE WHEN ?1 = 0 THEN NULL ELSE ?8 END, is_pinned = ?1, updated_at = ?2
             WHERE id = ?7 AND conversation_id = ?3 AND conversation_type = ?4 AND sender_id = ?5 AND sent_at = ?6 AND is_deleted = 0",
            params![
                pinned as i32,
                chrono::Utc::now().to_rfc3339(),
                scope.id.to_string(),
                scope.kind.as_str(),
                author.to_string(),
                sent_at as i64,
                id.to_string(),
                changed_at as i64,
            ],
        )?;
        if rows == 0 {
            return Ok(PinWrite::NoRow);
        }
        tx.execute(
            "INSERT INTO saved_pin_watermarks (message_id, changed_at) VALUES (?1, ?2)
             ON CONFLICT(message_id) DO UPDATE SET changed_at = excluded.changed_at",
            params![id.to_string(), changed_at as i64],
        )?;
        if local {
            Self::mark_dirty_with(&tx, author, id)?;
        }
        tx.commit()?;
        Ok(PinWrite::Applied { changed_at })
    }
}
