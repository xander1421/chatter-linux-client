//! Message-history backup payload: frame schema + dump / import.
//!
//! The backup payload (the plaintext that `crypto::message_backup` seals) is a
//! stream of length-delimited prost [`Frame`]s:
//!
//! ```text
//! Frame{Info}                          — exactly one, first
//! Frame{Table{name, columns}}          — starts a table section
//! Frame{Row{values…}}                  — one per row, columns as in Table
//! …repeat Table/Row sections…
//! ```
//!
//! # What is IN the backup
//!
//! Selective, Signal-style: user-visible history and social graph only.
//! - `messages` / `threads` / `unified_reactions` — full history including
//!   pins (`is_pinned`), edits (`is_edited`/`edited_at`) and reaction state.
//! - `attachments` referenced by those messages, as pointers (CDN key, key,
//!   digest, metadata) — never the bytes or this device's file paths. Without
//!   them a restored or linked device had attachment ids it could not
//!   describe or fetch: a GIF previewed as "📎 1 attachment" (§138).
//! - `channels` / `memberships` — conversation metadata (channels, server
//!   DMs, saved-messages).
//! - `users` / `profiles` / `friendships` — contact + friend list.
//! - own profile: the `settings` rows for display name and avatar bytes.
//!
//! # What is OUT (deliberately)
//!
//! - Signal ratchet/session state, identity keys, prekeys, sender keys —
//!   restoring session state onto another install corrupts ratchets; peers
//!   re-establish via the normal session-establish / V47 self-heal paths.
//!   (Identity + profile key travel via the separate SVR key backup.)
//! - `dm_mailboxes` / anon mailbox tables — our mailbox keypairs are
//!   transport secrets in the same class as sessions.
//! - Anon DM / anon group / mailbox-DM conversations — anonymity- and
//!   mailbox-scoped state tied to those mailbox keys; such a conversation is
//!   re-established, not restored. Their message rows are excluded with them
//!   (a restored row without its conversation would be invisible anyway).
//! - `settings` beyond the own-profile keys — auth tokens and `crypto.*`
//!   material must never leave the device in a history backup.
//!
//! # Idempotent import
//!
//! Inserts skip uniqueness conflicts: a row whose primary key (or dedup
//! unique index) already exists locally is skipped. That makes import safe to
//! re-run and — deliberately — means import NEVER clobbers newer local data:
//! Generic rows use skip-if-exists. Reaction evidence for actors absent locally
//! is merged deterministically within the payload before rebuilding projections;
//! locally retained actor authority wins over restored state across all emojis.
//! A payload with tables or columns unknown to this build is rejected before
//! the transaction commits, so restore never silently loses history.

use prost::Message;
use prost::bytes::Buf;
use rusqlite::Connection;
use rusqlite::types::{Value as SqlValue, ValueRef};

use crate::error::StorageError;
use crate::storage::repositories::settings_repo::keys;
use crate::storage::repositories::unified_message_repo::reaction_projection;
use std::collections::HashSet;

/// Payload schema version (independent of the container's format_version).
/// Bump this for every change to backup content, including exported tables,
/// columns, row selection, or the meaning of stored values. Older builds must
/// reject newer payloads rather than silently importing only familiar fields.
pub const PAYLOAD_VERSION: u32 = 1;

/// Whitelisted tables, in FK-safe import order (parents before children:
/// `profiles`/`friendships` reference `users`, `memberships` references
/// `channels`, `threads` references `messages`).
pub const BACKUP_TABLES: &[&str] = &[
    "users",
    "profiles",
    "friendships",
    "channels",
    "memberships",
    "messages",
    "attachments",
    "threads",
    "unified_reactions",
    "pending_scoped_reactions",
    "reaction_action_versions",
    "reaction_actor_selections",
    "pending_reaction_add_selections",
];

/// Own-profile settings rows included in the backup (see module doc — no
/// auth/crypto keys, ever).
pub const OWN_PROFILE_SETTINGS_KEYS: &[&str] = &[keys::DISPLAY_NAME, keys::AVATAR];

/// Conversation types whose message rows are backed up. Anon types are
/// excluded together with their conversation tables (module doc).
const MESSAGE_CONVERSATION_TYPES: &str = "('channel','server_dm','saved_messages')";

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct BackupInfo {
    #[prost(uint32, tag = "1")]
    pub payload_version: u32,
    /// The exporting account (informational; import does not require a match —
    /// a restored device has the same account by construction of both flows).
    #[prost(string, tag = "2")]
    pub user_id: String,
    #[prost(int64, tag = "3")]
    pub created_at_ms: i64,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TableHeader {
    #[prost(string, tag = "1")]
    pub table: String,
    #[prost(string, repeated, tag = "2")]
    pub columns: Vec<String>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RowFrame {
    #[prost(message, repeated, tag = "1")]
    pub values: Vec<ColumnValue>,
}

/// One SQLite value. `kind: None` encodes SQL NULL.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ColumnValue {
    #[prost(oneof = "column_value::Kind", tags = "1, 2, 3, 4")]
    pub kind: Option<column_value::Kind>,
}

pub mod column_value {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Kind {
        #[prost(int64, tag = "1")]
        Integer(i64),
        #[prost(double, tag = "2")]
        Real(f64),
        #[prost(string, tag = "3")]
        Text(String),
        #[prost(bytes, tag = "4")]
        Blob(Vec<u8>),
    }
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Frame {
    #[prost(oneof = "frame::Payload", tags = "1, 2, 3")]
    pub payload: Option<frame::Payload>,
}

pub mod frame {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Payload {
        #[prost(message, tag = "1")]
        Info(super::BackupInfo),
        #[prost(message, tag = "2")]
        Table(super::TableHeader),
        #[prost(message, tag = "3")]
        Row(super::RowFrame),
    }
}

/// Import outcome counters (also used by export for the row total).
#[derive(Debug, Default, Clone, Copy)]
pub struct ImportSummary {
    pub tables: usize,
    pub rows_seen: usize,
    /// Rows admitted, including updates to older evidence within this payload.
    /// Already retained local actor authority and duplicate evidence are skipped.
    pub rows_inserted: usize,
    /// Rows already present locally or not admitted by retained reaction authority.
    /// Unsupported payloads return an error instead of an incomplete summary.
    pub rows_skipped: usize,
}

fn value_ref_to_column(v: ValueRef<'_>) -> ColumnValue {
    use column_value::Kind;
    let kind = match v {
        ValueRef::Null => None,
        ValueRef::Integer(i) => Some(Kind::Integer(i)),
        ValueRef::Real(f) => Some(Kind::Real(f)),
        ValueRef::Text(t) => Some(Kind::Text(String::from_utf8_lossy(t).into_owned())),
        ValueRef::Blob(b) => Some(Kind::Blob(b.to_vec())),
    };
    ColumnValue { kind }
}

fn column_to_sql_value(c: &ColumnValue) -> SqlValue {
    use column_value::Kind;
    match &c.kind {
        None => SqlValue::Null,
        Some(Kind::Integer(i)) => SqlValue::Integer(*i),
        Some(Kind::Real(f)) => SqlValue::Real(*f),
        Some(Kind::Text(t)) => SqlValue::Text(t.clone()),
        Some(Kind::Blob(b)) => SqlValue::Blob(b.clone()),
    }
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>, StorageError> {
    // `table` is always from the compile-time whitelist here — never
    // attacker-controlled — but PRAGMA cannot be parameterized anyway.
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(cols)
}

fn push_frame(out: &mut Vec<u8>, payload: frame::Payload) {
    let f = Frame {
        payload: Some(payload),
    };
    f.encode_length_delimited(out)
        .expect("Vec<u8> encode cannot fail");
}

fn dump_table(
    conn: &Connection,
    out: &mut Vec<u8>,
    table: &str,
    select_sql: &str,
    params: &[&dyn rusqlite::ToSql],
    rows_out: &mut usize,
    keep: &dyn Fn(&rusqlite::Row<'_>) -> rusqlite::Result<bool>,
) -> Result<(), StorageError> {
    let columns = table_columns(conn, table)?;
    push_frame(
        out,
        frame::Payload::Table(TableHeader {
            table: table.to_string(),
            columns: columns.clone(),
        }),
    );
    let mut stmt = conn.prepare(select_sql)?;
    let ncols = columns.len();
    let mut rows = stmt.query(params)?;
    while let Some(row) = rows.next()? {
        if !keep(row)? {
            continue;
        }
        let mut values = Vec::with_capacity(ncols);
        for i in 0..ncols {
            values.push(value_ref_to_column(row.get_ref(i)?));
        }
        push_frame(out, frame::Payload::Row(RowFrame { values }));
        *rows_out += 1;
    }
    Ok(())
}

/// Serialize the whole backup payload (uncompressed frame stream). Returns
/// `(payload_bytes, total_rows)`.
pub fn dump_frames(conn: &Connection, user_id: &str) -> Result<(Vec<u8>, usize), StorageError> {
    let mut out = Vec::new();
    let mut rows = 0usize;
    push_frame(
        &mut out,
        frame::Payload::Info(BackupInfo {
            payload_version: PAYLOAD_VERSION,
            user_id: user_id.to_string(),
            created_at_ms: chrono::Utc::now().timestamp_millis(),
        }),
    );

    for &table in BACKUP_TABLES {
        if table == "attachments" {
            dump_attachments(conn, &mut out, &mut rows)?;
            continue;
        }
        // SELECT * would also work, but naming the PRAGMA columns pins the
        // value order to the emitted TableHeader.
        let cols = table_columns(conn, table)?.join(", ");
        // Reaction metadata must obey the same privacy boundary as history.
        // A scope string alone is insufficient: require its retained target.
        let ownership = match table {
            "unified_reactions" => Some(format!(
                "EXISTS (SELECT 1 FROM messages m WHERE m.conversation_type IN {MESSAGE_CONVERSATION_TYPES}
                 AND m.conversation_type={table}.conversation_type AND m.conversation_id={table}.conversation_id
                 AND m.sender_id={table}.sender_id AND m.sent_at={table}.sent_at_ns)")),
            "pending_scoped_reactions" | "reaction_action_versions" | "reaction_actor_selections" | "pending_reaction_add_selections" => Some(format!(
                "EXISTS (SELECT 1 FROM messages m WHERE m.conversation_type IN {MESSAGE_CONVERSATION_TYPES}
                 AND m.conversation_type={table}.conversation_type AND m.conversation_id={table}.conversation_id
                 AND m.sender_id={table}.author_id AND m.sent_at={table}.target_sent_at)")),
            "messages" => Some(format!("conversation_type IN {MESSAGE_CONVERSATION_TYPES}")),
            _ => None,
        };
        let select = match ownership {
            Some(predicate) => format!("SELECT {cols} FROM {table} WHERE {predicate}"),
            None => format!("SELECT {cols} FROM {table}"),
        };
        dump_table(conn, &mut out, table, &select, &[], &mut rows, &|_| {
            Ok(true)
        })?;
    }

    // Own-profile settings subset (never the whole settings table — see
    // module doc).
    let placeholders = OWN_PROFILE_SETTINGS_KEYS
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");
    let params: Vec<&dyn rusqlite::ToSql> = OWN_PROFILE_SETTINGS_KEYS
        .iter()
        .map(|k| k as &dyn rusqlite::ToSql)
        .collect();
    let cols = table_columns(conn, "settings")?.join(", ");
    dump_table(
        conn,
        &mut out,
        "settings",
        &format!("SELECT {cols} FROM settings WHERE key IN ({placeholders})"),
        &params,
        &mut rows,
        &|_| Ok(true),
    )?;

    Ok((out, rows))
}

/// Dump the attachment rows the backed-up messages reference, as pointers.
///
/// The ciphertext stays on the CDN; the importing device fetches it the way
/// it fetches a freshly received attachment. So the device-local columns are
/// reset rather than copied: file paths and the inline ciphertext cache name
/// THIS device's storage, and the download state must read "not downloaded"
/// (`AttachmentStatus::Pending` = 0, `ContentLocation::NotDownloaded` = 0 in
/// `attachment_repo`). Rows only referenced by excluded (anon) conversations
/// stay out, with those conversations.
fn dump_attachments(
    conn: &Connection,
    out: &mut Vec<u8>,
    rows_out: &mut usize,
) -> Result<(), StorageError> {
    use crate::domain::ids::AttachmentId;
    use crate::storage::serialization::db_deserialize;

    let mut referenced = std::collections::HashSet::new();
    {
        let mut stmt = conn.prepare(&format!(
            "SELECT attachments_blob FROM messages \
             WHERE conversation_type IN {MESSAGE_CONVERSATION_TYPES}"
        ))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let blob: Vec<u8> = row.get(0)?;
            if let Ok(ids) = db_deserialize::<Vec<AttachmentId>>(&blob) {
                referenced.extend(ids.iter().map(ToString::to_string));
            }
        }
    }

    let columns = table_columns(conn, "attachments")?;
    let id_index = columns
        .iter()
        .position(|c| c == "id")
        .ok_or_else(|| StorageError::InvalidData("attachments table has no id column".into()))?;
    let select = columns
        .iter()
        .map(|c| match c.as_str() {
            "local_path" | "file_path" | "encrypted_content" => "NULL".to_string(),
            "status" | "content_location" => "0".to_string(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    dump_table(
        conn,
        out,
        "attachments",
        &format!("SELECT {select} FROM attachments"),
        &[],
        rows_out,
        &|row| Ok(referenced.contains(&row.get::<_, String>(id_index)?)),
    )
}

/// Import a decrypted payload. Existing PK/dedup conflicts are skipped, so
/// rows that already exist locally (same PK / dedup key) are skipped — a
/// restore can never overwrite newer local data. Unknown tables and columns
/// reject the payload; the transaction then rolls back without partial import.
pub fn import_frames(conn: &mut Connection, payload: &[u8]) -> Result<ImportSummary, StorageError> {
    let mut buf = payload;
    let mut summary = ImportSummary::default();

    // Per-table insert state, rebuilt at each TableHeader frame.
    struct TableState {
        insert_sql: String,
        scoped_indices: Option<[usize; 6]>,
        ledger_table: Option<String>,
        columns: Vec<String>,
    }
    let tx = conn.transaction()?;
    // A restore merges history, never overrides a locally retained actor. The
    // guard is per actor, not per emoji, including removed and held selections.
    let local_actors: HashSet<(String, String, String, i64, String)> = tx
        .prepare(
            "SELECT conversation_id,conversation_type,author_id,target_sent_at,actor_id FROM reaction_action_versions
            UNION SELECT conversation_id,conversation_type,author_id,target_sent_at,actor_id FROM reaction_actor_selections
            UNION SELECT conversation_id,conversation_type,sender_id,sent_at_ns,user_id FROM unified_reactions
            UNION SELECT conversation_id,conversation_type,author_id,target_sent_at,actor_id FROM pending_scoped_reactions
            UNION SELECT conversation_id,conversation_type,author_id,target_sent_at,actor_id FROM pending_reaction_add_selections",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
        .collect::<Result<_, _>>()?;
    let mut ledgers = Vec::new();
    let mut scoped_votes = Vec::new();
    let mut current: Option<TableState> = None;

    let mut saw_info = false;
    while buf.has_remaining() {
        let f = Frame::decode_length_delimited(&mut buf)
            .map_err(|e| StorageError::InvalidData(format!("backup frame undecodable: {e}")))?;
        match f.payload {
            Some(frame::Payload::Info(info)) => {
                if saw_info {
                    return Err(StorageError::InvalidData(
                        "backup payload has duplicate info frames".into(),
                    ));
                }
                if info.payload_version > PAYLOAD_VERSION {
                    return Err(StorageError::UnsupportedBackup(format!(
                        "payload version {} is newer than supported version {}",
                        info.payload_version, PAYLOAD_VERSION
                    )));
                }
                if info.payload_version != PAYLOAD_VERSION {
                    return Err(StorageError::InvalidData(format!(
                        "unsupported backup payload version {} (this build supports version {})",
                        info.payload_version, PAYLOAD_VERSION
                    )));
                }
                saw_info = true;
            }
            Some(frame::Payload::Table(th)) => {
                if !saw_info {
                    return Err(StorageError::InvalidData(
                        "backup table appears before info frame".into(),
                    ));
                }
                let allowed = BACKUP_TABLES.contains(&th.table.as_str()) || th.table == "settings";
                if !allowed {
                    return Err(StorageError::UnsupportedBackup(format!(
                        "backup contains unknown table '{}'",
                        th.table
                    )));
                }
                let table = th.table.as_str();
                let live_cols = table_columns(&tx, table)?;
                for col in &th.columns {
                    if !live_cols.contains(col) {
                        return Err(StorageError::UnsupportedBackup(format!(
                            "backup table '{table}' contains unknown column '{col}'"
                        )));
                    }
                }
                if th.columns.is_empty() {
                    return Err(StorageError::InvalidData(format!(
                        "backup table '{table}' has no columns"
                    )));
                }
                let names = &th.columns;
                let ledger_table = matches!(
                    table,
                    "reaction_action_versions"
                        | "reaction_actor_selections"
                        | "pending_reaction_add_selections"
                        | "pending_scoped_reactions"
                )
                .then(|| th.table.clone());
                let scoped_indices: Option<[usize; 6]> =
                    if table == "unified_reactions" || ledger_table.is_some() {
                        let identity = if ledger_table.is_some() {
                            [
                                "conversation_id",
                                "conversation_type",
                                "author_id",
                                "target_sent_at",
                                "actor_id",
                                "emoji",
                            ]
                        } else {
                            [
                                "conversation_id",
                                "conversation_type",
                                "sender_id",
                                "sent_at_ns",
                                "user_id",
                                "emoji",
                            ]
                        };
                        Some(
                            identity
                                .map(|name| {
                                    names.iter().position(|column| column == name).ok_or_else(
                                        || {
                                            StorageError::InvalidData(
                                                "Scoped backup evidence lacks identity".into(),
                                            )
                                        },
                                    )
                                })
                                .into_iter()
                                .collect::<Result<Vec<_>, _>>()?
                                .try_into()
                                .map_err(|_| {
                                    StorageError::InvalidData("Scoped backup identity width".into())
                                })?,
                        )
                    } else {
                        None
                    };
                let placeholders = names.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let insert_sql = if let Some(table) = &ledger_table {
                    names.iter().position(|n| n == "action_ts").ok_or_else(|| {
                        StorageError::InvalidData("Reaction backup lacks action clock".into())
                    })?;
                    let mut updates =
                        "emoji=excluded.emoji,action_ts=excluded.action_ts".to_string();
                    let mut tie = format!("excluded.emoji>{table}.emoji");
                    let conflict = if table == "reaction_action_versions"
                        || table == "pending_scoped_reactions"
                    {
                        if !names.iter().any(|n| n == "remove") {
                            return Err(StorageError::InvalidData(
                                "Reaction backup lacks operation".into(),
                            ));
                        }
                        updates = "action_ts=excluded.action_ts,remove=excluded.remove".into();
                        tie = format!("excluded.remove>{table}.remove");
                        if table == "pending_scoped_reactions" {
                            if !names.iter().any(|n| n == "received_at") {
                                return Err(StorageError::InvalidData(
                                    "Held backup lacks receipt clock".into(),
                                ));
                            }
                            updates.push_str(",received_at=excluded.received_at");
                        }
                        "conversation_type,conversation_id,author_id,target_sent_at,actor_id,emoji"
                    } else {
                        "conversation_type,conversation_id,author_id,target_sent_at,actor_id"
                    };
                    format!(
                        "INSERT INTO {table} ({}) VALUES ({placeholders}) ON CONFLICT({conflict}) DO UPDATE SET {updates} WHERE excluded.action_ts>{table}.action_ts OR (excluded.action_ts={table}.action_ts AND {tie})",
                        names.join(",")
                    )
                } else if table == "unified_reactions" {
                    // Ignore only uniqueness conflicts, never NOT NULL failures.
                    format!(
                        "INSERT INTO {table} ({}) VALUES ({placeholders}) ON CONFLICT DO NOTHING",
                        names.join(", ")
                    )
                } else {
                    format!(
                        "INSERT OR IGNORE INTO {table} ({}) VALUES ({placeholders})",
                        names.join(", ")
                    )
                };
                current = Some(TableState {
                    insert_sql,
                    scoped_indices,
                    ledger_table,
                    columns: names.clone(),
                });
                summary.tables += 1;
            }
            Some(frame::Payload::Row(row)) => {
                let Some(state) = &current else {
                    return Err(StorageError::InvalidData(
                        "backup row has no table header".into(),
                    ));
                };
                if row.values.len() != state.columns.len() {
                    return Err(StorageError::InvalidData(format!(
                        "backup row has {} values for {} columns",
                        row.values.len(),
                        state.columns.len()
                    )));
                }
                summary.rows_seen += 1;

                let params: Vec<SqlValue> = row.values.iter().map(column_to_sql_value).collect();
                if let Some(indices) = state.scoped_indices {
                    let text = |i: usize| match params.get(i) {
                        Some(SqlValue::Text(value)) => Ok(value.clone()),
                        _ => Err(StorageError::InvalidData(
                            "Invalid scoped backup vote identity".into(),
                        )),
                    };
                    let timestamp = match params.get(indices[3]) {
                        Some(SqlValue::Integer(value)) => *value,
                        _ => {
                            return Err(StorageError::InvalidData(
                                "Invalid scoped backup vote timestamp".into(),
                            ));
                        }
                    };
                    let actor = (
                        text(indices[0])?,
                        text(indices[1])?,
                        text(indices[2])?,
                        timestamp,
                        text(indices[4])?,
                    );
                    let emoji = text(indices[5])?;
                    if let Some(table) = &state.ledger_table {
                        let clock_index =
                            state.columns.iter().position(|n| n == "action_ts").unwrap();
                        if !matches!(params.get(clock_index),Some(SqlValue::Integer(value)) if *value>0)
                        {
                            return Err(StorageError::InvalidData(
                                "Invalid restored reaction action clock".into(),
                            ));
                        }
                        if let Some(index) = state.columns.iter().position(|n| n == "remove") {
                            if !matches!(params.get(index), Some(SqlValue::Integer(0 | 1))) {
                                return Err(StorageError::InvalidData(
                                    "Invalid restored reaction operation".into(),
                                ));
                            }
                        }
                        if !local_actors.contains(&actor) {
                            ledgers.push((table.clone(), state.insert_sql.clone(), params));
                        }
                    } else if !local_actors.contains(&actor) {
                        scoped_votes.push((state.insert_sql.clone(), params, actor, emoji));
                    }
                    continue;
                }
                let inserted = tx
                    .prepare_cached(&state.insert_sql)?
                    .execute(rusqlite::params_from_iter(params.into_iter()))?;
                summary.rows_inserted += inserted;
            }
            None => {}
        }
    }
    if !saw_info {
        return Err(StorageError::InvalidData(
            "backup payload missing info frame".into(),
        ));
    }
    // All action evidence is installed before any vote, regardless of frame
    // order. LWW merge applies only to actors absent before this restore.
    // Receipt metadata and duplicate vote IDs are sorted for stable re-export.
    ledgers.sort_by_key(|row| format!("{:?}", row));
    for (_, sql, values) in ledgers {
        summary.rows_inserted += tx
            .prepare_cached(&sql)?
            .execute(rusqlite::params_from_iter(values))?;
    }
    // A vote is admitted only when the restored selection names it and that
    // selection's action is an add: the ledger, not the projection, orders actions.
    scoped_votes.sort_by_key(|row| format!("{:?}", row));
    for (sql, values, actor, emoji) in scoped_votes {
        let selected: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM reaction_actor_selections s JOIN reaction_action_versions v ON v.conversation_id=s.conversation_id AND v.conversation_type=s.conversation_type AND v.author_id=s.author_id AND v.target_sent_at=s.target_sent_at AND v.actor_id=s.actor_id AND v.emoji=s.emoji WHERE s.conversation_id=?1 AND s.conversation_type=?2 AND s.author_id=?3 AND s.target_sent_at=?4 AND s.actor_id=?5 AND s.emoji=?6 AND v.remove=0 AND v.action_ts=s.action_ts)",rusqlite::params![actor.0,actor.1,actor.2,actor.3,actor.4,emoji],|r|r.get(0))?;
        if selected {
            summary.rows_inserted += tx
                .prepare_cached(&sql)?
                .execute(rusqlite::params_from_iter(values.iter()))?;
        }
    }
    reaction_projection::rebuild_all(&tx)?;
    tx.commit()?;
    summary.rows_skipped = summary.rows_seen.saturating_sub(summary.rows_inserted);
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Database;

    fn test_db(dir: &std::path::Path) -> Database {
        let db = Database::open(&dir.join("test.db"), "test_passphrase").unwrap();
        db.run_migrations().unwrap();
        db
    }

    fn insert_message(conn: &Connection, id: &str, text: &str, sent_at: i64) {
        conn.execute(
            "INSERT INTO messages (id, conversation_type, conversation_id, sender_id, \
             content_type, content_json, is_pinned, is_edited, created_at, updated_at, sent_at) \
             VALUES (?1, 'channel', ?1, 'user-a', 0, ?2, 1, 1, '2026-01-01', '2026-01-01', ?3)",
            rusqlite::params![id, format!("{{\"text\":\"{text}\"}}"), sent_at],
        )
        .unwrap();
    }

    /// Inspect actual frame bytes and decoded row counts, not just SQL filters.
    #[test]
    fn serialized_reaction_export_requires_retained_history_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db(dir.path());
        let cases = [
            ("keep-channel", "channel", 10),
            ("keep-server-dm", "server_dm", 20),
            ("keep-saved", "saved_messages", 30),
            ("excluded-anon-dm", "anon_dm", 40),
            ("excluded-anon-group", "anon_group", 50),
        ];
        {
            let conn = db.connection().unwrap();
            for (scope, kind, timestamp) in cases {
                insert_message(&conn, scope, scope, timestamp);
                conn.execute(
                    "UPDATE messages SET conversation_id=?1,conversation_type=?2 WHERE id=?1",
                    rusqlite::params![scope, kind],
                )
                .unwrap();
                conn.execute("INSERT INTO unified_reactions(id,conversation_id,conversation_type,sender_id,sent_at_ns,user_id,emoji,created_at) VALUES(?1,?1,?2,'user-a',?3,'voter','👍','now')",rusqlite::params![scope,kind,timestamp]).unwrap();
                conn.execute("INSERT INTO pending_scoped_reactions VALUES(?1,?2,'user-a',?3,'actor','👍',0,1,1)",rusqlite::params![scope,kind,timestamp]).unwrap();
                conn.execute("INSERT INTO reaction_action_versions VALUES(?1,?2,'user-a',?3,'actor','👍',1,0)",rusqlite::params![scope,kind,timestamp]).unwrap();
                conn.execute("INSERT INTO pending_reaction_add_selections VALUES(?1,?2,'user-a',?3,'actor','👍',1)",rusqlite::params![scope,kind,timestamp]).unwrap();
            }
            // An eligible-looking scope without its actual target is excluded.
            conn.execute_batch("INSERT INTO unified_reactions VALUES('excluded-fake-scope','keep-channel','channel','user-a',999,'voter','👍','now');
                INSERT INTO pending_scoped_reactions VALUES('excluded-fake-scope','channel','user-a',999,'actor','👍',0,1,1);
                INSERT INTO reaction_action_versions VALUES('excluded-fake-scope','channel','user-a',999,'actor','👍',1,0);
                INSERT INTO pending_reaction_add_selections VALUES('excluded-fake-scope','channel','user-a',999,'actor','👍',1);").unwrap();
        }
        let payload = dump_frames(&db.connection().unwrap(), "owner").unwrap().0;
        for marker in ["keep-channel", "keep-server-dm", "keep-saved"] {
            assert!(
                payload
                    .windows(marker.len())
                    .any(|bytes| bytes == marker.as_bytes()),
                "positive control missing: {marker}"
            );
        }
        for marker in [
            "excluded-anon-dm",
            "excluded-anon-group",
            "excluded-fake-scope",
        ] {
            assert!(
                !payload
                    .windows(marker.len())
                    .any(|bytes| bytes == marker.as_bytes()),
                "excluded state leaked: {marker}"
            );
        }
        let mut counts = std::collections::HashMap::<String, usize>::new();
        let mut current = String::new();
        let mut input = payload.as_slice();
        while input.has_remaining() {
            match Frame::decode_length_delimited(&mut input).unwrap().payload {
                Some(frame::Payload::Table(header)) => current = header.table,
                Some(frame::Payload::Row(_)) => *counts.entry(current.clone()).or_default() += 1,
                _ => {}
            }
        }
        for table in [
            "unified_reactions",
            "pending_scoped_reactions",
            "reaction_action_versions",
            "pending_reaction_add_selections",
        ] {
            assert_eq!(
                counts.get(table),
                Some(&3),
                "exact eligible controls for {table}"
            );
        }
        assert_eq!(counts.get("messages"), Some(&3));
    }

    /// Round-trip: rows dumped from one DB appear byte-exact in a fresh DB.
    /// MUTATION: dropping a table from BACKUP_TABLES fails the count/content
    /// assertions; switching INSERT OR IGNORE to plain INSERT fails the
    /// idempotence re-run below.
    #[test]
    fn dump_import_round_trip_and_idempotence() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();
        let src = test_db(src_dir.path());
        let dst = test_db(dst_dir.path());

        {
            let conn = src.connection().unwrap();
            insert_message(&conn, "m1", "hello backup", 1111);
            insert_message(&conn, "m2", "second message", 2222);
            conn.execute(
                "INSERT INTO unified_reactions (id, sender_id, sent_at_ns, user_id, emoji, created_at,conversation_id,conversation_type) \
                 VALUES ('r1', 'user-a', 1111, 'user-b', '👍', '2026-01-01','m1','channel')",
                [],
            )
            .unwrap();
            conn.execute("INSERT INTO reaction_action_versions VALUES('m1','channel','user-a',1111,'user-b','👍',10,0)", []).unwrap();
            conn.execute("INSERT INTO reaction_actor_selections VALUES('m1','channel','user-a',1111,'user-b','👍',10)", []).unwrap();
            conn.execute(
                "INSERT INTO users (id, username, svr_public_key, last_active, created_at) \
                 VALUES ('user-b', 'bob', X'00', '2026-01-01', '2026-01-01')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO settings (key, value, updated_at) VALUES (?1, '\"Alice\"', '2026-01-01')",
                [keys::DISPLAY_NAME],
            )
            .unwrap();
            // A settings row that must NOT be exported:
            conn.execute(
                "INSERT INTO settings (key, value, updated_at) VALUES ('crypto.profile_key', '\"SECRET\"', '2026-01-01')",
                [],
            )
            .unwrap();
        }

        let (payload, rows) = {
            let conn = src.connection().unwrap();
            dump_frames(&conn, "user-a").unwrap()
        };
        assert!(rows >= 5, "expected at least 5 rows dumped, got {rows}");
        // Exclusion proof: the crypto.* settings row is not in the payload.
        assert!(
            !payload
                .windows(b"crypto.profile_key".len())
                .any(|w| w == b"crypto.profile_key")
        );

        let summary = {
            let mut conn = dst.connection().unwrap();
            import_frames(&mut conn, &payload).unwrap()
        };
        assert_eq!(
            summary.rows_inserted, rows,
            "all rows must import into a fresh DB"
        );

        {
            let conn = dst.connection().unwrap();
            let text: String = conn
                .query_row(
                    "SELECT content_json FROM messages WHERE id = 'm1'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(text, "{\"text\":\"hello backup\"}");
            let pinned: i64 = conn
                .query_row("SELECT is_pinned FROM messages WHERE id = 'm1'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(pinned, 1, "pin state must survive the round trip");
            let emoji: String = conn
                .query_row(
                    "SELECT emoji FROM unified_reactions WHERE id = 'r1'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(emoji, "👍");
            let leaked: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM settings WHERE key = 'crypto.profile_key'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(leaked, 0, "crypto settings must not ride the backup");
        }

        // Idempotence + no-clobber: mutate a local row, re-import, the local
        // edit must survive (INSERT OR IGNORE skips the existing PK).
        {
            let conn = dst.connection().unwrap();
            conn.execute(
                "UPDATE messages SET content_json = '{\"text\":\"locally newer\"}' WHERE id = 'm1'",
                [],
            )
            .unwrap();
        }
        let summary2 = {
            let mut conn = dst.connection().unwrap();
            import_frames(&mut conn, &payload).unwrap()
        };
        assert_eq!(
            summary2.rows_inserted, 0,
            "re-import must skip every existing row"
        );
        {
            let conn = dst.connection().unwrap();
            let text: String = conn
                .query_row(
                    "SELECT content_json FROM messages WHERE id = 'm1'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                text, "{\"text\":\"locally newer\"}",
                "import must never clobber existing local rows"
            );
        }
    }

    /// §138: a GIF in channel history previewed as "📎 1 attachment" on a
    /// freshly linked device, because the archive carried the message's
    /// attachment ids but not the attachment rows. The rows must ride as
    /// pointers (metadata + CDN key + key), with this device's file path and
    /// download state reset; rows only an anon conversation references stay out.
    /// MUTATION: dropping "attachments" from BACKUP_TABLES fails the preview
    /// assertion; copying status/file_path verbatim fails the reset assertions.
    #[test]
    fn attachment_pointers_ride_the_archive_so_previews_survive() {
        use crate::domain::attachment::{
            Attachment, AttachmentStatus, ContentLocation, FileMetadata,
        };
        use crate::domain::ids::{AttachmentId, ChannelId, UserId};
        use crate::domain::message::{ConversationType, Message};
        use crate::storage::repositories::attachment_repo::AttachmentRepository;
        use crate::storage::repositories::unified_message_repo::UnifiedMessageRepository;
        use std::sync::Arc;

        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();
        let src = Arc::new(test_db(src_dir.path()));
        let dst = Arc::new(test_db(dst_dir.path()));

        // Sender side: an uploaded GIF, stored the way upload.rs stores it.
        let gif_id = AttachmentId::from_cdn_key("cdn/gif-1");
        let mut gif = Attachment::new(
            gif_id,
            FileMetadata::new("".into(), "image/gif".into(), 4096),
        );
        gif.status = AttachmentStatus::Ready;
        gif.content_location = ContentLocation::OnFilesystem;
        gif.remote_url = Some("cdn/gif-1".into());
        gif.encryption_key = Some(vec![7; 64]);
        gif.encryption_iv = Some(vec![9; 32]);
        AttachmentRepository::new(src.clone())
            .insert_with_file_path(
                &gif,
                std::path::Path::new("/src-device/attachments/gif.enc"),
            )
            .unwrap();
        // An attachment only an anon DM references: excluded with the DM.
        let anon_id = AttachmentId::from_cdn_key("cdn/anon-1");
        AttachmentRepository::new(src.clone())
            .insert(&Attachment::new(
                anon_id,
                FileMetadata::new("secret.pdf".into(), "application/pdf".into(), 1),
            ))
            .unwrap();

        let sender = UserId::new();
        let mut channel_msg = Message::new_text(
            ChannelId::new(),
            ConversationType::Channel,
            Some(sender),
            String::new(),
            1_000,
        );
        channel_msg.attachments = vec![gif_id];
        let mut anon_msg = Message::new_text(
            ChannelId::new(),
            ConversationType::AnonDm,
            Some(sender),
            String::new(),
            2_000,
        );
        anon_msg.attachments = vec![anon_id];
        let msgs = UnifiedMessageRepository::new(src.clone());
        msgs.insert(&channel_msg).unwrap();
        msgs.insert(&anon_msg).unwrap();

        let (payload, _) = dump_frames(&src.connection().unwrap(), "user-a").unwrap();
        import_frames(&mut dst.connection().unwrap(), &payload).unwrap();

        let repo = AttachmentRepository::new(dst.clone());
        let restored = repo
            .get(&gif_id)
            .unwrap()
            .expect("GIF attachment row must ride the archive");
        let label = restored.metadata.preview_label();
        assert_eq!(
            channel_msg.sidebar_preview(None, Some(&label)),
            "\u{1F39E}\u{FE0F} GIF",
            "a GIF must preview as GIF on the linked device, not as a bare attachment count"
        );
        assert!(
            restored.is_downloadable(),
            "pointer (CDN key + key) must survive"
        );
        assert_eq!(
            restored.encryption_iv,
            Some(vec![9; 32]),
            "digest must survive"
        );
        assert_eq!(
            restored.status,
            AttachmentStatus::Pending,
            "download state must reset"
        );
        assert_eq!(restored.content_location, ContentLocation::NotDownloaded);
        let file_path: Option<String> = dst
            .connection()
            .unwrap()
            .query_row(
                "SELECT file_path FROM attachments WHERE id = ?1",
                [gif_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            file_path, None,
            "the source device's file path must not ride"
        );
        assert!(
            repo.get(&anon_id).unwrap().is_none(),
            "an attachment only an anon conversation references must stay out"
        );
    }

    /// Garbage and truncated payloads are rejected, never partially applied.
    #[test]
    fn import_rejects_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db(dir.path());
        let mut conn = db.connection().unwrap();
        assert!(import_frames(&mut conn, b"not frames at all").is_err());
        assert!(
            import_frames(&mut conn, b"").is_err(),
            "empty payload lacks the info frame"
        );
    }
    #[test]
    fn scoped_backup_restores_ordering_and_old_votes_cannot_reverse_local_removal() {
        use crate::domain::message::{ConversationType, Message};
        use crate::domain::reaction::ReactionScope;
        use crate::domain::{ChannelId, UserId};
        use crate::storage::MessageRepository;
        use std::sync::Arc;
        let source_dir = tempfile::tempdir().unwrap();
        let source = Arc::new(test_db(source_dir.path()));
        let author = UserId::new();
        let own = UserId::new();
        let scope = ReactionScope::new(ChannelId::new(), ConversationType::ServerDm);
        let repo = MessageRepository::new(source.clone());
        let message = Message::new_text(
            scope.id,
            scope.kind.clone(),
            Some(author),
            "history and drafts preserved".into(),
            100,
        );
        repo.insert(&message).unwrap();
        repo.add_reaction_at(&scope, &author, 100, &own, &own, "👍", 10)
            .unwrap();
        let old = dump_frames(&source.connection().unwrap(), &own.to_string())
            .unwrap()
            .0;
        repo.remove_reaction_at(&scope, &author, 100, &own, &own, "👍", 20)
            .unwrap();
        // An imported pending add at 15 is older than this retained tombstone.
        source.connection().unwrap().execute("INSERT INTO pending_scoped_reactions VALUES(?1,'server_dm',?2,100,?3,'👍',0,15,15)",rusqlite::params![scope.id.to_string(),author.to_string(),own.to_string()]).unwrap();
        let current = dump_frames(&source.connection().unwrap(), &own.to_string())
            .unwrap()
            .0;
        let restore_dir = tempfile::tempdir().unwrap();
        let restored = Arc::new(test_db(restore_dir.path()));
        import_frames(&mut restored.connection().unwrap(), &current).unwrap();
        import_frames(&mut restored.connection().unwrap(), &old).unwrap();
        let settings =
            crate::storage::repositories::settings_repo::SettingsRepository::new(restored.clone());
        settings
            .set("auth.current_user_id", &own.to_string())
            .unwrap();
        let restored_repo = MessageRepository::new(restored.clone());
        assert!(restored_repo.replay_held_reactions().unwrap().is_empty());
        assert!(
            restored_repo
                .get_reactions(&scope, &author, 100)
                .unwrap()
                .is_empty()
        );
        assert!(
            !restored_repo
                .add_reaction_at(&scope, &author, 100, &own, &own, "👍", 15)
                .unwrap()
        );
        assert_eq!(
            restored_repo.get(&message.id).unwrap().unwrap().content,
            message.content
        );
        let (clock, remove): (i64, bool) = restored
            .connection()
            .unwrap()
            .query_row(
                "SELECT action_ts,remove FROM reaction_action_versions",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((clock, remove), (20, true));
        // Fresh restore of the old snapshot preserves its actual clock, too.
        let older_dir = tempfile::tempdir().unwrap();
        let older = Arc::new(test_db(older_dir.path()));
        import_frames(&mut older.connection().unwrap(), &old).unwrap();
        let older_repo = MessageRepository::new(older.clone());
        assert_eq!(
            older_repo
                .get_reaction_counts(&scope, &author, 100)
                .unwrap()[0]
                .count,
            1
        );
        assert!(
            !older_repo
                .remove_reaction_at(&scope, &author, 100, &own, &own, "👍", 9)
                .unwrap()
        );
        assert_eq!(
            older_repo
                .get_reaction_counts(&scope, &author, 100)
                .unwrap()[0]
                .count,
            1
        );
    }
}

#[cfg(test)]
#[path = "reaction_backup_regressions.rs"]
mod reaction_backup_regressions;
