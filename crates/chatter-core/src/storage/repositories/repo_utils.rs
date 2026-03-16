//! Shared repository utilities.
//!
//! Common helpers used by both `AnonDmRepository` and `AnonGroupRepository`
//! to eliminate duplicated byte conversion, datetime parsing, and rate
//! limiting logic.

use chrono::{DateTime, Utc};
use rusqlite::params;

use crate::domain::AnonRateLimitEntry;
use crate::error::StorageError;
use crate::storage::Database;

// ============================================================================
// Byte array conversion helpers
// ============================================================================

/// Convert an `Option<Vec<u8>>` to an `Option<[u8; 32]>`.
///
/// Returns `None` if the vec is `None` or not exactly 32 bytes.
pub fn vec_to_array_32(v: Option<Vec<u8>>) -> Option<[u8; 32]> {
    v.and_then(|bytes| {
        if bytes.len() == 32 {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes);
            Some(arr)
        } else {
            None
        }
    })
}

/// Convert a `Vec<u8>` to `[u8; 32]`, returning a rusqlite error on size mismatch.
pub fn require_array_32(v: &[u8]) -> Result<[u8; 32], rusqlite::Error> {
    if v.len() != 32 {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(v);
    Ok(arr)
}

/// Convert a `Vec<u8>` to `[u8; 64]`, returning a rusqlite error on size mismatch.
pub fn require_array_64(v: &[u8]) -> Result<[u8; 64], rusqlite::Error> {
    if v.len() != 64 {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let mut arr = [0u8; 64];
    arr.copy_from_slice(v);
    Ok(arr)
}

// ============================================================================
// DateTime parsing
// ============================================================================

/// Parse an RFC3339 datetime string, falling back to `Utc::now()` on error.
pub fn parse_datetime(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

// ============================================================================
// Rate limiting
// ============================================================================

/// Get rate limit entry from the shared `anon_rate_limits` table.
pub fn get_rate_limit(
    db: &Database,
    entity_type: &str,
    entity_id: &[u8],
) -> Result<Option<AnonRateLimitEntry>, StorageError> {
    let conn = db.read_connection()?;
    let mut stmt = conn.prepare(
        "SELECT id, entity_type, entity_id, window_start, message_count, last_message_at
         FROM anon_rate_limits WHERE entity_type = ?1 AND entity_id = ?2",
    )?;

    let result = stmt.query_row(params![entity_type, entity_id], |row| {
        Ok(AnonRateLimitEntry {
            id: row.get(0)?,
            entity_type: row.get(1)?,
            entity_id: row.get(2)?,
            window_start: parse_datetime(&row.get::<_, String>(3)?),
            message_count: row.get::<_, i32>(4)? as u32,
            last_message_at: parse_datetime(&row.get::<_, String>(5)?),
        })
    });

    match result {
        Ok(entry) => Ok(Some(entry)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(StorageError::Database(e)),
    }
}

/// Increment rate limit counter, resetting window if expired.
///
/// Returns the new message count for the current window.
pub fn increment_rate_limit(
    db: &Database,
    entity_type: &str,
    entity_id: &[u8],
    window_duration: chrono::Duration,
) -> Result<u32, StorageError> {
    let conn = db.connection()?;
    let now = Utc::now();

    if let Some(entry) = get_rate_limit(db, entity_type, entity_id)? {
        if entry.is_window_expired(window_duration) {
            // Reset window
            conn.execute(
                "UPDATE anon_rate_limits SET window_start = ?1, message_count = 1, last_message_at = ?1
                 WHERE entity_type = ?2 AND entity_id = ?3",
                params![now.to_rfc3339(), entity_type, entity_id],
            )?;
            Ok(1)
        } else {
            // Increment counter
            let new_count = entry.message_count + 1;
            conn.execute(
                "UPDATE anon_rate_limits SET message_count = ?1, last_message_at = ?2
                 WHERE entity_type = ?3 AND entity_id = ?4",
                params![new_count as i32, now.to_rfc3339(), entity_type, entity_id],
            )?;
            Ok(new_count)
        }
    } else {
        // Create new entry
        let entry = AnonRateLimitEntry::new(entity_type, entity_id.to_vec());
        conn.execute(
            "INSERT INTO anon_rate_limits (id, entity_type, entity_id, window_start, message_count, last_message_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                entry.id,
                entry.entity_type,
                entry.entity_id,
                entry.window_start.to_rfc3339(),
                entry.message_count as i32,
                entry.last_message_at.to_rfc3339(),
            ],
        )?;
        Ok(1)
    }
}
