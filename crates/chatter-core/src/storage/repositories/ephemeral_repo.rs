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

    /// Get ephemeral state for a message.
    pub fn get(&self, message_id: &MessageId) -> Result<Option<EphemeralMessage>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT message_id, ttl_seconds, expires_at, read_at, countdown_started
             FROM ephemeral_messages WHERE message_id = ?1",
        )?;

        let result = stmt.query_row(params![message_id.to_string()], |row| {
            row_to_ephemeral(row)
        });

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
                 expires_at = datetime(?1, '+' || ttl_seconds || ' seconds')
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

    /// Delete all expired ephemeral messages in a single query.
    ///
    /// This is more efficient than list_expired() + N delete() calls.
    /// Returns the message IDs that were deleted.
    pub fn delete_all_expired(&self) -> Result<Vec<MessageId>, StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now().to_rfc3339();

        // First, get the IDs to return (so caller knows what was deleted)
        let ids: Vec<MessageId> = {
            let mut stmt = conn.prepare(
                "SELECT message_id FROM ephemeral_messages
                 WHERE countdown_started = 1 AND expires_at < ?1",
            )?;
            let mut rows = stmt.query(params![now])?;
            let mut ids = Vec::new();
            while let Some(row) = rows.next()? {
                let id_str: String = row.get(0)?;
                if let Ok(id) = id_str.parse() {
                    ids.push(id);
                }
            }
            ids
        };

        // Then delete them all in one query
        if !ids.is_empty() {
            conn.execute(
                "DELETE FROM ephemeral_messages
                 WHERE countdown_started = 1 AND expires_at < ?1",
                params![now],
            )?;
        }

        Ok(ids)
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
            ))))
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
