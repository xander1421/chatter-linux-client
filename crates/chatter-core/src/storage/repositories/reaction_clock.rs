//! Durable actor Lamport clock for outgoing reactions.
//!
//! LWW compares signed-SQL-safe nanosecond versions, with storage's deterministic
//! tie rules for concurrent devices. Allocation advances above wall time, every
//! retained observed actor version and earlier allocations (including failed
//! sends). Backwards clocks and observed future skew therefore cannot prevent a
//! new local selection. Unobserved actions on unsynchronized devices have no
//! guaranteed real-time intent order. Never substitute transport completion time.
use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

impl UnifiedMessageRepository {
    pub fn allocate_reaction_clock(&self, reactor: &UserId) -> Result<u64, StorageError> {
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let wall = u64::try_from(wall)
            .map_err(|_| StorageError::InvalidData("Reaction clock overflow".into()))?;
        let mut conn = self.db.connection()?;
        allocate_with_conn(&mut conn, &reactor.to_string(), wall)
    }
}

fn allocate_with_conn(
    conn: &mut rusqlite::Connection,
    actor: &str,
    wall: u64,
) -> Result<u64, StorageError> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    // Local allocator state is deliberately retained across deletion of targets.
    // It is not a projection and must not be reset by snapshot/import operations.
    tx.execute_batch("CREATE TABLE IF NOT EXISTS reaction_send_clocks(actor_id TEXT PRIMARY KEY, action_ts INTEGER NOT NULL CHECK(action_ts>0))")?;
    let known: i64 = tx.query_row(
        "SELECT COALESCE(MAX(action_ts),0) FROM (
            SELECT action_ts FROM reaction_send_clocks WHERE actor_id=?1
            UNION ALL SELECT action_ts FROM reaction_action_versions WHERE actor_id=?1
            UNION ALL SELECT action_ts FROM reaction_actor_selections WHERE actor_id=?1
            UNION ALL SELECT action_ts FROM pending_scoped_reactions WHERE actor_id=?1
        )",
        [actor],
        |row| row.get(0),
    )?;
    let next = next_clock(wall, known)?;
    tx.execute("INSERT INTO reaction_send_clocks(actor_id,action_ts) VALUES(?1,?2) ON CONFLICT(actor_id) DO UPDATE SET action_ts=excluded.action_ts", params![actor, next as i64])?;
    tx.commit()?;
    Ok(next)
}

fn next_clock(wall: u64, known: i64) -> Result<u64, StorageError> {
    let next = wall.max(
        (known.max(0) as u64)
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("Reaction clock overflow".into()))?,
    );
    if next > i64::MAX as u64 {
        return Err(StorageError::InvalidData(
            "Reaction clock exhausted signed SQL range".into(),
        ));
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE reaction_action_versions(actor_id TEXT,action_ts INTEGER);
            CREATE TABLE reaction_actor_selections(actor_id TEXT,action_ts INTEGER);
            CREATE TABLE pending_scoped_reactions(actor_id TEXT,action_ts INTEGER);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn allocator_retains_failed_send_clock_and_observed_skew() {
        let mut conn = db();
        assert_eq!(allocate_with_conn(&mut conn, "me", 100).unwrap(), 100);
        // No persisted vote: a failed dispatch still consumes its allocation.
        assert_eq!(allocate_with_conn(&mut conn, "me", 10).unwrap(), 101);
        conn.execute("INSERT INTO pending_scoped_reactions VALUES('me',500)", [])
            .unwrap();
        assert_eq!(allocate_with_conn(&mut conn, "me", 0).unwrap(), 501);
        conn.execute("INSERT INTO reaction_action_versions VALUES('me',700)", [])
            .unwrap();
        assert_eq!(allocate_with_conn(&mut conn, "me", 2).unwrap(), 701);
        // Recreating the repository does not reset the database allocator.
        assert_eq!(allocate_with_conn(&mut conn, "me", 1).unwrap(), 702);
        assert_eq!(allocate_with_conn(&mut conn, "other", 1).unwrap(), 1);
    }

    #[test]
    fn allocator_survives_file_close_reopen_and_target_evidence_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("allocator.db");
        {
            let mut conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE reaction_action_versions(actor_id TEXT,action_ts INTEGER);
                CREATE TABLE reaction_actor_selections(actor_id TEXT,action_ts INTEGER);
                CREATE TABLE pending_scoped_reactions(actor_id TEXT,action_ts INTEGER);
                INSERT INTO reaction_actor_selections VALUES('me',800);",
            )
            .unwrap();
            assert_eq!(allocate_with_conn(&mut conn, "me", 100).unwrap(), 801);
            // Failed send has no vote; allocated version must still survive.
        }
        {
            let mut reopened = rusqlite::Connection::open(&path).unwrap();
            reopened
                .execute("DELETE FROM reaction_actor_selections", [])
                .unwrap();
            assert_eq!(allocate_with_conn(&mut reopened, "me", 0).unwrap(), 802);
        }
        let mut reopened = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(allocate_with_conn(&mut reopened, "me", 1).unwrap(), 803);
        assert_eq!(allocate_with_conn(&mut reopened, "other", 1).unwrap(), 1);
    }

    #[test]
    fn clock_exhaustion_does_not_wrap_or_overwrite() {
        let mut conn = db();
        assert_eq!(
            allocate_with_conn(&mut conn, "me", i64::MAX as u64).unwrap(),
            i64::MAX as u64
        );
        assert!(allocate_with_conn(&mut conn, "me", 0).is_err());
        let retained: i64 = conn
            .query_row(
                "SELECT action_ts FROM reaction_send_clocks WHERE actor_id='me'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(retained, i64::MAX);
        assert!(next_clock(u64::MAX, 0).is_err());
    }
}
