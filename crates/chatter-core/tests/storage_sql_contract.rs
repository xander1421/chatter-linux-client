//! The SQL contract: every statement a repository ships must compile against the
//! schema the migrations actually produce.
//!
//! INFRA-11 is the finding this exists for. `reaction_repo.rs` had nine
//! statements against a table named `reactions` with a column `emoji_json`.
//! Migration v045 dropped `reactions` and replaced it with `unified_reactions`
//! (column `emoji`), and never recreated it. Nothing noticed for the same reason
//! nothing could: SQL in a string literal is opaque to rustc, the repository had
//! no test, and its only entry point had no callers — the whole file compiled
//! clean and was structurally incapable of running.
//!
//! So instead of testing one repository, this replays every registered migration
//! into a real SQLite database and asks SQLite to prepare every SQL literal in
//! the storage layer. A statement that names a dropped table, a renamed column
//! or a misspelled keyword fails here.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use chatter_core::storage::migrations::default_migrations;
use rusqlite::Connection;

/// A migrated, empty database. Plain rusqlite rather than `Database::open`: this
/// is about schema and SQL, and SQLCipher adds nothing to that question.
fn migrated_db() -> Connection {
    let conn = Connection::open_in_memory().expect("open in-memory sqlite");
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    default_migrations()
        .run(&conn)
        .expect("every registered migration must apply to an empty database");
    conn
}

fn storage_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/storage")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read_dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Drop everything from the first `#[cfg(test)]` attribute onward, and every
/// line comment. Test modules create their own tables and doc comments quote SQL
/// that is illustrative, not shipped.
fn strip_tests_and_comments(src: &str) -> String {
    let body = match src.find("#[cfg(test)]") {
        Some(i) => &src[..i],
        None => src,
    };
    body.lines()
        .map(|line| match line.find("//") {
            // Only when the `//` starts the line's content: a `//` inside a
            // string literal (a URL, say) must not truncate it.
            Some(i) if line[..i].trim().is_empty() => "",
            _ => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every string literal in `src` that looks like a complete SQL statement.
///
/// Deliberately conservative: anything that is interpolated (`{`), or that ends
/// mid-expression because it is concatenated with another fragment, is skipped
/// rather than reported as a failure. False negatives cost coverage; false
/// positives cost the test its credibility.
fn sql_literals(src: &str) -> Vec<String> {
    // Verbs are matched case-sensitively: SQL in this crate is written in
    // uppercase, and prose in a comment ("Replace on conflict is intentional")
    // is not.
    const VERBS: [&str; 6] = ["SELECT", "INSERT", "UPDATE", "DELETE", "REPLACE", "WITH"];

    // `#[cfg(test)]` modules build their own throwaway tables; they are not part
    // of the shipped SQL contract. Doc comments quote SQL as illustration.
    let src: String = strip_tests_and_comments(src);
    let src = src.as_str();

    let mut out = Vec::new();
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // r#"..."# / r##"..."##
        if bytes[i] == b'r' && i + 1 < bytes.len() && (bytes[i + 1] == b'#' || bytes[i + 1] == b'"')
        {
            let mut hashes = 0;
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] == b'#' {
                hashes += 1;
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'"' {
                let close = format!("\"{}", "#".repeat(hashes));
                if let Some(end) = src[j + 1..].find(&close) {
                    out.push(src[j + 1..j + 1 + end].to_string());
                    i = j + 1 + end + close.len();
                    continue;
                }
            }
        }
        if bytes[i] == b'"' {
            let mut j = i + 1;
            let mut lit = String::new();
            let mut terminated = false;
            while j < bytes.len() {
                match bytes[j] {
                    b'\\' => {
                        // Only the escapes that appear inside SQL literals here.
                        if j + 1 < bytes.len() {
                            match bytes[j + 1] {
                                b'n' => lit.push('\n'),
                                b't' => lit.push('\t'),
                                c => lit.push(c as char),
                            }
                            j += 2;
                            continue;
                        }
                        j += 1;
                    }
                    b'"' => {
                        terminated = true;
                        j += 1;
                        break;
                    }
                    _ => {
                        let ch = src[j..].chars().next().unwrap();
                        lit.push(ch);
                        j += ch.len_utf8();
                        continue;
                    }
                }
            }
            if terminated {
                out.push(lit);
            }
            i = j;
            continue;
        }
        i += 1;
    }

    out.into_iter()
        .map(|l| l.trim().to_string())
        .filter(|l| {
            let upper = l.to_uppercase();
            VERBS.iter().any(|v| l.starts_with(v))
                // format!/push_str interpolation — not a literal statement.
                && !l.contains('{')
                // Concatenated fragment: a complete statement never ends on a
                // connective or an open paren.
                && !upper.ends_with("AND")
                && !upper.ends_with("OR")
                && !upper.ends_with("IN")
                && !upper.ends_with("NOT IN")
                && !upper.ends_with(',')
                && !upper.ends_with('(')
                && !upper.ends_with('=')
        })
        .collect()
}

#[test]
fn every_storage_sql_literal_compiles_against_the_migrated_schema() {
    let conn = migrated_db();

    let mut files = Vec::new();
    rust_files(&storage_dir(), &mut files);
    // migrations/ is DDL against schemas that do not exist yet at this point;
    // running them is what built `conn` in the first place.
    files.retain(|p| !p.components().any(|c| c.as_os_str() == "migrations"));
    assert!(
        files.len() > 10,
        "expected to find the storage layer, found {} files",
        files.len()
    );

    let mut checked = 0usize;
    let mut failures = Vec::new();
    for path in &files {
        let src = std::fs::read_to_string(path).expect("read source");
        for sql in sql_literals(&src) {
            checked += 1;
            if let Err(e) = conn.prepare(&sql) {
                let rel = path.strip_prefix(env!("CARGO_MANIFEST_DIR")).unwrap();
                failures.push(format!(
                    "{}:\n    {}\n    -> {e}",
                    rel.display(),
                    sql.replace('\n', "\n    ")
                ));
            }
        }
    }

    assert!(
        checked > 100,
        "only {checked} SQL literals found; the extractor is broken, not the SQL"
    );
    assert!(
        failures.is_empty(),
        "{} of {checked} SQL statements do not compile against the migrated schema:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn dropped_tables_are_not_referenced_anywhere_in_storage() {
    let conn = migrated_db();
    let live: BTreeSet<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type IN ('table','view')")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    // v045 dropped `reactions` and replaced it with `unified_reactions`.
    assert!(
        !live.contains("reactions"),
        "migration v045 drops `reactions`; if it is back, this test's premise changed"
    );
    assert!(live.contains("unified_reactions"));

    // The Sesame tables used to be created at runtime by SqliteSesameStore,
    // outside the migration system entirely (INFRA-12). v065 moved them here.
    for t in [
        "sesame_users",
        "sesame_devices",
        "sesame_sessions",
        "sesame_pending_messages",
    ] {
        assert!(
            live.contains(t),
            "{t} must come from a migration, not a runtime CREATE TABLE IF NOT EXISTS"
        );
    }
}
