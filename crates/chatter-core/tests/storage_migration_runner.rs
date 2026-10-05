//! The migration runner's own contract (INFRA-26).
//!
//! `run()` used to take `MAX(version)` as a high-water mark and apply only
//! `version > current`. Versions 8, 14 and 43 are free in the registered
//! sequence, so a migration back-filled into any of those slots would have run
//! on a fresh install and been silently skipped on every existing one -- two
//! databases with the same `schema_version` max and different schemas, and
//! nothing to report it. The watermark query also ended in `.unwrap_or(0)`,
//! which turns any read failure into "nothing applied" and replays from v001
//! over a populated database.

use chatter_core::storage::migrations::{Migration, MigrationRunner, default_migrations};
use rusqlite::Connection;

struct Fake(u32, &'static str, &'static str);

impl Migration for Fake {
    fn version(&self) -> u32 {
        self.0
    }
    fn description(&self) -> &'static str {
        self.1
    }
    fn up(&self) -> &'static str {
        self.2
    }
}

fn applied(conn: &Connection) -> Vec<u32> {
    let mut stmt = conn
        .prepare("SELECT version FROM schema_version ORDER BY version")
        .unwrap();
    stmt.query_map([], |r| r.get::<_, u32>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name = ?1",
        [name],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        > 0
}

#[test]
fn a_migration_back_filled_into_a_gap_is_applied_not_skipped() {
    let conn = Connection::open_in_memory().unwrap();

    // A database that has already run 1 and 3, leaving 2 free.
    MigrationRunner::new()
        .register(Fake(1, "one", "CREATE TABLE one (id INTEGER);"))
        .register(Fake(3, "three", "CREATE TABLE three (id INTEGER);"))
        .run(&conn)
        .unwrap();
    assert_eq!(applied(&conn), vec![1, 3]);

    // Now v002 is written and shipped. MAX(version) is 3, so a watermark runner
    // would decide 2 is in the past and never run it.
    MigrationRunner::new()
        .register(Fake(1, "one", "CREATE TABLE one (id INTEGER);"))
        .register(Fake(2, "two", "CREATE TABLE two (id INTEGER);"))
        .register(Fake(3, "three", "CREATE TABLE three (id INTEGER);"))
        .run(&conn)
        .unwrap();

    assert!(
        table_exists(&conn, "two"),
        "a migration numbered below the current maximum was silently skipped"
    );
    assert_eq!(applied(&conn), vec![1, 2, 3]);
}

#[test]
fn already_applied_migrations_are_not_re_run() {
    let conn = Connection::open_in_memory().unwrap();
    // Not idempotent on purpose: a second application must fail loudly if it
    // happens at all.
    let build =
        || MigrationRunner::new().register(Fake(1, "one", "CREATE TABLE one (id INTEGER);"));
    build().run(&conn).unwrap();
    build().run(&conn).expect("second run must be a no-op");
    assert_eq!(applied(&conn), vec![1]);
}

#[test]
fn a_failing_migration_leaves_no_partial_schema_and_is_not_recorded() {
    let conn = Connection::open_in_memory().unwrap();
    let err = MigrationRunner::new()
        .register(Fake(
            1,
            "half-broken",
            "CREATE TABLE good (id INTEGER); CREATE TABLE bad (id INTEGER, ;",
        ))
        .run(&conn);
    assert!(err.is_err());
    assert!(!table_exists(&conn, "good"), "partial migration committed");
    assert_eq!(applied(&conn), Vec::<u32>::new());
}

#[test]
fn every_registered_version_is_unique_and_matches_what_was_applied() {
    let conn = Connection::open_in_memory().unwrap();
    default_migrations().run(&conn).unwrap();

    let versions = applied(&conn);
    let mut sorted = versions.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        versions.len(),
        "two migrations share a version number"
    );
    assert!(
        versions.len() >= 60,
        "expected the full registered set, got {}",
        versions.len()
    );

    // Running the whole set again must change nothing.
    default_migrations().run(&conn).unwrap();
    assert_eq!(applied(&conn), versions);
}
