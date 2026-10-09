//! Pin clock for Saved records synced between our own devices (migration
//! v092).
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

pub struct SavedSyncRepository {
    db: Arc<Database>,
}

impl SavedSyncRepository {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
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
    /// Signal-style key. Stale (clock not newer than the watermark) writes
    /// nothing. Row columns and watermark move in one transaction.
    pub fn apply_synced_pin(
        &self,
        scope: &ReactionScope,
        author: &UserId,
        sent_at: u64,
        id: &MessageId,
        pinned: bool,
        changed_at: u64,
    ) -> Result<PinWrite, StorageError> {
        self.write_pin(scope, author, sent_at, id, pinned, |watermark| {
            (changed_at > watermark).then_some(changed_at)
        })
    }

    /// Apply a pin change made here, dated now — or one tick past the
    /// watermark if this clock reads earlier than a change already applied,
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
        self.write_pin(scope, author, sent_at, id, pinned, |watermark| {
            Some(now_ns.max(watermark.saturating_add(1)))
        })
    }

    /// Record the clock of a pin state stored with a freshly created row
    /// (a snapshot that arrived pinned). Written before the row insert: an
    /// orphan watermark only keeps an older pin from applying later, which
    /// is the right answer anyway; a row without its watermark is not.
    pub fn record_pin_watermark(
        &self,
        id: &MessageId,
        changed_at: u64,
    ) -> Result<(), StorageError> {
        if changed_at == 0 {
            return Ok(());
        }
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT INTO saved_pin_watermarks (message_id, changed_at) VALUES (?1, ?2)
             ON CONFLICT(message_id) DO UPDATE SET changed_at = MAX(changed_at, excluded.changed_at)",
            params![id.to_string(), changed_at as i64],
        )?;
        Ok(())
    }

    fn write_pin(
        &self,
        scope: &ReactionScope,
        author: &UserId,
        sent_at: u64,
        id: &MessageId,
        pinned: bool,
        clock: impl FnOnce(u64) -> Option<u64>,
    ) -> Result<PinWrite, StorageError> {
        let conn = self.db.connection()?;
        let tx = conn.unchecked_transaction()?;
        let watermark = Self::watermark_with(&tx, id)?;
        let Some(changed_at) = clock(watermark) else {
            return Ok(PinWrite::Stale);
        };
        // The pin columns of `UnifiedMessageRepository::set_pinned_scoped`,
        // narrowed to this row id: `pinned_at` orders the pinned list, set on
        // the transition to pinned and cleared on unpin.
        let now = chrono::Utc::now();
        let rows = tx.execute(
            "UPDATE messages SET pinned_at = CASE WHEN ?1 = 0 THEN NULL WHEN is_pinned = 0 THEN ?8 ELSE pinned_at END, is_pinned = ?1, updated_at = ?2
             WHERE id = ?7 AND conversation_id = ?3 AND conversation_type = ?4 AND sender_id = ?5 AND sent_at = ?6 AND is_deleted = 0",
            params![
                pinned as i32,
                now.to_rfc3339(),
                scope.id.to_string(),
                scope.kind.as_str(),
                author.to_string(),
                sent_at as i64,
                id.to_string(),
                now.timestamp_nanos_opt().unwrap_or_default(),
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
        tx.commit()?;
        Ok(PinWrite::Applied { changed_at })
    }
}
