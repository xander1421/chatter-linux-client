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

/// Decode each row independently. Query/iteration errors still fail the query;
/// a malformed stored entity only removes that entity from the result. Never
/// format a decoder error here: JSON errors can contain stored message text.
pub(crate) fn collect_decoded_rows<T>(
    rows: &mut rusqlite::Rows<'_>,
    entity: &'static str,
    decode: impl Fn(&rusqlite::Row<'_>) -> Result<T, StorageError>,
) -> Result<Vec<T>, StorageError> {
    let mut decoded = Vec::new();
    while let Some(row) = rows.next()? {
        match decode(row) {
            Ok(value) => decoded.push(value),
            Err(error) => log_invalid_row(row, entity, &error),
        }
    }
    Ok(decoded)
}

pub(super) fn log_invalid_row(row: &rusqlite::Row<'_>, entity: &'static str, error: &StorageError) {
    let row_id = row
        .get::<_, String>(0)
        .or_else(|_| row.get::<_, i64>(0).map(|id| id.to_string()))
        .unwrap_or_else(|_| "<unreadable>".into());
    let error_class = match error {
        StorageError::InvalidData(_) => "invalid_data",
        StorageError::Database(_) => "row_decode",
        StorageError::ConnectionPool(_) => "connection_pool",
        StorageError::Migration(_) => "migration",
        StorageError::NotFound(_) => "not_found",
        StorageError::Encryption(_) => "encryption",
    };
    tracing::warn!(
        entity,
        row_id,
        error_class,
        "Skipping undecodable stored row"
    );
}

#[cfg(test)]
mod corrupt_row_tests {
    use super::*;

    #[test]
    fn corrupt_blob_is_skipped_without_hiding_later_rows() {
        use crate::storage::serialization::{db_deserialize, db_serialize};
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE fixture(id TEXT, blob BLOB)")
            .unwrap();
        for (id, blob) in [
            ("before", db_serialize(&vec![1u32]).unwrap()),
            ("bad", vec![0, 1]),
            ("after", db_serialize(&vec![2u32]).unwrap()),
        ] {
            conn.execute("INSERT INTO fixture VALUES (?1, ?2)", params![id, blob])
                .unwrap();
        }
        let mut stmt = conn
            .prepare("SELECT id, blob FROM fixture ORDER BY rowid")
            .unwrap();
        let result = collect_decoded_rows(&mut stmt.query([]).unwrap(), "fixture", |row| {
            let blob: Vec<u8> = row.get(1)?;
            db_deserialize::<Vec<u32>>(&blob)
        })
        .unwrap();
        assert_eq!(result, vec![vec![1], vec![2]]);
    }
}
