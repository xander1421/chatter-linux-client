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
//! - Countdown state and deletion tombstones for history restore.
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
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, OptionalExtension};

use crate::error::StorageError;
use crate::storage::repositories::settings_repo::keys;
use crate::storage::repositories::unified_message_repo::reaction_projection;
use std::collections::{HashMap, HashSet};

/// Payload schema version (independent of the container's format_version).
/// Bump this for every change to backup content, including exported tables,
/// columns, row selection, or the meaning of stored values. Older builds must
/// reject newer payloads rather than silently importing only familiar fields.
pub const PAYLOAD_VERSION: u32 = 4;

/// Whitelisted tables, in FK-safe import order (parents before children:
/// `profiles`/`friendships` reference `users`, `memberships` references
/// `channels`, `threads` references `messages`).
pub const BACKUP_TABLES: &[&str] = &[
    "users",
    "profiles",
    "friendships",
    "channels",
    "memberships",
    "deleted_messages",
    "dm_receive_tombstones",
    "pending_message_modifiers",
    "ephemeral_messages",
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

const BACKUP_DELETE_MODIFIERS: &str = "('channel','server_dm','call:server_dm')";

const LIFECYCLE_TABLES: &[&str] = &[
    "deleted_messages",
    "dm_receive_tombstones",
    "pending_message_modifiers",
    "ephemeral_messages",
];

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

fn row_value<'a>(columns: &[String], values: &'a [SqlValue], column: &str) -> &'a SqlValue {
    columns
        .iter()
        .position(|name| name == column)
        .map_or(&SqlValue::Null, |index| &values[index])
}

fn text_value(value: &SqlValue) -> Option<&str> {
    match value {
        SqlValue::Text(text) => Some(text),
        _ => None,
    }
}

struct MessageExpiry {
    ttl: i64,
    expires_at: String,
    deadline: chrono::DateTime<chrono::FixedOffset>,
    read_at: Option<String>,
    started: bool,
}

impl MessageExpiry {
    fn from_row(columns: &[String], values: &[SqlValue]) -> Option<Self> {
        let get = |name| row_value(columns, values, name);
        let SqlValue::Integer(ttl) = get("ttl_seconds") else {
            return None;
        };
        u32::try_from(*ttl).ok()?;
        let SqlValue::Integer(started @ (0 | 1)) = get("countdown_started") else {
            return None;
        };
        let expires_at = text_value(get("expires_at"))?;
        let deadline = chrono::DateTime::parse_from_rfc3339(expires_at).ok()?;
        Some(Self {
            ttl: *ttl,
            expires_at: expires_at.to_owned(),
            deadline,
            read_at: text_value(get("read_at")).map(str::to_owned),
            started: *started == 1,
        })
    }

    fn expired(&self) -> bool {
        // Unread rows have a placeholder date that is replaced on read.
        self.started && self.deadline <= chrono::Utc::now()
    }

    fn insert(&self, conn: &Connection, id: &str) -> Result<(), StorageError> {
        conn.execute(
            "INSERT INTO ephemeral_messages (message_id,ttl_seconds,expires_at,read_at,countdown_started)
             VALUES (?1,?2,?3,?4,?5)",
            rusqlite::params![id, self.ttl, self.expires_at, self.read_at, self.started],
        )?;
        Ok(())
    }
}

const NO_MESSAGE_TOMBSTONE: &str =
    "NOT EXISTS(SELECT 1 FROM deleted_messages WHERE id=m.id)
     AND NOT (m.conversation_type='server_dm' AND EXISTS(SELECT 1 FROM dm_receive_tombstones
         WHERE owner_id IN (:owner, (SELECT json_extract(value,'$') FROM settings WHERE key=:user_key))
           AND conversation_id=m.conversation_id AND sender_id=m.sender_id AND sent_at=m.sent_at))
     AND NOT EXISTS(SELECT 1 FROM pending_message_modifiers WHERE kind='delete'
         AND (conversation_id IS NULL OR conversation_id=m.conversation_id)
         AND ((body=m.conversation_type AND author_id=m.sender_id AND target_sent_at=m.sent_at)
              OR (m.conversation_type='server_dm' AND body='call:server_dm' AND author_id=m.id AND target_sent_at=0)))";

fn live_message_predicate() -> String {
    format!(
        "m.is_deleted=0 AND m.conversation_type IN {MESSAGE_CONVERSATION_TYPES}
        AND NOT EXISTS(SELECT 1 FROM ephemeral_messages e WHERE e.message_id=m.id
            AND e.countdown_started<>0
            AND COALESCE(julianday(e.expires_at) > julianday(:now),0)=0)
        AND {NO_MESSAGE_TOMBSTONE}"
    )
}

fn retained_messages_cte() -> String {
    format!(
        "WITH retained_messages AS NOT MATERIALIZED (SELECT m.* FROM messages m WHERE {})",
        live_message_predicate()
    )
}

const EXCLUDED_REPLY_PARENT: &str = "m.reply_to IS NOT NULL
    AND NOT EXISTS (SELECT 1 FROM retained_messages parent WHERE parent.id=m.reply_to)
    AND NOT EXISTS (
        SELECT 1 FROM retained_messages parent
        WHERE parent.conversation_type=m.conversation_type
          AND parent.conversation_id=m.conversation_id
          AND parent.sender_id=m.reply_to_author_id
          AND parent.sent_at>0 AND backup_reply_id(parent.sent_at)=m.reply_to
    )";

const REDACTED_REPLY_CONTENT: &str = "CASE WHEN json_valid(m.content_json)
    AND json_type(m.content_json,'$.quote')='object'
    THEN json_set(m.content_json,'$.quote.preview','') ELSE m.content_json END";

fn register_backup_reply_id(conn: &Connection) -> Result<(), StorageError> {
    use rusqlite::functions::FunctionFlags;

    conn.create_scalar_function(
        "backup_reply_id",
        1,
        FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let sent_at = ctx.get::<i64>(0)?;
            Ok(crate::domain::ids::MessageId::from_timestamp(sent_at as u64).to_string())
        },
    )?;
    Ok(())
}

fn message_tombstoned(
    conn: &Connection,
    columns: &[String],
    values: &[SqlValue],
    archive_owner: &str,
) -> Result<bool, StorageError> {
    let message = |column| row_value(columns, values, column);
    Ok(conn.prepare_cached(&format!(
        "SELECT NOT ({NO_MESSAGE_TOMBSTONE}) FROM
         (SELECT :id AS id,:kind AS conversation_type,:conv AS conversation_id,
                 :sender AS sender_id,:sent AS sent_at) m"
    ))?.query_row(
        rusqlite::named_params! {
            ":id": message("id"), ":kind": message("conversation_type"),
            ":conv": message("conversation_id"), ":sender": message("sender_id"),
            ":sent": message("sent_at"), ":owner": archive_owner, ":user_key": keys::CURRENT_USER_ID,
        },
        |row| row.get(0),
    )?)
}

fn attachment_ids(blob: &[u8]) -> impl Iterator<Item = String> {
    use crate::domain::ids::AttachmentId;
    use crate::storage::serialization::db_deserialize;

    db_deserialize::<Vec<AttachmentId>>(blob)
        .unwrap_or_default()
        .into_iter()
        .map(|id| id.to_string())
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
    params: impl rusqlite::Params,
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
    if conn.is_autocommit() {
        let tx = conn.unchecked_transaction()?;
        let result = dump_snapshot(&tx, user_id)?;
        tx.commit()?;
        return Ok(result);
    }
    dump_snapshot(conn, user_id)
}

fn dump_snapshot(conn: &Connection, user_id: &str) -> Result<(Vec<u8>, usize), StorageError> {
    let mut out = Vec::new();
    let mut rows = 0usize;
    let now = chrono::Utc::now();
    let cutoff = now.to_rfc3339();
    register_backup_reply_id(conn)?;
    let retained = retained_messages_cte();
    let history_params: &[(&str, &dyn rusqlite::ToSql)] = &[
        (":now", &cutoff),
        (":owner", &user_id),
        (":user_key", &keys::CURRENT_USER_ID),
    ];
    let owner_params: &[(&str, &dyn rusqlite::ToSql)] = &[(":owner", &user_id)];
    push_frame(
        &mut out,
        frame::Payload::Info(BackupInfo {
            payload_version: PAYLOAD_VERSION,
            user_id: user_id.to_string(),
            created_at_ms: now.timestamp_millis(),
        }),
    );

    for &table in BACKUP_TABLES {
        if table == "attachments" {
            dump_attachments(conn, &mut out, &retained, history_params, &mut rows)?;
            continue;
        }
        let mut cols = table_columns(conn, table)?.join(", ");
        let history = match table {
            "unified_reactions" =>
                "EXISTS (SELECT 1 FROM retained_messages m
                   WHERE m.conversation_type=unified_reactions.conversation_type AND m.conversation_id=unified_reactions.conversation_id
                   AND m.sender_id=unified_reactions.sender_id AND m.sent_at=unified_reactions.sent_at_ns)".to_owned(),
            "pending_scoped_reactions" | "reaction_action_versions" | "reaction_actor_selections" | "pending_reaction_add_selections" =>
                format!("EXISTS (SELECT 1 FROM retained_messages m
                   WHERE m.conversation_type={table}.conversation_type AND m.conversation_id={table}.conversation_id
                   AND m.sender_id={table}.author_id AND m.sent_at={table}.target_sent_at)"),
            "ephemeral_messages" => "message_id IN (SELECT id FROM retained_messages)".to_owned(),
            "threads" => "root_message_id IN (SELECT id FROM retained_messages)".to_owned(),
            "messages" => {
                cols = table_columns(conn, table)?
                    .iter()
                    .map(|col| match col.as_str() {
                        "reply_to_text" | "reply_to_thumbnail" | "reply_to_thumbnail_mime" =>
                            format!("CASE WHEN {EXCLUDED_REPLY_PARENT} THEN NULL ELSE m.{col} END"),
                        "content_json" => format!("CASE WHEN {EXCLUDED_REPLY_PARENT}
                            THEN {REDACTED_REPLY_CONTENT} ELSE m.content_json END"),
                        _ => format!("m.{col}"),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                "1".to_owned()
            }
            _ => String::new(),
        };
        let (select, params) = if !history.is_empty() {
            let source = if table == "messages" {
                "retained_messages m"
            } else {
                table
            };
            (
                format!("{retained} SELECT {cols} FROM {source} WHERE {history}"),
                history_params,
            )
        } else {
            match table {
                "dm_receive_tombstones" => (
                    format!("SELECT {cols} FROM {table} WHERE owner_id=:owner"),
                    owner_params,
                ),
                "pending_message_modifiers" => (
                    format!(
                        "SELECT {cols} FROM {table} WHERE kind='delete' AND body IN {BACKUP_DELETE_MODIFIERS}"
                    ),
                    &[][..],
                ),
                _ => (format!("SELECT {cols} FROM {table}"), &[][..]),
            }
        };
        dump_table(conn, &mut out, table, &select, params, &mut rows, &|_| {
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
        params.as_slice(),
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
    retained: &str,
    params: &[(&str, &dyn rusqlite::ToSql)],
    rows_out: &mut usize,
) -> Result<(), StorageError> {
    let mut referenced = HashSet::new();
    {
        let mut stmt = conn.prepare(&format!(
            "{retained} SELECT attachments_blob FROM retained_messages"
        ))?;
        let mut rows = stmt.query(params)?;
        while let Some(row) = rows.next()? {
            let blob: Vec<u8> = row.get(0)?;
            referenced.extend(attachment_ids(&blob));
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
        [],
        rows_out,
        &|row| Ok(referenced.contains(&row.get::<_, String>(id_index)?)),
    )
}

fn read_frame(buf: &mut &[u8]) -> Result<Frame, StorageError> {
    Frame::decode_length_delimited(buf)
        .map_err(|error| StorageError::InvalidData(format!("backup frame undecodable: {error}")))
}

/// Import a decrypted payload. Existing PK/dedup conflicts are skipped, so
/// rows that already exist locally (same PK / dedup key) are skipped — a
/// restore can never overwrite newer local data. Unknown tables and columns
/// reject the payload; the transaction then rolls back without partial import.
pub fn import_frames(conn: &mut Connection, payload: &[u8]) -> Result<ImportSummary, StorageError> {
    let mut buf = payload;
    let Some(frame::Payload::Info(info)) = read_frame(&mut buf)?.payload else {
        return Err(StorageError::InvalidData(
            "backup payload must start with an info frame".into(),
        ));
    };
    if info.payload_version != PAYLOAD_VERSION {
        return Err(StorageError::UnsupportedBackupVersion {
            found: info.payload_version,
            expected: PAYLOAD_VERSION,
        });
    }
    let archive_owner = info.user_id;

    let mut summary = ImportSummary::default();

    // Per-table insert state, rebuilt at each TableHeader frame.
    struct TableState {
        table: String,
        insert_sql: String,
        scoped_indices: Option<[usize; 6]>,
        ledger_table: Option<String>,
        columns: Vec<String>,
    }
    let tx = conn.transaction()?;
    // A restore merges history, never overrides a locally retained actor. The
    // guard is per actor, not per emoji, including removed and held selections.
    let local_actors: HashSet<((String, String, String, i64), String)> = tx
        .prepare(
            "SELECT conversation_id,conversation_type,author_id,target_sent_at,actor_id FROM reaction_action_versions
            UNION SELECT conversation_id,conversation_type,author_id,target_sent_at,actor_id FROM reaction_actor_selections
            UNION SELECT conversation_id,conversation_type,sender_id,sent_at_ns,user_id FROM unified_reactions
            UNION SELECT conversation_id,conversation_type,author_id,target_sent_at,actor_id FROM pending_scoped_reactions
            UNION SELECT conversation_id,conversation_type,author_id,target_sent_at,actor_id FROM pending_reaction_add_selections",
        )?
        .query_map([], |r| Ok(((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?), r.get(4)?)))?
        .collect::<Result<_, _>>()?;
    let mut ledgers = Vec::new();
    let mut scoped_votes = Vec::new();
    let mut attachments_and_threads = Vec::new();
    let mut retained_ids = HashSet::new();
    let mut retained_attachments = HashSet::new();
    let mut retained_scopes = HashSet::new();
    let retained_duplicate = format!(
        "SELECT m.id,m.attachments_blob FROM messages m WHERE {}
         AND m.conversation_type=:kind AND m.conversation_id=:conv AND m.sender_id IS :sender
         AND m.sent_at=:sent AND (m.id=:id OR (:sender IS NOT NULL AND :sent>0))
         ORDER BY m.id=:id DESC LIMIT 1",
        live_message_predicate()
    );
    let now = chrono::Utc::now().to_rfc3339();
    let mut current: Option<TableState> = None;
    let mut expiry_by_id = HashMap::new();
    let mut inserted_replies = Vec::new();
    let mut saw_messages = false;
    while buf.has_remaining() {
        match read_frame(&mut buf)?.payload {
            Some(frame::Payload::Info(_)) => {
                return Err(StorageError::InvalidData(
                    "backup payload has duplicate info frames".into(),
                ));
            }
            Some(frame::Payload::Table(th)) => {
                let allowed = BACKUP_TABLES.contains(&th.table.as_str()) || th.table == "settings";
                if !allowed {
                    return Err(StorageError::InvalidData(format!(
                        "backup contains unknown table '{}'",
                        th.table
                    )));
                }
                let table = th.table.as_str();
                if LIFECYCLE_TABLES.contains(&table) && saw_messages {
                    return Err(StorageError::InvalidData(
                        "backup lifecycle records must precede messages".into(),
                    ));
                }
                saw_messages |= table == "messages";
                let live_cols = table_columns(&tx, table)?;
                let mut seen_columns = HashSet::new();
                for col in &th.columns {
                    if !live_cols.contains(col) {
                        return Err(StorageError::InvalidData(format!(
                            "backup table '{table}' contains unknown column '{col}'"
                        )));
                    }
                    if !seen_columns.insert(col) {
                        return Err(StorageError::InvalidData(format!(
                            "backup table '{table}' repeats column '{col}'"
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
                    table: th.table,
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
                let get = |column| row_value(&state.columns, &params, column);
                match state.table.as_str() {
                    "settings"
                        if !text_value(get("key"))
                            .is_some_and(|key| OWN_PROFILE_SETTINGS_KEYS.contains(&key)) =>
                    {
                        continue;
                    }
                    "ephemeral_messages" => {
                        let id = text_value(get("message_id")).ok_or_else(|| {
                            StorageError::InvalidData("backup countdown lacks message id".into())
                        })?;
                        let expiry =
                            MessageExpiry::from_row(&state.columns, &params).ok_or_else(|| {
                                StorageError::InvalidData("invalid backup countdown".into())
                            })?;
                        if expiry_by_id.insert(id.to_owned(), expiry).is_some() {
                            return Err(StorageError::InvalidData(
                                "duplicate backup countdown".into(),
                            ));
                        }
                        continue;
                    }
                    "dm_receive_tombstones"
                        if text_value(get("owner_id")) != Some(archive_owner.as_str()) =>
                    {
                        continue;
                    }
                    "pending_message_modifiers"
                        if text_value(get("kind")) != Some("delete")
                            || !matches!(
                                text_value(get("body")),
                                Some("channel" | "server_dm" | "call:server_dm")
                            ) =>
                    {
                        continue;
                    }
                    "attachments" | "threads" => {
                        let parent = if state.table == "attachments" {
                            "id"
                        } else {
                            "root_message_id"
                        };
                        if let Some(id) = text_value(get(parent)) {
                            attachments_and_threads.push((
                                state.table == "attachments",
                                id.to_owned(),
                                state.insert_sql.clone(),
                                params,
                            ));
                        }
                        continue;
                    }
                    "messages" => {
                        let id = text_value(get("id")).ok_or_else(|| {
                            StorageError::InvalidData("backup message lacks id".into())
                        })?;
                        let kind = text_value(get("conversation_type")).ok_or_else(|| {
                            StorageError::InvalidData(
                                "backup message lacks conversation type".into(),
                            )
                        })?;
                        let expiry = expiry_by_id.get(id);
                        if !matches!(kind, "channel" | "server_dm" | "saved_messages")
                            || matches!(get("is_deleted"), SqlValue::Integer(value) if *value != 0)
                            || message_tombstoned(&tx, &state.columns, &params, &archive_owner)?
                            || expiry.is_some_and(MessageExpiry::expired)
                        {
                            continue;
                        }
                        let inserted = tx
                            .prepare_cached(&state.insert_sql)?
                            .execute(rusqlite::params_from_iter(&params))?;
                        summary.rows_inserted += inserted;
                        if inserted != 0 {
                            if let Some(expiry) = expiry {
                                expiry.insert(&tx, id)?;
                                summary.rows_inserted += 1;
                            }
                            if text_value(get("reply_to")).is_some() {
                                inserted_replies.push(id.to_owned());
                            }
                        }
                        let retained = if inserted != 0 {
                            Some((
                                id.to_owned(),
                                match get("attachments_blob") {
                                    SqlValue::Blob(blob) => blob.clone(),
                                    _ => Vec::new(),
                                },
                            ))
                        } else {
                            tx.prepare_cached(&retained_duplicate)?.query_row(
                                rusqlite::named_params! {
                                    ":id": id, ":kind": kind, ":conv": get("conversation_id"),
                                    ":sender": get("sender_id"), ":sent": get("sent_at"),
                                    ":owner": &archive_owner, ":user_key": keys::CURRENT_USER_ID, ":now": &now,
                                },
                                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
                            ).optional()?
                        };
                        if let Some((retained_id, blob)) = retained {
                            retained_ids.insert(retained_id);
                            retained_attachments.extend(attachment_ids(&blob));
                            if let (Some(conv), Some(sender), SqlValue::Integer(sent_at)) = (
                                text_value(get("conversation_id")),
                                text_value(get("sender_id")),
                                get("sent_at"),
                            ) {
                                retained_scopes.insert((
                                    conv.to_owned(),
                                    kind.to_owned(),
                                    sender.to_owned(),
                                    *sent_at,
                                ));
                            }
                        }
                        continue;
                    }
                    _ => {}
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
                    let scope = (
                        text(indices[0])?,
                        text(indices[1])?,
                        text(indices[2])?,
                        timestamp,
                    );
                    let actor = (scope, text(indices[4])?);
                    let emoji = text(indices[5])?;
                    if state.ledger_table.is_some() {
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
                            ledgers.push((actor.0, state.insert_sql.clone(), params));
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
    if !inserted_replies.is_empty() {
        // A parent retained by the exporter can expire or be deleted locally
        // before the archive is restored.
        register_backup_reply_id(&tx)?;
        let retained = retained_messages_cte();
        let mut redact = tx.prepare(&format!(
            "{retained} UPDATE messages AS m SET reply_to_text=NULL,
             reply_to_thumbnail=NULL,reply_to_thumbnail_mime=NULL,
             content_json={REDACTED_REPLY_CONTENT} WHERE m.id=:id AND {EXCLUDED_REPLY_PARENT}"
        ))?;
        for id in inserted_replies {
            redact.execute(rusqlite::named_params! {
                ":id": &id, ":now": &now, ":owner": &archive_owner, ":user_key": keys::CURRENT_USER_ID,
            })?;
        }
    }
    for (attachment, id, sql, values) in attachments_and_threads {
        let retained = if attachment {
            retained_attachments.contains(&id)
        } else {
            retained_ids.contains(&id)
        };
        if retained {
            summary.rows_inserted += tx
                .prepare_cached(&sql)?
                .execute(rusqlite::params_from_iter(values))?;
        }
    }
    // All action evidence is installed before any vote, regardless of frame
    // order. LWW merge applies only to actors absent before this restore.
    // Receipt metadata and duplicate vote IDs are sorted for stable re-export.
    ledgers.sort_by_key(|row| format!("{:?}", row));
    for (scope, sql, values) in ledgers {
        if retained_scopes.contains(&scope) {
            summary.rows_inserted += tx
                .prepare_cached(&sql)?
                .execute(rusqlite::params_from_iter(values))?;
        }
    }
    // A vote is admitted only when the restored selection names it and that
    // selection's action is an add: the ledger, not the projection, orders actions.
    scoped_votes.sort_by_key(|row| format!("{:?}", row));
    for (sql, values, (scope, actor), emoji) in scoped_votes {
        if !retained_scopes.contains(&scope) {
            continue;
        }
        let selected: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM reaction_actor_selections s JOIN reaction_action_versions v ON v.conversation_id=s.conversation_id AND v.conversation_type=s.conversation_type AND v.author_id=s.author_id AND v.target_sent_at=s.target_sent_at AND v.actor_id=s.actor_id AND v.emoji=s.emoji WHERE s.conversation_id=?1 AND s.conversation_type=?2 AND s.author_id=?3 AND s.target_sent_at=?4 AND s.actor_id=?5 AND s.emoji=?6 AND v.remove=0 AND v.action_ts=s.action_ts)",rusqlite::params![scope.0,scope.1,scope.2,scope.3,actor,emoji],|r|r.get(0))?;
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

    #[test]
    fn history_backup_redacts_quotes_without_a_retained_parent() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db(dir.path());
        let conn = db.connection().unwrap();
        insert_message(&conn, "deleted-parent", "original message", 1);
        insert_message(&conn, "live-parent", "retained message", 2);
        let synthetic = crate::domain::ids::MessageId::from_timestamp(2).to_string();
        let replies = [
            ("deleted", "deleted-parent", "live-parent", "user-a", false),
            ("missing", "missing-parent", "live-parent", "user-a", false),
            ("live", "live-parent", "live-parent", "user-a", true),
            (
                "synthetic",
                synthetic.as_str(),
                "live-parent",
                "user-a",
                true,
            ),
            (
                "wrong-chat",
                synthetic.as_str(),
                "other-chat",
                "user-a",
                false,
            ),
            (
                "wrong-author",
                synthetic.as_str(),
                "live-parent",
                "user-b",
                false,
            ),
        ];
        for (index, (id, parent, chat, author, _)) in replies.into_iter().enumerate() {
            insert_message(&conn, id, "reply", index as i64 + 3);
            let content = serde_json::json!({
                "text": format!("reply body {id}"),
                "quote": { "preview": format!("{id} quote preview") },
            });
            conn.execute(
                "UPDATE messages SET reply_to=?2, reply_to_text=?3, reply_to_thumbnail=?4,
                    reply_to_thumbnail_mime=?5, content_json=?6, conversation_id=?7,
                    reply_to_author_id=?8 WHERE id=?1",
                rusqlite::params![
                    id,
                    parent,
                    format!("{id} cached text"),
                    format!("{id} thumbnail").into_bytes(),
                    format!("image/x-{id}"),
                    content.to_string(),
                    chat,
                    author,
                ],
            )
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO deleted_messages VALUES ('deleted-parent');
             DELETE FROM messages WHERE id='deleted-parent';",
        )
        .unwrap();

        // Check the serialized bytes before import can clear a tombstoned quote.
        let (payload, _) = dump_frames(&conn, "owner").unwrap();
        let contains = |marker: &str| {
            payload
                .windows(marker.len())
                .any(|bytes| bytes == marker.as_bytes())
        };
        for (id, _, _, _, retained) in replies {
            assert!(contains(&format!("reply body {id}")));
            for marker in [
                format!("{id} cached text"),
                format!("{id} quote preview"),
                format!("{id} thumbnail"),
                format!("image/x-{id}"),
            ] {
                assert_eq!(contains(&marker), retained, "{marker}");
            }
        }

        let dst_dir = tempfile::tempdir().unwrap();
        let dst = test_db(dst_dir.path());
        let mut dst_conn = dst.connection().unwrap();
        dst_conn
            .execute("INSERT INTO deleted_messages VALUES ('live-parent')", [])
            .unwrap();
        import_frames(&mut dst_conn, &payload).unwrap();
        for id in ["live", "synthetic"] {
            let redacted: bool = dst_conn.query_row(
                "SELECT reply_to_text IS NULL AND reply_to_thumbnail IS NULL
                    AND reply_to_thumbnail_mime IS NULL AND json_extract(content_json,'$.quote.preview')=''
                 FROM messages WHERE id=?1",
                [id],
                |row| row.get(0),
            ).unwrap();
            assert!(redacted, "{id}");
        }
    }

    #[test]
    fn history_restore_applies_only_profile_settings() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db(dir.path());
        let mut conn = db.connection().unwrap();
        let mut payload = Vec::new();
        push_frame(
            &mut payload,
            frame::Payload::Info(BackupInfo {
                payload_version: PAYLOAD_VERSION,
                user_id: "owner".into(),
                created_at_ms: 0,
            }),
        );
        push_frame(
            &mut payload,
            frame::Payload::Table(TableHeader {
                table: "settings".into(),
                columns: ["value", "updated_at", "key"].map(str::to_owned).to_vec(),
            }),
        );
        for (key, value) in [(keys::APP_LOCK, "{}"), (keys::DISPLAY_NAME, "\"Alice\"")] {
            push_frame(
                &mut payload,
                frame::Payload::Row(RowFrame {
                    values: [value, "2026-01-01", key]
                        .map(|text| ColumnValue {
                            kind: Some(column_value::Kind::Text(text.into())),
                        })
                        .to_vec(),
                }),
            );
        }
        let summary = import_frames(&mut conn, &payload).unwrap();
        assert_eq!((summary.rows_inserted, summary.rows_skipped), (1, 1));
        let (key, value): (String, String) = conn
            .query_row("SELECT key,value FROM settings", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(
            (key.as_str(), value.as_str()),
            (keys::DISPLAY_NAME, "\"Alice\"")
        );

        conn.execute(
            "INSERT INTO settings(key,value,updated_at) VALUES (?1,'local','2026-01-01')",
            [keys::APP_LOCK],
        )
        .unwrap();
        let summary = import_frames(&mut conn, &payload).unwrap();
        assert_eq!((summary.rows_inserted, summary.rows_skipped), (0, 2));
        let value: String = conn
            .query_row(
                "SELECT value FROM settings WHERE key=?1",
                [keys::APP_LOCK],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(value, "local");
    }

    #[test]
    fn history_restore_preserves_countdowns_and_omits_deleted_messages() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();
        let src = test_db(src_dir.path());
        let dst = test_db(dst_dir.path());
        let payload = {
            let conn = src.connection().unwrap();
            for (i, id) in [
                "running",
                "unread",
                "expired",
                "local-id",
                "local-receipt",
                "backup-id",
                "backup-delete",
                "reply",
            ]
            .into_iter()
            .enumerate()
            {
                insert_message(&conn, id, "placeholder", i as i64 + 1);
            }
            conn.execute_batch(
                "UPDATE messages SET conversation_type='server_dm',conversation_id='peer';
                 UPDATE messages SET reply_to='expired',reply_to_text='expiring text' WHERE id='reply';
                 INSERT INTO threads(root_message_id,conversation_id) VALUES ('expired','peer');
                 INSERT INTO ephemeral_messages VALUES
                     ('running',3600,'2099-01-01T01:00:00+00:00','2099-01-01T00:00:00+00:00',1),
                     ('unread',3600,'2000-01-01T00:00:00+00:00',NULL,0),
                     ('expired',3600,'2000-01-01T01:00:00+00:00','2000-01-01T00:00:00+00:00',1);
                 INSERT INTO deleted_messages VALUES ('backup-id');
                 INSERT INTO pending_message_modifiers(author_id,target_sent_at,kind,conversation_id,body,received_at)
                   VALUES ('user-a',7,'delete','peer','server_dm',1);",
            ).unwrap();
            dump_frames(&conn, "owner").unwrap().0
        };
        let mut conn = dst.connection().unwrap();
        conn.execute_batch(
            "INSERT INTO deleted_messages VALUES ('local-id');
             INSERT INTO dm_receive_tombstones VALUES ('owner','peer','user-a',5,1);",
        )
        .unwrap();
        import_frames(&mut conn, &payload).unwrap();
        assert_eq!(import_frames(&mut conn, &payload).unwrap().rows_inserted, 0);

        let ids = conn
            .prepare("SELECT id FROM messages ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(ids, ["reply", "running", "unread"]);
        let running: (i64, String, String, bool) = conn.query_row(
            "SELECT ttl_seconds,expires_at,read_at,countdown_started FROM ephemeral_messages WHERE message_id='running'",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).unwrap();
        assert_eq!(
            running,
            (
                3600,
                "2099-01-01T01:00:00+00:00".into(),
                "2099-01-01T00:00:00+00:00".into(),
                true
            )
        );
        assert!(conn.query_row(
            "SELECT countdown_started=0 AND read_at IS NULL FROM ephemeral_messages WHERE message_id='unread'",
            [], |row| row.get::<_, bool>(0),
        ).unwrap());
        conn.execute("UPDATE messages SET is_read=1 WHERE id='unread'", [])
            .unwrap();
        crate::storage::repositories::ephemeral_repo::start_read_countdowns(&conn).unwrap();
        assert!(conn.query_row(
            "SELECT countdown_started=1 AND expires_at > '2000-01-01' FROM ephemeral_messages WHERE message_id='unread'",
            [], |row| row.get::<_, bool>(0),
        ).unwrap());
        assert!(
            conn.query_row(
                "SELECT reply_to_text IS NULL FROM messages WHERE id='reply'",
                [],
                |row| row.get::<_, bool>(0)
            )
            .unwrap()
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM threads", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn history_restore_refuses_every_noncurrent_version_without_inserting_rows() {
        let src_dir = tempfile::tempdir().unwrap();
        let src = test_db(src_dir.path());
        let src_conn = src.connection().unwrap();
        insert_message(&src_conn, "from-archive", "backup message", 1);
        let (payload, _) = dump_frames(&src_conn, "owner").unwrap();
        let mut frames = payload.as_slice();
        let Some(frame::Payload::Info(info)) = read_frame(&mut frames).unwrap().payload else {
            panic!("missing backup header");
        };

        let dst_dir = tempfile::tempdir().unwrap();
        let dst = test_db(dst_dir.path());
        let mut dst_conn = dst.connection().unwrap();
        insert_message(&dst_conn, "local", "local message", 2);
        for version in [0, 1, 2, 3, PAYLOAD_VERSION + 1, u32::MAX] {
            let mut unsupported = Vec::new();
            push_frame(
                &mut unsupported,
                frame::Payload::Info(BackupInfo {
                    payload_version: version,
                    ..info.clone()
                }),
            );
            unsupported.extend_from_slice(frames);
            assert!(matches!(
                import_frames(&mut dst_conn, &unsupported),
                Err(StorageError::UnsupportedBackupVersion { found, expected: PAYLOAD_VERSION }) if found == version
            ));
            let ids = dst_conn
                .prepare("SELECT id FROM messages")
                .unwrap()
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(ids, ["local"]);
        }

        assert_eq!(
            import_frames(&mut dst_conn, &payload)
                .unwrap()
                .rows_inserted,
            1
        );
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

    #[test]
    fn history_backup_related_rows_require_a_retained_message() {
        use crate::domain::attachment::{Attachment, FileMetadata};
        use crate::domain::ids::AttachmentId;
        use crate::storage::repositories::attachment_repo::AttachmentRepository;
        use crate::storage::serialization::db_serialize;
        use std::sync::Arc;

        let src_dir = tempfile::tempdir().unwrap();
        let src = Arc::new(test_db(src_dir.path()));
        let attachments = AttachmentRepository::new(src.clone());
        let cases = [
            "live",
            "soft-deleted",
            "expired",
            "backup-deleted",
            "locally-deleted",
        ];
        for id in cases {
            attachments
                .insert(&Attachment::new(
                    AttachmentId::from_cdn_key(id),
                    FileMetadata::new(id.into(), "image/png".into(), 1),
                ))
                .unwrap();
        }
        let conn = src.connection().unwrap();
        for (index, id) in cases.into_iter().enumerate() {
            let sent = index as i64 + 1;
            insert_message(&conn, id, id, sent);
            conn.execute(
                "UPDATE messages SET attachments_blob=?2 WHERE id=?1",
                rusqlite::params![
                    id,
                    db_serialize(&vec![AttachmentId::from_cdn_key(id)]).unwrap()
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO threads(root_message_id,conversation_id) VALUES (?1,?1)",
                [id],
            )
            .unwrap();
            conn.execute("INSERT INTO unified_reactions VALUES(?1,?1,'channel','user-a',?2,'actor','A','now')", rusqlite::params![id,sent]).unwrap();
            conn.execute("INSERT INTO pending_scoped_reactions VALUES(?1,'channel','user-a',?2,'actor','A',0,1,1)", rusqlite::params![id,sent]).unwrap();
            conn.execute("INSERT INTO reaction_action_versions VALUES(?1,'channel','user-a',?2,'actor','A',1,0)", rusqlite::params![id,sent]).unwrap();
            conn.execute("INSERT INTO reaction_actor_selections VALUES(?1,'channel','user-a',?2,'actor','A',1)", rusqlite::params![id,sent]).unwrap();
            conn.execute("INSERT INTO pending_reaction_add_selections VALUES(?1,'channel','user-a',?2,'actor','A',1)", rusqlite::params![id,sent]).unwrap();
        }
        conn.execute_batch(
            "UPDATE messages SET is_deleted=1 WHERE id='soft-deleted';
             INSERT INTO deleted_messages VALUES ('backup-deleted');
             INSERT INTO ephemeral_messages VALUES ('expired',60,'2000-01-01T00:01:00+00:00','2000-01-01T00:00:00+00:00',1);",
        ).unwrap();

        let related_tables = [
            ("messages", "id"),
            ("attachments", "id"),
            ("threads", "root_message_id"),
            ("unified_reactions", "conversation_id"),
            ("pending_scoped_reactions", "conversation_id"),
            ("reaction_action_versions", "conversation_id"),
            ("reaction_actor_selections", "conversation_id"),
            ("pending_reaction_add_selections", "conversation_id"),
        ];
        let current = dump_frames(&conn, "owner").unwrap().0;
        let mut counts = HashMap::<String, usize>::new();
        let mut table = String::new();
        let mut input = current.as_slice();
        while input.has_remaining() {
            match Frame::decode_length_delimited(&mut input).unwrap().payload {
                Some(frame::Payload::Table(header)) => table = header.table,
                Some(frame::Payload::Row(_)) => *counts.entry(table.clone()).or_default() += 1,
                _ => {}
            }
        }
        for (table, _) in related_tables {
            assert_eq!(counts.get(table), Some(&2), "{table}");
        }
        assert!(!counts.contains_key("ephemeral_messages"));

        // Exercise current-version input with rows for targets discarded at import.
        let mut unfiltered = Vec::new();
        push_frame(
            &mut unfiltered,
            frame::Payload::Info(BackupInfo {
                payload_version: PAYLOAD_VERSION,
                user_id: "owner".into(),
                created_at_ms: 0,
            }),
        );
        for table in ["deleted_messages", "ephemeral_messages"]
            .into_iter()
            .chain(related_tables.map(|(table, _)| table))
        {
            let cols = table_columns(&conn, table).unwrap().join(", ");
            dump_table(
                &conn,
                &mut unfiltered,
                table,
                &format!("SELECT {cols} FROM {table}"),
                [],
                &mut 0,
                &|_| Ok(true),
            )
            .unwrap();
        }
        let dst_dir = tempfile::tempdir().unwrap();
        let dst = test_db(dst_dir.path());
        let mut dst_conn = dst.connection().unwrap();
        dst_conn
            .execute(
                "INSERT INTO deleted_messages VALUES ('locally-deleted')",
                [],
            )
            .unwrap();
        import_frames(&mut dst_conn, &unfiltered).unwrap();
        for (table, column) in related_tables {
            let ids = dst_conn
                .prepare(&format!("SELECT {column} FROM {table}"))
                .unwrap()
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            let expected = if table == "attachments" {
                AttachmentId::from_cdn_key("live").to_string()
            } else {
                "live".into()
            };
            assert_eq!(ids, [expected], "{table}");
        }
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
            channel_msg.sidebar_preview(None, Some(&label), None),
            "\u{2068}\u{1F3AC} GIF\u{2069}",
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
