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
//!   Reaction ledgers and pending state require a matching retained target;
//!   unscoped evidence without one provable eligible owner stays local only.
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
//! Unknown tables/columns are skipped (forward compatibility across schema drift).

use prost::Message;
use prost::bytes::Buf;
use rusqlite::Connection;
use rusqlite::types::{Value as SqlValue, ValueRef};

use crate::error::StorageError;
use crate::storage::repositories::settings_repo::keys;
use crate::storage::repositories::unified_message_repo::reaction_normalization;

/// Payload schema version (independent of the container's format_version).
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
    "reaction_legacy_unscoped",
    "reaction_scope_unresolved",
    "reaction_legacy_pending",
    "pending_scoped_reactions",
    "reaction_action_versions",
    "reaction_actor_selections",
    "reaction_scoped_quarantine",
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
    /// Missing in RC57/RC59. v088 distinguishes repaired latest-add selections
    /// from shipped v087 removal-derived rows, including equal-clock removals.
    #[prost(uint32, tag = "4")]
    pub reaction_selection_version: u32,
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
            reaction_selection_version: 88,
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
            "unified_reactions" | "reaction_scope_unresolved" | "reaction_scoped_quarantine" => Some(format!(
                "EXISTS (SELECT 1 FROM messages m WHERE m.conversation_type IN {MESSAGE_CONVERSATION_TYPES}
                 AND m.conversation_type={table}.conversation_type AND m.conversation_id={table}.conversation_id
                 AND m.sender_id={table}.sender_id AND m.sent_at={table}.sent_at_ns)")),
            "pending_scoped_reactions" | "reaction_action_versions" | "reaction_actor_selections" | "pending_reaction_add_selections" => Some(format!(
                "EXISTS (SELECT 1 FROM messages m WHERE m.conversation_type IN {MESSAGE_CONVERSATION_TYPES}
                 AND m.conversation_type={table}.conversation_type AND m.conversation_id={table}.conversation_id
                 AND m.sender_id={table}.author_id AND m.sent_at={table}.target_sent_at)")),
            "reaction_legacy_unscoped" => Some(format!(
                "(SELECT count(*) FROM messages m WHERE m.sender_id={table}.sender_id AND m.sent_at={table}.sent_at_ns)=1
                 AND EXISTS (SELECT 1 FROM messages m WHERE m.sender_id={table}.sender_id AND m.sent_at={table}.sent_at_ns
                 AND m.conversation_type IN {MESSAGE_CONVERSATION_TYPES})")),
            "reaction_legacy_pending" => Some(format!(
                "{table}.kind='reaction'
                 AND (SELECT count(*) FROM messages m WHERE m.sender_id={table}.author_id AND m.sent_at={table}.target_sent_at)=1
                 AND EXISTS (SELECT 1 FROM messages m WHERE m.sender_id={table}.author_id AND m.sent_at={table}.target_sent_at
                 AND m.conversation_type IN {MESSAGE_CONVERSATION_TYPES}
                 AND ({table}.conversation_id IS NULL OR {table}.conversation_id=m.conversation_id))")),
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
/// restore can never overwrite newer local data. Unknown tables and unknown
/// columns are skipped silently (forward compatibility).
pub fn import_frames(conn: &mut Connection, payload: &[u8]) -> Result<ImportSummary, StorageError> {
    let mut buf = payload;
    let mut summary = ImportSummary::default();

    // Per-table insert state, rebuilt at each TableHeader frame.
    struct TableState {
        insert_sql: String,
        /// Indices into the frame's value vector, aligned with insert_sql
        /// placeholders (backup columns unknown to the live schema dropped).
        value_indices: Vec<usize>,
        legacy_id_index: Option<usize>,
        scoped_indices: Option<[usize; 6]>,
        ledger_table: Option<String>,
        columns: Vec<String>,
    }
    let tx = conn.transaction()?;
    // A restore merges history, never overrides a locally retained actor. The
    // guard is per actor, not per emoji, including removed and held selections.
    tx.execute_batch("CREATE TEMP TABLE rc60_local_reaction_actors AS
        SELECT conversation_id,conversation_type,author_id,target_sent_at,actor_id FROM reaction_action_versions
        UNION SELECT conversation_id,conversation_type,author_id,target_sent_at,actor_id FROM reaction_actor_selections
        UNION SELECT conversation_id,conversation_type,sender_id,sent_at_ns,user_id FROM unified_reactions
        UNION SELECT conversation_id,conversation_type,author_id,target_sent_at,actor_id FROM pending_scoped_reactions
        UNION SELECT conversation_id,conversation_type,author_id,target_sent_at,actor_id FROM pending_reaction_add_selections;")?;
    // DDL participates in the import transaction; malformed frames restore the
    // index and all evidence on rollback. Normalize before recreating the index.
    tx.execute_batch("DROP INDEX IF EXISTS idx_ur_single_actor")?;
    let mut ledgers = Vec::new();
    let mut scoped_votes = Vec::new();
    let mut current: Option<TableState> = None;
    let mut legacy_votes = Vec::new();

    let mut saw_info = false;
    let mut reaction_selection_version = 0;
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
                if info.reaction_selection_version != 0 && info.reaction_selection_version != 88 {
                    return Err(StorageError::InvalidData(
                        "unsupported reaction selection semantics".into(),
                    ));
                }
                if info.payload_version != PAYLOAD_VERSION {
                    return Err(StorageError::InvalidData(format!(
                        "unsupported backup payload version {}",
                        info.payload_version
                    )));
                }
                reaction_selection_version = info.reaction_selection_version;
                saw_info = true;
            }
            Some(frame::Payload::Table(th)) => {
                let allowed = BACKUP_TABLES.contains(&th.table.as_str()) || th.table == "settings";
                if !allowed {
                    // Unknown table from a future payload — skip its rows.
                    current = None;
                    continue;
                }
                let legacy_reactions = th.table == "unified_reactions"
                    && !th.columns.iter().any(|c| c == "conversation_id")
                    && !th.columns.iter().any(|c| c == "conversation_type");
                let target = if legacy_reactions {
                    "reaction_legacy_unscoped"
                } else {
                    &th.table
                };
                if target == "reaction_legacy_unscoped" {
                    let required = [
                        "id",
                        "sender_id",
                        "sent_at_ns",
                        "user_id",
                        "emoji",
                        "created_at",
                    ];
                    if required
                        .iter()
                        .any(|name| !th.columns.iter().any(|c| c == name))
                    {
                        return Err(StorageError::InvalidData(
                            "legacy reaction payload missing required columns".into(),
                        ));
                    }
                }
                let live_cols = table_columns(&tx, target)?;
                let mut names = Vec::new();
                let mut value_indices = Vec::new();
                for (i, col) in th.columns.iter().enumerate() {
                    if live_cols.iter().any(|c| c == col) {
                        names.push(col.clone());
                        value_indices.push(i);
                    }
                }
                if names.is_empty() {
                    current = None;
                    continue;
                }
                let placeholders = names.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let insert_sql = if th.table == "unified_reactions"
                    || target == "reaction_scoped_quarantine"
                    || target == "reaction_legacy_unscoped"
                {
                    // Ignore only uniqueness conflicts, never NOT NULL failures.
                    format!(
                        "INSERT INTO {target} ({}) VALUES ({placeholders}) ON CONFLICT DO NOTHING",
                        names.join(", ")
                    )
                } else {
                    format!(
                        "INSERT OR IGNORE INTO {target} ({}) VALUES ({placeholders})",
                        names.join(", ")
                    )
                };
                let ledger_table = matches!(
                    th.table.as_str(),
                    "reaction_action_versions"
                        | "reaction_actor_selections"
                        | "pending_reaction_add_selections"
                        | "pending_scoped_reactions"
                )
                .then(|| th.table.clone());
                let scoped_indices = if (th.table == "unified_reactions" && !legacy_reactions)
                    || th.table == "reaction_scoped_quarantine"
                    || ledger_table.is_some()
                {
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
                                names
                                    .iter()
                                    .position(|column| column == name)
                                    .ok_or_else(|| {
                                        StorageError::InvalidData(
                                            "Scoped backup evidence lacks identity".into(),
                                        )
                                    })
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
                let insert_sql = if let Some(table) = &ledger_table {
                    names.iter().position(|n| n == "action_ts").ok_or_else(|| {
                        StorageError::InvalidData("Reaction backup lacks action clock".into())
                    })?;
                    let i = scoped_indices.unwrap();
                    let guard = format!(
                        "NOT EXISTS(SELECT 1 FROM rc60_local_reaction_actors l WHERE l.conversation_id=?{} AND l.conversation_type=?{} AND l.author_id=?{} AND l.target_sent_at=?{} AND l.actor_id=?{})",
                        i[0] + 1,
                        i[1] + 1,
                        i[2] + 1,
                        i[3] + 1,
                        i[4] + 1
                    );
                    let numbered = (1..=names.len())
                        .map(|n| format!("?{n}"))
                        .collect::<Vec<_>>()
                        .join(",");
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
                    // Numbered placeholders preserve arbitrary serialized column order.
                    format!(
                        "INSERT INTO {table} ({}) SELECT {numbered} WHERE {guard} ON CONFLICT({conflict}) DO UPDATE SET {updates} WHERE excluded.action_ts>{table}.action_ts OR (excluded.action_ts={table}.action_ts AND {tie})",
                        names.join(",")
                    )
                } else {
                    insert_sql
                };
                current = Some(TableState {
                    insert_sql,
                    scoped_indices,
                    ledger_table,
                    columns: names.clone(),
                    value_indices,
                    legacy_id_index: (target == "reaction_legacy_unscoped")
                        .then(|| names.iter().position(|name| name == "id").unwrap()),
                });
                summary.tables += 1;
            }
            Some(frame::Payload::Row(row)) => {
                let Some(state) = &current else { continue };
                summary.rows_seen += 1;

                let params: Vec<SqlValue> = state
                    .value_indices
                    .iter()
                    .map(|&i| {
                        row.values
                            .get(i)
                            .map(column_to_sql_value)
                            .unwrap_or(SqlValue::Null)
                    })
                    .collect();
                if let Some(id_index) = state.legacy_id_index {
                    if params.iter().any(|v| matches!(v, SqlValue::Null)) {
                        return Err(StorageError::InvalidData(
                            "legacy reaction payload contains NULL evidence".into(),
                        ));
                    }
                    // Defer until every message frame is present, independent of
                    // table order. Never let scoped NOT NULL silently discard votes.
                    legacy_votes.push((state.insert_sql.clone(), id_index, params));
                    continue;
                }
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
                    let key = (
                        text(indices[0])?,
                        text(indices[1])?,
                        text(indices[2])?,
                        timestamp,
                        text(indices[4])?,
                        text(indices[5])?,
                    );
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
                        ledgers.push((table.clone(), state.insert_sql.clone(), params));
                    } else {
                        let id_index =
                            state
                                .columns
                                .iter()
                                .position(|n| n == "id")
                                .ok_or_else(|| {
                                    StorageError::InvalidData(
                                        "Scoped backup vote lacks evidence ID".into(),
                                    )
                                })?;
                        scoped_votes.push((state.insert_sql.clone(), params, key, id_index));
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
    // All action evidence is installed before any projection, regardless of
    // frame order. LWW merge applies only to actors absent before this restore.
    // Receipt metadata and duplicate vote IDs are sorted for stable re-export.
    ledgers.sort_by_key(|row| format!("{:?}", row));
    for (_, sql, values) in ledgers {
        summary.rows_inserted += tx
            .prepare_cached(&sql)?
            .execute(rusqlite::params_from_iter(values))?;
    }
    let mut imported_legacy_ids = Vec::new();
    scoped_votes.sort_by_key(|row| format!("{:?}", row));
    for (sql, values, key, id_index) in scoped_votes {
        let (active_seen, quarantined_seen): (bool, bool) = tx.query_row("SELECT EXISTS(SELECT 1 FROM unified_reactions WHERE id=?6 AND conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at_ns=?4 AND user_id=?5 AND emoji=?7), EXISTS(SELECT 1 FROM reaction_scoped_quarantine WHERE id=?6 AND conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at_ns=?4 AND user_id=?5 AND emoji=?7)",rusqlite::params![key.0,key.1,key.2,key.3,key.4,values[id_index],key.5],|r|Ok((r.get(0)?,r.get(1)?)))?;
        let local: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM rc60_local_reaction_actors WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND target_sent_at=?4 AND actor_id=?5)",rusqlite::params![key.0,key.1,key.2,key.3,key.4],|r|r.get(0))?;
        let clocked: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM reaction_action_versions WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND target_sent_at=?4 AND actor_id=?5 AND emoji=?6 AND action_ts>0)",rusqlite::params![key.0,key.1,key.2,key.3,key.4,key.5],|r|r.get(0))?;
        if active_seen
            || (quarantined_seen
                && (local || !clocked || sql.starts_with("INSERT INTO reaction_scoped_quarantine")))
        {
            continue;
        }
        let mut inserted = 0;
        if !local && clocked {
            inserted = tx
                .prepare_cached(&sql)?
                .execute(rusqlite::params_from_iter(values.iter()))?;
        }
        if inserted == 0 {
            // Unclocked votes never enter the active table's global-ID/dedup
            // constraints. Keep every scoped row in quarantine, including ID
            // collisions and votes rejected by retained local actor authority.
            let quarantine = sql.replace(
                "INSERT INTO unified_reactions",
                "INSERT INTO reaction_scoped_quarantine",
            );
            inserted = tx
                .prepare_cached(&quarantine)?
                .execute(rusqlite::params_from_iter(values.iter()))?;
        }
        summary.rows_inserted += inserted;
    }
    for (sql, id_index, params) in legacy_votes {
        // Preserve skip-existing policy, including an already recovered vote.
        let id = &params[id_index];
        let existing: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM unified_reactions WHERE id=?1)",
            [id],
            |r| r.get(0),
        )?;
        if !existing {
            let inserted = tx
                .prepare_cached(&sql)?
                .execute(rusqlite::params_from_iter(params.iter()))?;
            summary.rows_inserted += inserted;
            if inserted != 0 {
                imported_legacy_ids.push(id.clone());
            }
        }
    }
    for id in imported_legacy_ids {
        // Unique target identity does not establish action order. Promote only
        // clocked evidence for an actor absent locally; the same normalizer
        // then applies selection/removal authority to scoped and legacy votes.
        tx.execute("INSERT INTO unified_reactions
            SELECT r.id,m.conversation_id,m.conversation_type,r.sender_id,r.sent_at_ns,r.user_id,r.emoji,r.created_at
            FROM reaction_legacy_unscoped r JOIN messages m ON m.sender_id=r.sender_id AND m.sent_at=r.sent_at_ns
            WHERE r.id=?1 AND (SELECT count(*) FROM messages x WHERE x.sender_id=r.sender_id AND x.sent_at=r.sent_at_ns)=1
            AND EXISTS(SELECT 1 FROM reaction_action_versions v WHERE v.conversation_id=m.conversation_id AND v.conversation_type=m.conversation_type AND v.author_id=r.sender_id AND v.target_sent_at=r.sent_at_ns AND v.actor_id=r.user_id AND v.emoji=r.emoji AND v.action_ts>0)
            AND NOT EXISTS(SELECT 1 FROM rc60_local_reaction_actors l WHERE l.conversation_id=m.conversation_id AND l.conversation_type=m.conversation_type AND l.author_id=r.sender_id AND l.target_sent_at=r.sent_at_ns AND l.actor_id=r.user_id)
            ON CONFLICT DO NOTHING", [&id])?;
    }
    tx.execute("INSERT OR IGNORE INTO reaction_scope_unresolved
        SELECT DISTINCT m.conversation_id,m.conversation_type,m.sender_id,m.sent_at
        FROM messages m JOIN reaction_legacy_unscoped r ON r.sender_id=m.sender_id AND r.sent_at_ns=m.sent_at
        WHERE NOT EXISTS(SELECT 1 FROM reaction_action_versions v WHERE v.conversation_id=m.conversation_id AND v.conversation_type=m.conversation_type AND v.author_id=r.sender_id AND v.target_sent_at=r.sent_at_ns AND v.actor_id=r.user_id AND v.emoji=r.emoji)", [])?;
    if reaction_selection_version < 88 {
        // Repair old backup ledger semantics without touching retained local
        // selections (especially RC60's legitimate equal-clock add/remove).
        let repair = reaction_normalization::REPAIR_V087_SELECTIONS.replace(
            "WHERE EXISTS (",
            "WHERE NOT EXISTS(SELECT 1 FROM rc60_local_reaction_actors l WHERE l.conversation_id=s.conversation_id AND l.conversation_type=s.conversation_type AND l.author_id=s.author_id AND l.target_sent_at=s.target_sent_at AND l.actor_id=s.actor_id) AND EXISTS (",
        );
        tx.execute_batch(&repair)?;
    }
    reaction_normalization::normalize(&tx)?;
    reaction_normalization::normalize_pending(&tx)?;
    // Remove only evidence recovered as this exact surviving active vote.
    // Losers/unordered legacy evidence remains quarantined, including orphans.
    tx.execute("DELETE FROM reaction_legacy_unscoped AS r WHERE EXISTS
        (SELECT 1 FROM unified_reactions u WHERE u.id=r.id AND u.sender_id=r.sender_id
        AND u.sent_at_ns=r.sent_at_ns AND u.user_id=r.user_id AND u.emoji=r.emoji AND u.created_at=r.created_at)", [])?;

    tx.execute_batch("DROP TABLE rc60_local_reaction_actors")?;
    tx.commit()?;
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
                conn.execute(
                    "INSERT INTO reaction_scope_unresolved VALUES(?1,?2,'user-a',?3)",
                    rusqlite::params![scope, kind, timestamp],
                )
                .unwrap();
                conn.execute("INSERT INTO pending_scoped_reactions VALUES(?1,?2,'user-a',?3,'actor','👍',0,1,1)",rusqlite::params![scope,kind,timestamp]).unwrap();
                conn.execute("INSERT INTO reaction_action_versions VALUES(?1,?2,'user-a',?3,'actor','👍',1,0)",rusqlite::params![scope,kind,timestamp]).unwrap();
                conn.execute("INSERT INTO reaction_scoped_quarantine VALUES(?1,?1,?2,'user-a',?3,'unknown','X','old')",rusqlite::params![scope,kind,timestamp]).unwrap();
                conn.execute("INSERT INTO pending_reaction_add_selections VALUES(?1,?2,'user-a',?3,'actor','👍',1)",rusqlite::params![scope,kind,timestamp]).unwrap();
                conn.execute("INSERT INTO reaction_legacy_unscoped(id,sender_id,sent_at_ns,user_id,emoji,created_at) VALUES(?1,'user-a',?2,'legacy-voter','👍','now')",rusqlite::params![format!("legacy-{scope}"),timestamp]).unwrap();
                conn.execute("INSERT INTO reaction_legacy_pending(author_id,target_sent_at,kind,actor_id,emoji,conversation_id,body,action_ts,remove,received_at) VALUES('user-a',?1,'reaction',?2,'👍',NULL,NULL,1,0,1)",rusqlite::params![timestamp,format!("legacy-pending-{scope}")]).unwrap();
            }
            // Ambiguity spans retained and excluded history. Counting only
            // retained messages would incorrectly leak this unscoped vote.
            insert_message(&conn, "amb-kept", "retained ambiguous target", 60);
            insert_message(
                &conn,
                "excluded-amb-target",
                "excluded ambiguous target",
                60,
            );
            conn.execute("UPDATE messages SET conversation_type='anon_dm',conversation_id='excluded-amb-target' WHERE id='excluded-amb-target'",[]).unwrap();
            for (marker, timestamp) in [
                ("excluded-ambiguous-evidence", 60),
                ("excluded-orphan-evidence", 70),
            ] {
                conn.execute("INSERT INTO reaction_legacy_unscoped(id,sender_id,sent_at_ns,user_id,emoji,created_at) VALUES(?1,'user-a',?2,?1,'👍','now')",rusqlite::params![marker,timestamp]).unwrap();
                conn.execute("INSERT INTO reaction_legacy_pending(author_id,target_sent_at,kind,actor_id,emoji,conversation_id,body,action_ts,remove,received_at) VALUES('user-a',?1,'reaction',?2,'👍',NULL,NULL,1,0,1)",rusqlite::params![timestamp,marker]).unwrap();
            }
            // An eligible-looking scope without its actual target is excluded.
            conn.execute_batch("INSERT INTO unified_reactions VALUES('excluded-fake-scope','keep-channel','channel','user-a',999,'voter','👍','now');
                INSERT INTO reaction_scope_unresolved VALUES('excluded-fake-scope','channel','user-a',999);
                INSERT INTO pending_scoped_reactions VALUES('excluded-fake-scope','channel','user-a',999,'actor','👍',0,1,1);
                INSERT INTO reaction_action_versions VALUES('excluded-fake-scope','channel','user-a',999,'actor','👍',1,0);
                INSERT INTO reaction_scoped_quarantine VALUES('excluded-fake-scope','keep-channel','channel','user-a',999,'unknown','X','old');
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
            "excluded-amb-target",
            "excluded-ambiguous-evidence",
            "excluded-orphan-evidence",
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
            "reaction_scope_unresolved",
            "pending_scoped_reactions",
            "reaction_action_versions",
            "reaction_legacy_unscoped",
            "reaction_legacy_pending",
            "reaction_scoped_quarantine",
            "pending_reaction_add_selections",
        ] {
            assert_eq!(
                counts.get(table),
                Some(&3),
                "exact eligible controls for {table}"
            );
        }
        assert_eq!(counts.get("messages"), Some(&4));
        let conn = db.connection().unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM reaction_legacy_unscoped", [], |r| r
                .get::<_, i64>(
                0
            ))
            .unwrap(),
            7,
            "export must not delete quarantine"
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM reaction_legacy_pending", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            7,
            "export must not delete pending evidence"
        );
    }

    /// Real pre-v084 table headers, not migration-only SQL. Deferred votes
    /// must survive even when their table arrives before message ownership.
    #[test]
    fn legacy_payload_recovers_only_unique_votes_and_quarantines_the_rest() {
        use crate::domain::ids::{ChannelId, UserId};
        use crate::domain::message::ConversationType;
        use crate::domain::reaction::ReactionScope;
        use crate::storage::repositories::unified_message_repo::UnifiedMessageRepository;
        use std::sync::Arc;

        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();
        let src = test_db(src_dir.path());
        let dst = Arc::new(test_db(dst_dir.path()));
        let author = UserId::new();
        let voter = UserId::new();
        let other = UserId::new();
        let conv_a = ChannelId::new();
        let conv_b = ChannelId::new();
        let scope_a = ReactionScope::new(conv_a, ConversationType::Channel);
        let scope_b = ReactionScope::new(conv_b, ConversationType::Channel);
        let mut payload = Vec::new();
        push_frame(
            &mut payload,
            frame::Payload::Info(BackupInfo {
                payload_version: PAYLOAD_VERSION,
                user_id: author.to_string(),
                created_at_ms: 0,
                reaction_selection_version: 0,
            }),
        );
        {
            let conn = src.connection().unwrap();
            // Construct the actual old six-column schema and dump its header
            // and rows through the production frame serializer.
            conn.execute_batch("DROP TABLE unified_reactions;
                CREATE TABLE unified_reactions(id TEXT PRIMARY KEY,sender_id TEXT NOT NULL,
                sent_at_ns INTEGER NOT NULL,user_id TEXT NOT NULL,emoji TEXT NOT NULL,created_at TEXT NOT NULL);").unwrap();
            for (id, timestamp) in [("known", 10), ("ambiguous", 20), ("orphan", 30)] {
                conn.execute(
                    "INSERT INTO unified_reactions VALUES(?1,?2,?3,?4,'👍','2026-01-01')",
                    rusqlite::params![id, author.to_string(), timestamp, voter.to_string()],
                )
                .unwrap();
            }
            insert_message(&conn, "unique", "known history", 10);
            insert_message(&conn, "amb-a", "ambiguous history a", 20);
            insert_message(&conn, "amb-b", "ambiguous history b", 20);
            conn.execute(
                "UPDATE messages SET sender_id=?1,conversation_id=CASE WHEN id='amb-b' THEN ?3 ELSE ?2 END",
                rusqlite::params![author.to_string(), conv_a.to_string(), conv_b.to_string()],
            )
            .unwrap();
            conn.execute(
                "UPDATE messages SET conversation_id=?1 WHERE id='amb-b'",
                [conv_b.to_string()],
            )
            .unwrap();
            conn.execute("UPDATE messages SET reaction_counts_blob=X'CAFE',current_user_reactions_blob=X'BEEF' WHERE sent_at=20", []).unwrap();
            let mut rows = 0;
            dump_table(
                &conn,
                &mut payload,
                "unified_reactions",
                "SELECT * FROM unified_reactions",
                &[],
                &mut rows,
                &|_| Ok(true),
            )
            .unwrap();
            dump_table(
                &conn,
                &mut payload,
                "messages",
                "SELECT * FROM messages",
                &[],
                &mut rows,
                &|_| Ok(true),
            )
            .unwrap();
            conn.execute(
                "INSERT INTO reaction_action_versions VALUES(?1,'channel',?2,10,?3,'👍',10,0)",
                rusqlite::params![conv_a.to_string(), author.to_string(), voter.to_string()],
            )
            .unwrap();
            dump_table(
                &conn,
                &mut payload,
                "reaction_action_versions",
                "SELECT * FROM reaction_action_versions",
                &[],
                &mut rows,
                &|_| Ok(true),
            )
            .unwrap();
            assert_eq!(rows, 7);
        }
        {
            let mut conn = dst.connection().unwrap();
            insert_message(&conn, "local-history", "keep local history", 99);
            conn.execute("INSERT INTO channels(id,channel_type,name,owner_id,created_at,updated_at) VALUES(?1,0,'local',?2,'now','now')",rusqlite::params![conv_a.to_string(),author.to_string()]).unwrap();
            conn.execute("INSERT INTO drafts(id,channel_id,content,created_at,updated_at) VALUES('local-draft',?1,'keep draft','now','now')",[conv_a.to_string()]).unwrap();
            let result = import_frames(&mut conn, &payload).unwrap();
            assert_eq!(result.rows_seen, 7);
            assert_eq!(result.rows_inserted, 7);
            assert_eq!(import_frames(&mut conn, &payload).unwrap().rows_inserted, 0);
            assert_eq!(conn.query_row("SELECT count(*) FROM unified_reactions WHERE id='known' AND conversation_id=?1",[conv_a.to_string()],|r|r.get::<_,i64>(0)).unwrap(),1);
            assert_eq!(
                conn.query_row("SELECT count(*) FROM reaction_legacy_unscoped", [], |r| r
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
                2
            );
            assert_eq!(
                conn.query_row("SELECT count(*) FROM reaction_scope_unresolved", [], |r| {
                    r.get::<_, i64>(0)
                })
                .unwrap(),
                2
            );
        }
        let repo = UnifiedMessageRepository::new(Arc::clone(&dst));
        assert!(
            repo.add_reaction(&scope_a, &author, 10, &other, &voter, "🎉")
                .unwrap()
        );
        assert!(
            repo.remove_reaction(&scope_a, &author, 10, &other, &voter, "🎉")
                .unwrap()
        );
        for scope in [&scope_a, &scope_b] {
            assert!(
                repo.add_reaction(scope, &author, 20, &other, &voter, "🎉")
                    .is_err()
            );
            assert!(
                repo.remove_reaction(scope, &author, 20, &voter, &voter, "👍")
                    .is_err()
            );
        }
        {
            let conn = dst.connection().unwrap();
            assert_eq!(
                conn.query_row(
                    "SELECT count(*) FROM unified_reactions WHERE id='known'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                1,
                "add/remove must retain recovered old vote"
            );
            for id in ["amb-a", "amb-b"] {
                let blobs:(Vec<u8>,Vec<u8>) = conn.query_row("SELECT reaction_counts_blob,current_user_reactions_blob FROM messages WHERE id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
                assert_eq!(blobs, (vec![0xca, 0xfe], vec![0xbe, 0xef]));
            }
            assert_eq!(
                conn.query_row(
                    "SELECT content FROM drafts WHERE id='local-draft'",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
                "keep draft"
            );
            assert_eq!(
                conn.query_row(
                    "SELECT content_json FROM messages WHERE id='local-history'",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
                "{\"text\":\"keep local history\"}"
            );
            // Orphans are not assigned by guesswork; later history arrival
            // activates the migration's ambiguity trigger instead.
            conn.execute("INSERT INTO messages(id,conversation_type,conversation_id,sender_id,content_type,content_json,created_at,updated_at,sent_at) VALUES('late-orphan','channel',?1,?2,0,'{}','now','now',30)",rusqlite::params![conv_b.to_string(),author.to_string()]).unwrap();
        }
        assert!(
            repo.add_reaction(&scope_b, &author, 30, &other, &voter, "🎉")
                .is_err()
        );
        assert!(
            repo.remove_reaction(&scope_b, &author, 30, &voter, &voter, "👍")
                .is_err()
        );
        assert_eq!(
            dst.connection()
                .unwrap()
                .query_row("SELECT count(*) FROM reaction_legacy_unscoped", [], |r| r
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            2
        );
    }

    #[test]
    fn legacy_reaction_null_evidence_is_an_error_not_a_silent_skip() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db(dir.path());
        let mut payload = Vec::new();
        push_frame(
            &mut payload,
            frame::Payload::Info(BackupInfo {
                payload_version: PAYLOAD_VERSION,
                user_id: "owner".into(),
                created_at_ms: 0,
                reaction_selection_version: 0,
            }),
        );
        push_frame(
            &mut payload,
            frame::Payload::Table(TableHeader {
                table: "unified_reactions".into(),
                columns: [
                    "id",
                    "sender_id",
                    "sent_at_ns",
                    "user_id",
                    "emoji",
                    "created_at",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            }),
        );
        push_frame(
            &mut payload,
            frame::Payload::Row(RowFrame {
                values: vec![
                    ColumnValue {
                        kind: Some(column_value::Kind::Text("legacy".into())),
                    },
                    ColumnValue {
                        kind: Some(column_value::Kind::Text("author".into())),
                    },
                    ColumnValue {
                        kind: Some(column_value::Kind::Integer(10)),
                    },
                    ColumnValue {
                        kind: Some(column_value::Kind::Text("voter".into())),
                    },
                    ColumnValue {
                        kind: Some(column_value::Kind::Text("👍".into())),
                    },
                    ColumnValue { kind: None },
                ],
            }),
        );
        let mut conn = db.connection().unwrap();
        assert!(matches!(
            import_frames(&mut conn, &payload),
            Err(StorageError::InvalidData(_))
        ));
        assert_eq!(
            conn.query_row("SELECT count(*) FROM reaction_legacy_unscoped", [], |r| r
                .get::<_, i64>(
                0
            ))
            .unwrap(),
            0
        );
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
