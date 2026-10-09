//! Ephemeral message repository.
//!
//! Data access for ephemeral (self-destructing) message tracking.

use crate::domain::ephemeral::{EphemeralMessage, MessageTTL};
use crate::domain::ids::MessageId;
use crate::error::StorageError;
use crate::storage::Database;
use chrono::{DateTime, Utc};
use rusqlite::params;
use std::sync::Arc;

/// Ephemeral message data repository.
///
/// Provides CRUD operations for ephemeral message tracking data.
/// Ephemeral messages have a TTL that starts counting down when read.
pub struct EphemeralRepository {
    db: Arc<Database>,
}

impl EphemeralRepository {
    /// Create a new EphemeralRepository with the given database.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Insert ephemeral tracking for a message.
    pub fn insert(&self, ephemeral: &EphemeralMessage) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT INTO ephemeral_messages (message_id, ttl_seconds, expires_at, read_at, countdown_started)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                ephemeral.message_id.to_string(),
                ephemeral.ttl.as_secs() as i64,
                ephemeral.expires_at.to_rfc3339(),
                ephemeral.read_at.map(|dt| dt.to_rfc3339()),
                ephemeral.countdown_started as i32,
            ],
        )?;
        Ok(())
    }

    /// Arm a message's disappearing timer (Signal's expireTimer).
    ///
    /// The clock starts when the local user has read the message: at once
    /// for a message already stored read (our own — see
    /// `UnifiedMessageRepository::stored_is_read` — or one read before it was
    /// armed), otherwise when it is marked read
    /// (`UnifiedMessageRepository::mark_*read*` call
    /// [`start_read_countdowns`]). A row whose clock runs has
    /// `countdown_started = 1` and `expires_at = start + ttl`, which is what
    /// the reaper (`list_expired`) collects. A message
    /// not stored yet counts as read, so the timer never waits forever.
    /// An own message that is still `sending` or `failed` keeps its stamped
    /// TTL with the clock stopped; the `ephemeral_countdown_on_sent` trigger
    /// (v097) starts it when any path confirms the send.
    ///
    /// Takes arbitrary `ttl_seconds` (a peer's timer need not be a preset).
    /// `INSERT OR REPLACE` so re-arming the same message (a retry, a
    /// self-sync copy after the primary) is idempotent.
    pub fn insert_armed(
        &self,
        message_id: &MessageId,
        ttl_seconds: u32,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT OR REPLACE INTO ephemeral_messages
                 (message_id, ttl_seconds, expires_at, read_at, countdown_started)
             SELECT ?1, ?2,
                    strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', '+' || ?2 || ' seconds'),
                    CASE WHEN read THEN strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') END,
                    read
             FROM (SELECT COALESCE((SELECT is_read AND delivery_status NOT IN ('sending', 'failed')
                                    FROM messages WHERE id = ?1), 1) AS read)",
            params![message_id.to_string(), i64::from(ttl_seconds)],
        )?;
        Ok(())
    }

    /// Get ephemeral state for a message.
    pub fn get(&self, message_id: &MessageId) -> Result<Option<EphemeralMessage>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT message_id, ttl_seconds, expires_at, read_at, countdown_started
             FROM ephemeral_messages WHERE message_id = ?1",
        )?;

        let result = stmt.query_row(params![message_id.to_string()], |row| row_to_ephemeral(row));

        match result {
            Ok(ephemeral) => Ok(Some(ephemeral?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Mark message as read (starts countdown).
    ///
    /// This sets read_at to now, countdown_started to true, and recalculates
    /// expires_at based on now + ttl_seconds.
    pub fn mark_read(&self, message_id: &MessageId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now();

        // Only update if countdown hasn't started (idempotent)
        let rows = conn.execute(
            "UPDATE ephemeral_messages
             SET read_at = ?1,
                 countdown_started = 1,
                 expires_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', ?1, '+' || ttl_seconds || ' seconds')
             WHERE message_id = ?2 AND countdown_started = 0",
            params![now.to_rfc3339(), message_id.to_string()],
        )?;

        if rows == 0 {
            // Check if message exists but was already read
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM ephemeral_messages WHERE message_id = ?1)",
                params![message_id.to_string()],
                |row| row.get(0),
            )?;

            if !exists {
                return Err(StorageError::NotFound(format!(
                    "Ephemeral message {}",
                    message_id
                )));
            }
            // Otherwise already read, which is fine (idempotent)
        }
        Ok(())
    }

    /// List all expired messages (countdown_started=1 AND expires_at < now).
    ///
    /// Returns message IDs that should be deleted.
    pub fn list_expired(&self) -> Result<Vec<MessageId>, StorageError> {
        let conn = self.db.read_connection()?;
        let now = Utc::now().to_rfc3339();

        let mut stmt = conn.prepare(
            "SELECT message_id FROM ephemeral_messages
             WHERE countdown_started = 1 AND expires_at < ?1",
        )?;

        let mut rows = stmt.query(params![now])?;
        let mut message_ids = Vec::new();

        while let Some(row) = rows.next()? {
            let id_str: String = row.get(0)?;
            let message_id = id_str.parse().map_err(|_| {
                StorageError::InvalidData(format!("Invalid message ID: {}", id_str))
            })?;
            message_ids.push(message_id);
        }

        Ok(message_ids)
    }

    /// Delete ephemeral tracking (after message deletion).
    pub fn delete(&self, message_id: &MessageId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "DELETE FROM ephemeral_messages WHERE message_id = ?1",
            params![message_id.to_string()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!(
                "Ephemeral message {}",
                message_id
            )));
        }
        Ok(())
    }

    /// List pending (unread) ephemeral messages.
    ///
    /// Returns ephemeral messages where countdown has not yet started.
    pub fn list_pending(&self) -> Result<Vec<EphemeralMessage>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT message_id, ttl_seconds, expires_at, read_at, countdown_started
             FROM ephemeral_messages WHERE countdown_started = 0",
        )?;

        let mut rows = stmt.query([])?;
        let mut ephemeral_messages = Vec::new();

        while let Some(row) = rows.next()? {
            let ephemeral = row_to_ephemeral(row)?;
            ephemeral_messages.push(ephemeral?);
        }

        Ok(ephemeral_messages)
    }
}

// ============================================================================
// Helper functions
// ============================================================================

/// Start the disappearing clock of every armed message that is now read
/// (Signal: a received message's timer starts when it is read). Called by
/// each `UnifiedMessageRepository` path that marks messages read; the pending
/// set is small (`idx_ephemeral_pending`), so it is not scoped further.
pub(crate) fn start_read_countdowns(conn: &rusqlite::Connection) -> Result<(), StorageError> {
    conn.execute(
        "UPDATE ephemeral_messages
         SET countdown_started = 1,
             read_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
             expires_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', '+' || ttl_seconds || ' seconds')
         WHERE countdown_started = 0
           AND message_id IN (SELECT id FROM messages WHERE is_read = 1
                              AND delivery_status NOT IN ('sending', 'failed'))",
        [],
    )?;
    Ok(())
}

/// Parse an RFC3339 datetime string, falling back to now if invalid.
fn parse_datetime(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

/// Convert a row to an EphemeralMessage.
fn row_to_ephemeral(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<EphemeralMessage, StorageError>> {
    let message_id_str: String = row.get(0)?;
    let ttl_seconds: i64 = row.get(1)?;
    let expires_at_str: String = row.get(2)?;
    let read_at_str: Option<String> = row.get(3)?;
    let countdown_started: i32 = row.get(4)?;

    let message_id = match message_id_str.parse() {
        Ok(id) => id,
        Err(_) => {
            return Ok(Err(StorageError::InvalidData(format!(
                "Invalid message ID: {}",
                message_id_str
            ))));
        }
    };

    // Try to convert ttl_seconds to MessageTTL, fall back to closest match
    let ttl = MessageTTL::from_secs(ttl_seconds as u64).unwrap_or({
        // Fall back to the closest predefined TTL
        if ttl_seconds <= 30 {
            MessageTTL::Seconds30
        } else if ttl_seconds <= 60 {
            MessageTTL::Minutes1
        } else if ttl_seconds <= 300 {
            MessageTTL::Minutes5
        } else if ttl_seconds <= 3600 {
            MessageTTL::Hours1
        } else {
            MessageTTL::Hours24
        }
    });

    Ok(Ok(EphemeralMessage {
        message_id,
        ttl,
        expires_at: parse_datetime(&expires_at_str),
        read_at: read_at_str.map(|s| parse_datetime(&s)),
        countdown_started: countdown_started != 0,
    }))
}
