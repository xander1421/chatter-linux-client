//! Call history repository.
//!
//! Local-only record of past calls. Written by `call_history_recorder` on
//! observable orchestrator events (ring / accept / decline / end / missed)
//! and by `ChatterClient::start_call` for the outgoing side; read by the
//! Calls screen. No server sync yet — the server exposes no call-history RPC.
//!
//! Table defined in migration v053 (`call_history`).

use crate::domain::call::{CallDirection, CallHistoryEntry, CallHistoryFilter, CallResult};
use crate::domain::message::ConversationType;
use crate::error::StorageError;
use crate::storage::Database;
use crate::storage::repositories::SettingsRepository;
use chrono::{DateTime, Utc};
use rusqlite::{params, params_from_iter, types::Value as SqlValue};
use std::sync::Arc;

/// `settings` key holding when the Calls screen was last opened (RFC3339).
const MISSED_SEEN_AT_KEY: &str = "calls.missed_seen_at";

/// Local call history data repository.
///
/// All methods are synchronous and block the current thread briefly for the
/// SQLite write lock. Safe to call from any thread — the underlying
/// `Database` serializes writers and pools readers.
pub struct CallHistoryRepository {
    db: Arc<Database>,
}

impl CallHistoryRepository {
    /// Create a new `CallHistoryRepository`.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    // ========================================================================
    // Writes
    // ========================================================================

    /// Insert a new call history entry, or replace an existing one with the
    /// same id. "Replace on conflict" is intentional: signaling often emits
    /// the same call through multiple paths (offer, accept, end) and we want
    /// the most recent write to win.
    pub fn insert_entry(&self, entry: &CallHistoryEntry) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT INTO call_history (
                id, conversation_id, conversation_type, peer_name,
                started_at, ended_at, direction, result, duration_secs, has_video
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET
                 conversation_id = excluded.conversation_id,
                 conversation_type = excluded.conversation_type,
                 peer_name = excluded.peer_name,
                 started_at = excluded.started_at,
                 ended_at = excluded.ended_at,
                 direction = excluded.direction,
                 result = excluded.result,
                 duration_secs = excluded.duration_secs,
                 has_video = excluded.has_video",
            params![
                entry.id,
                entry.conversation_id,
                entry.conversation_type.as_str(),
                entry.peer_name,
                entry.started_at.to_rfc3339(),
                entry.ended_at.map(|dt| dt.to_rfc3339()),
                entry.direction.as_str(),
                entry.result.as_str(),
                entry.duration_secs,
                entry.has_video as i64,
            ],
        )?;
        Ok(())
    }

    /// Close out a call that CONNECTED: `ended_at = now`, the duration, and
    /// `result = 'completed'`. A connected call was answered, so this
    /// overrides the pessimistic `'missed'` an incoming row starts as.
    pub fn mark_completed(&self, id: &str, duration_secs: i64) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now().to_rfc3339();
        let rows = conn.execute(
            "UPDATE call_history
             SET ended_at = ?1,
                 duration_secs = ?2,
                 result = 'completed'
             WHERE id = ?3",
            params![now, duration_secs, id],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!(
                "Call history entry {} not found",
                id
            )));
        }
        Ok(())
    }

    /// Close out a call that never connected: stamp `ended_at`, leave
    /// `duration_secs` NULL, and record the terminal `result`.
    ///
    /// The other half of [`mark_completed`](Self::mark_completed), which is
    /// only for a call that connected.
    pub fn mark_unconnected(&self, id: &str, result: CallResult) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE call_history
             SET ended_at = ?1, duration_secs = NULL, result = ?2
             WHERE id = ?3",
            params![Utc::now().to_rfc3339(), result.as_str(), id],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!(
                "Call history entry {} not found",
                id
            )));
        }
        Ok(())
    }

    /// Delete a single entry by id. Silently succeeds if the row is absent —
    /// the UI's "swipe to delete" should be idempotent.
    pub fn delete(&self, id: &str) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute("DELETE FROM call_history WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Delete every call history entry.
    ///
    /// Returns the number of rows deleted.
    pub fn delete_all(&self) -> Result<u64, StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute("DELETE FROM call_history", [])?;
        Ok(rows as u64)
    }

    // ========================================================================
    // Reads
    // ========================================================================

    /// List call history entries matching `filter`, newest first.
    ///
    /// Pagination uses a `(started_at, id)` cursor: callers pass the
    /// `started_at` of the last row they saw as `cursor_started_at`. Rows
    /// strictly older than that are returned. `id` is used as a tiebreaker
    /// so duplicate-timestamp rows don't get lost. Pass `None` for the
    /// first page.
    pub fn list(
        &self,
        filter: CallHistoryFilter,
        limit: u32,
        cursor: Option<(DateTime<Utc>, String)>,
    ) -> Result<Vec<CallHistoryEntry>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut sql = String::from(
            "SELECT id, conversation_id, conversation_type, peer_name,
                    started_at, ended_at, direction, result, duration_secs, has_video
             FROM call_history
             WHERE 1=1",
        );
        let mut args: Vec<SqlValue> = Vec::new();

        match filter {
            CallHistoryFilter::All => {}
            CallHistoryFilter::Missed => {
                // An unanswered call we placed is stored as 'missed' too;
                // the filter means calls the user missed.
                sql.push_str(" AND result = 'missed' AND direction = 'incoming'");
            }
            CallHistoryFilter::Outgoing => {
                sql.push_str(" AND direction = 'outgoing'");
            }
            CallHistoryFilter::Incoming => {
                sql.push_str(" AND direction = 'incoming'");
            }
        }

        if let Some((cursor_ts, cursor_id)) = cursor {
            // Strictly-older-than cursor with id tiebreaker.
            sql.push_str(
                " AND (started_at < ?\
                       OR (started_at = ? AND id < ?))",
            );
            let ts_str = cursor_ts.to_rfc3339();
            args.push(SqlValue::Text(ts_str.clone()));
            args.push(SqlValue::Text(ts_str));
            args.push(SqlValue::Text(cursor_id));
        }

        sql.push_str(" ORDER BY started_at DESC, id DESC LIMIT ?");
        args.push(SqlValue::Integer(limit as i64));

        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(params_from_iter(args.iter()))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(row_to_entry(row)?);
        }
        Ok(out)
    }

    /// One entry by id.
    pub fn get(&self, id: &str) -> Result<Option<CallHistoryEntry>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, conversation_id, conversation_type, peer_name,
                    started_at, ended_at, direction, result, duration_secs, has_video
             FROM call_history WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![id])?;
        rows.next()?.map(row_to_entry).transpose()
    }

    /// Total number of entries. Cheap — uses SQLite's cached index count.
    pub fn count(&self) -> Result<u64, StorageError> {
        let conn = self.db.read_connection()?;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM call_history", [], |row| row.get(0))?;
        Ok(n as u64)
    }

    /// Number of UNSEEN missed incoming calls — the Calls tab badge.
    ///
    /// Outgoing rows are excluded: an unanswered call we placed is stored as
    /// `'missed'` too, but it is nothing the user missed. "Unseen" means it
    /// ended after the last [`mark_missed_seen`](Self::mark_missed_seen); a
    /// row still ringing (no `ended_at`) is judged by when it started.
    pub fn missed_count(&self) -> Result<u64, StorageError> {
        let seen_at = SettingsRepository::new(self.db.clone())
            .get_string(MISSED_SEEN_AT_KEY)?
            .unwrap_or_default();
        let conn = self.db.read_connection()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM call_history
             WHERE result = 'missed' AND direction = 'incoming'
               AND (?1 = '' OR julianday(COALESCE(ended_at, started_at)) > julianday(?1))",
            params![seen_at],
            |row| row.get(0),
        )?;
        Ok(n as u64)
    }

    /// The user has looked at the Calls screen: every missed call so far is
    /// seen, so [`missed_count`](Self::missed_count) drops to zero.
    pub fn mark_missed_seen(&self) -> Result<(), StorageError> {
        SettingsRepository::new(self.db.clone())
            .set_string(MISSED_SEEN_AT_KEY, &Utc::now().to_rfc3339())
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn parse_datetime(s: &str) -> Result<DateTime<Utc>, StorageError> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| StorageError::InvalidData(format!("Invalid timestamp '{}': {}", s, e)))
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> Result<CallHistoryEntry, StorageError> {
    let id: String = row.get(0)?;
    let conversation_id: String = row.get(1)?;
    let conversation_type_str: String = row.get(2)?;
    let peer_name: String = row.get(3)?;
    let started_at_str: String = row.get(4)?;
    let ended_at_str: Option<String> = row.get(5)?;
    let direction_str: String = row.get(6)?;
    let result_str: String = row.get(7)?;
    let duration_secs: Option<i64> = row.get(8)?;
    let has_video_i: i64 = row.get(9)?;

    let conversation_type = ConversationType::from_str_value(&conversation_type_str)?;
    let direction = CallDirection::from_str(&direction_str).ok_or_else(|| {
        StorageError::InvalidData(format!("Invalid direction '{}'", direction_str))
    })?;
    let result = CallResult::from_str(&result_str)
        .ok_or_else(|| StorageError::InvalidData(format!("Invalid result '{}'", result_str)))?;

    let started_at = parse_datetime(&started_at_str)?;
    let ended_at = match ended_at_str {
        Some(s) => Some(parse_datetime(&s)?),
        None => None,
    };

    Ok(CallHistoryEntry {
        id,
        conversation_id,
        conversation_type,
        peer_name,
        started_at,
        ended_at,
        direction,
        result,
        duration_secs,
        has_video: has_video_i != 0,
    })
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_db() -> Arc<Database> {
        // Follow the pattern from search_index tests: tempdir + SQLCipher.
        // SQLCipher's :memory: handling requires a passphrase after attach,
        // and the existing `Database::open` assumes a real path — so a temp
        // file on disk is the closest we can get to in-memory without
        // refactoring `Database`. The temp dir lives until the test ends.
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("call_history_test.db");
        let db = Arc::new(Database::open(&db_path, "testpass").unwrap());
        db.run_migrations().unwrap();
        crate::test_tempdir::hold(temp_dir);
        db
    }

    fn sample_entry(id: &str, started_at: DateTime<Utc>) -> CallHistoryEntry {
        CallHistoryEntry {
            id: id.to_string(),
            conversation_id: "peer-1".to_string(),
            conversation_type: ConversationType::ServerDm,
            peer_name: "Alice".to_string(),
            started_at,
            ended_at: None,
            direction: CallDirection::Outgoing,
            result: CallResult::Completed,
            duration_secs: Some(42),
            has_video: false,
        }
    }

    #[test]
    fn insert_and_list_roundtrip() {
        let db = setup_db();
        let repo = CallHistoryRepository::new(db);

        let e1 = sample_entry("call-1", Utc::now());
        repo.insert_entry(&e1).unwrap();

        let rows = repo.list(CallHistoryFilter::All, 10, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "call-1");
        assert_eq!(rows[0].peer_name, "Alice");
        assert_eq!(rows[0].duration_secs, Some(42));
        assert_eq!(rows[0].result, CallResult::Completed);
        assert_eq!(
            repo.get("call-1").unwrap().map(|e| e.peer_name),
            Some("Alice".into())
        );
        assert!(repo.get("call-2").unwrap().is_none());
    }

    #[test]
    fn insert_replaces_on_conflict() {
        let db = setup_db();
        let repo = CallHistoryRepository::new(db);

        let t = Utc::now();
        let mut e = sample_entry("call-1", t);
        e.peer_name = "Alice".into();
        repo.insert_entry(&e).unwrap();

        e.peer_name = "Alice Renamed".into();
        repo.insert_entry(&e).unwrap();

        let rows = repo.list(CallHistoryFilter::All, 10, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].peer_name, "Alice Renamed");
    }

    #[test]
    fn list_newest_first_and_missed_filter() {
        let db = setup_db();
        let repo = CallHistoryRepository::new(db);

        let t0 = Utc::now();
        let mut e_old = sample_entry("call-old", t0);
        e_old.started_at = t0 - chrono::Duration::minutes(10);
        repo.insert_entry(&e_old).unwrap();

        let mut e_mid = sample_entry("call-mid", t0);
        e_mid.started_at = t0 - chrono::Duration::minutes(5);
        e_mid.result = CallResult::Missed;
        e_mid.direction = CallDirection::Incoming;
        e_mid.duration_secs = None;
        repo.insert_entry(&e_mid).unwrap();

        let mut e_new = sample_entry("call-new", t0);
        e_new.started_at = t0;
        repo.insert_entry(&e_new).unwrap();

        let all = repo.list(CallHistoryFilter::All, 10, None).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].id, "call-new");
        assert_eq!(all[1].id, "call-mid");
        assert_eq!(all[2].id, "call-old");

        let missed = repo.list(CallHistoryFilter::Missed, 10, None).unwrap();
        assert_eq!(missed.len(), 1);
        assert_eq!(missed[0].id, "call-mid");
        assert_eq!(missed[0].result, CallResult::Missed);

        let incoming = repo.list(CallHistoryFilter::Incoming, 10, None).unwrap();
        assert_eq!(incoming.len(), 1);
        assert_eq!(incoming[0].id, "call-mid");

        let outgoing = repo.list(CallHistoryFilter::Outgoing, 10, None).unwrap();
        assert_eq!(outgoing.len(), 2);

        assert_eq!(repo.count().unwrap(), 3);
        assert_eq!(repo.missed_count().unwrap(), 1);
    }

    #[test]
    fn badge_counts_only_unseen_missed_incoming() {
        let db = setup_db();
        let repo = CallHistoryRepository::new(db);
        let t = Utc::now() - chrono::Duration::minutes(1);

        // Our own unanswered call: stored 'missed', but not a missed call.
        let mut out = sample_entry("out-unanswered", t);
        out.result = CallResult::Missed;
        out.duration_secs = None;
        repo.insert_entry(&out).unwrap();

        let mut inc = out.clone();
        inc.id = "in-missed".into();
        inc.direction = CallDirection::Incoming;
        repo.insert_entry(&inc).unwrap();

        assert_eq!(repo.missed_count().unwrap(), 1);
        let missed = repo.list(CallHistoryFilter::Missed, 10, None).unwrap();
        assert_eq!(missed.len(), 1);
        assert_eq!(missed[0].id, "in-missed");

        // Opening the Calls screen clears it...
        repo.mark_missed_seen().unwrap();
        assert_eq!(repo.missed_count().unwrap(), 0);

        // ...and a call missed afterwards counts again.
        let mut later = inc.clone();
        later.id = "in-missed-later".into();
        later.started_at = Utc::now() + chrono::Duration::seconds(1);
        repo.insert_entry(&later).unwrap();
        assert_eq!(repo.missed_count().unwrap(), 1);
    }

    #[test]
    fn pagination_cursor_is_strictly_older() {
        let db = setup_db();
        let repo = CallHistoryRepository::new(db);

        let t0 = Utc::now();
        for i in 0..5 {
            let mut e = sample_entry(&format!("call-{}", i), t0);
            e.started_at = t0 - chrono::Duration::minutes(i);
            repo.insert_entry(&e).unwrap();
        }

        // Page 1: 2 newest.
        let p1 = repo.list(CallHistoryFilter::All, 2, None).unwrap();
        assert_eq!(p1.len(), 2);
        assert_eq!(p1[0].id, "call-0");
        assert_eq!(p1[1].id, "call-1");

        // Page 2: next 2, strictly older than page 1's last.
        let cursor = (p1[1].started_at, p1[1].id.clone());
        let p2 = repo.list(CallHistoryFilter::All, 2, Some(cursor)).unwrap();
        assert_eq!(p2.len(), 2);
        assert_eq!(p2[0].id, "call-2");
        assert_eq!(p2[1].id, "call-3");

        // Page 3: just the tail.
        let cursor = (p2[1].started_at, p2[1].id.clone());
        let p3 = repo.list(CallHistoryFilter::All, 2, Some(cursor)).unwrap();
        assert_eq!(p3.len(), 1);
        assert_eq!(p3[0].id, "call-4");
    }

    #[test]
    fn mark_completed_sets_duration_and_result() {
        let db = setup_db();
        let repo = CallHistoryRepository::new(db);

        let t = Utc::now();
        let mut e = sample_entry("call-live", t);
        e.ended_at = None;
        e.duration_secs = None;
        e.result = CallResult::Completed; // pre-populated in-progress
        repo.insert_entry(&e).unwrap();

        repo.mark_completed("call-live", 125).unwrap();

        let rows = repo.list(CallHistoryFilter::All, 10, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].ended_at.is_some());
        assert_eq!(rows[0].duration_secs, Some(125));
        assert_eq!(rows[0].result, CallResult::Completed);
    }

    #[test]
    fn mark_completed_overrides_the_pessimistic_missed() {
        let db = setup_db();
        let repo = CallHistoryRepository::new(db);

        let t = Utc::now();
        let mut e = sample_entry("call-missed", t);
        e.result = CallResult::Missed;
        e.duration_secs = None;
        repo.insert_entry(&e).unwrap();

        // An incoming row starts as missed; answering and talking makes it
        // a completed call.
        repo.mark_completed("call-missed", 11).unwrap();
        let rows = repo.list(CallHistoryFilter::All, 10, None).unwrap();
        assert_eq!(rows[0].result, CallResult::Completed);
        assert_eq!(rows[0].duration_secs, Some(11));
    }

    #[test]
    fn mark_completed_missing_row_errors() {
        let db = setup_db();
        let repo = CallHistoryRepository::new(db);

        let err = repo.mark_completed("nope", 10).unwrap_err();
        matches!(err, StorageError::NotFound(_));
    }

    #[test]
    fn delete_and_delete_all() {
        let db = setup_db();
        let repo = CallHistoryRepository::new(db);

        let t0 = Utc::now();
        for i in 0..3 {
            let mut e = sample_entry(&format!("call-{}", i), t0);
            e.started_at = t0 - chrono::Duration::minutes(i);
            repo.insert_entry(&e).unwrap();
        }
        assert_eq!(repo.count().unwrap(), 3);

        repo.delete("call-1").unwrap();
        assert_eq!(repo.count().unwrap(), 2);

        // idempotent: deleting a missing row is fine
        repo.delete("nonexistent").unwrap();

        let n = repo.delete_all().unwrap();
        assert_eq!(n, 2);
        assert_eq!(repo.count().unwrap(), 0);
    }

    #[test]
    fn missed_call_path_end_to_end() {
        let db = setup_db();
        let repo = CallHistoryRepository::new(db);

        // Simulate: incoming call arrives, rings, never answered -> missed.
        let t = Utc::now();
        let entry = CallHistoryEntry {
            id: "ring-42".to_string(),
            conversation_id: "peer-bob".to_string(),
            conversation_type: ConversationType::ServerDm,
            peer_name: "Bob".to_string(),
            started_at: t,
            ended_at: Some(t + chrono::Duration::seconds(30)),
            direction: CallDirection::Incoming,
            result: CallResult::Missed,
            duration_secs: None,
            has_video: true,
        };
        repo.insert_entry(&entry).unwrap();

        let rows = repo.list(CallHistoryFilter::Missed, 10, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "ring-42");
        assert_eq!(rows[0].direction, CallDirection::Incoming);
        assert_eq!(rows[0].result, CallResult::Missed);
        assert_eq!(rows[0].duration_secs, None);
        assert!(rows[0].has_video);
    }

    /// Every conversation type that can host a call must survive the round
    /// trip — the `conversation_type` CHECK constraint and `from_str_value`
    /// are two separate vocabularies and this is what pins them together.
    #[test]
    fn every_call_capable_conversation_type_roundtrips() {
        let db = setup_db();
        let repo = CallHistoryRepository::new(db);

        let t0 = Utc::now();
        let types = [
            ConversationType::Channel,
            ConversationType::ServerDm,
            ConversationType::AnonDm,
            ConversationType::AnonGroup,
        ];
        for (i, ct) in types.iter().enumerate() {
            let mut e = sample_entry(ct.as_str(), t0 - chrono::Duration::minutes(i as i64));
            e.conversation_type = ct.clone();
            repo.insert_entry(&e).unwrap();
        }

        let rows = repo.list(CallHistoryFilter::All, 10, None).unwrap();
        assert_eq!(rows.len(), types.len());
        for ct in &types {
            assert!(
                rows.iter().any(|r| &r.conversation_type == ct),
                "{ct:?} lost"
            );
        }
    }
}
