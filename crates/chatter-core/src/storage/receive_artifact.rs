//! Account/device-bound authenticated secret receive journal. Not history backup.
use crate::error::StorageError;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

pub const MAX_RECEIVE_CONTENT: usize = 64 * 1024;
pub const MAX_RECOVERY_BATCH: usize = 100;
/// Fail closed at capacity. Never prune unresolved receive evidence implicitly.
pub const MAX_PENDING_ARTIFACTS: i64 = 1024;
pub const MAX_RETAINED_ARTIFACTS: i64 = 4096;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiveKind {
    Reaction,
    Content,
}
impl ReceiveKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reaction => "reaction",
            Self::Content => "content",
        }
    }
}
pub fn check_capacity(conn: &Connection) -> Result<(), StorageError> {
    let (pending, unacknowledged): (i64, i64) = conn.query_row(
        "SELECT COALESCE(sum(state='pending'),0),COALESCE(sum(state!='acknowledged'),0) FROM reaction_receive_artifacts",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if pending >= MAX_PENDING_ARTIFACTS || unacknowledged >= MAX_RETAINED_ARTIFACTS {
        tracing::warn!(
            pending,
            unacknowledged,
            "Secret receive artifact capacity reached"
        );
        return Err(StorageError::InvalidData(
            "secret receive journal capacity reached; evidence retained".into(),
        ));
    }
    Ok(())
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReceiveKey {
    pub local_account: String,
    pub local_device: u32,
    pub transport: String,
    pub scope: String,
    pub envelope_id: String,
    pub server_timestamp_ns: u64,
}
impl ReceiveKey {
    pub fn validate(&self) -> Result<(), StorageError> {
        if uuid::Uuid::parse_str(&self.local_account).is_err()
            || !(1..=127).contains(&self.local_device)
            || !matches!(self.transport.as_str(), "anon_dm" | "anon_group")
            || self.scope.is_empty()
            || self.scope.len() > 256
            || self.envelope_id.is_empty()
            || self.envelope_id.len() > 256
            || self.server_timestamp_ns == 0
            || self.server_timestamp_ns > i64::MAX as u64
        {
            return Err(StorageError::InvalidData(
                "invalid receive journal binding".into(),
            ));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceiveArtifact {
    pub id: String,
    pub key: ReceiveKey,
    pub digest: Vec<u8>,
    pub sender: String,
    pub sender_device: u32,
    pub content: Vec<u8>,
    pub message_type: u8,
    pub kind: ReceiveKind,
    pub applied: bool,
    pub acknowledged: bool,
}
pub fn envelope_digest(ciphertext: &[u8]) -> Vec<u8> {
    Sha256::digest(ciphertext).to_vec()
}
impl ReceiveArtifact {
    pub(crate) fn new(
        key: ReceiveKey,
        digest: Vec<u8>,
        sender: String,
        sender_device: u32,
        content: Vec<u8>,
        message_type: u8,
    ) -> Result<Self, StorageError> {
        Self::new_kind(
            key,
            digest,
            sender,
            sender_device,
            content,
            message_type,
            ReceiveKind::Reaction,
        )
    }
    pub(crate) fn new_content(
        key: ReceiveKey,
        digest: Vec<u8>,
        sender: String,
        sender_device: u32,
        content: Vec<u8>,
        message_type: u8,
    ) -> Result<Self, StorageError> {
        Self::new_kind(
            key,
            digest,
            sender,
            sender_device,
            content,
            message_type,
            ReceiveKind::Content,
        )
    }
    fn new_kind(
        key: ReceiveKey,
        digest: Vec<u8>,
        sender: String,
        sender_device: u32,
        content: Vec<u8>,
        message_type: u8,
        kind: ReceiveKind,
    ) -> Result<Self, StorageError> {
        key.validate()?;
        if digest.len() != 32
            || uuid::Uuid::parse_str(&sender).is_err()
            || !(1..=127).contains(&sender_device)
            || !matches!(message_type, 2 | 3)
            || content.is_empty()
            || content.len() > MAX_RECEIVE_CONTENT
        {
            return Err(StorageError::InvalidData(
                "invalid authenticated receive artifact".into(),
            ));
        }
        let encoded = serde_json::to_vec(&key).map_err(|_| {
            StorageError::InvalidData("receive binding serialization failed".into())
        })?;
        Ok(Self {
            id: uuid::Uuid::new_v5(&uuid::Uuid::nil(), &encoded).to_string(),
            key,
            digest,
            sender,
            sender_device,
            content,
            message_type,
            kind,
            applied: false,
            acknowledged: false,
        })
    }
}
const SELECT: &str = "SELECT id,local_account,local_device,transport,scope,envelope_id,server_timestamp_ns,digest,sender,sender_device,content,state,message_type,kind FROM reaction_receive_artifacts";
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ReceiveArtifact> {
    Ok(ReceiveArtifact {
        id: r.get(0)?,
        key: ReceiveKey {
            local_account: r.get(1)?,
            local_device: r.get(2)?,
            transport: r.get(3)?,
            scope: r.get(4)?,
            envelope_id: r.get(5)?,
            server_timestamp_ns: r.get::<_, i64>(6)? as u64,
        },
        digest: r.get(7)?,
        sender: r.get(8)?,
        sender_device: r.get(9)?,
        content: r.get(10)?,
        applied: r.get::<_, String>(11)? != "pending",
        acknowledged: r.get::<_, String>(11)? == "acknowledged",
        message_type: r.get(12)?,
        kind: match r.get::<_, String>(13)?.as_str() {
            "reaction" => ReceiveKind::Reaction,
            "content" => ReceiveKind::Content,
            _ => {
                return Err(rusqlite::Error::InvalidColumnType(
                    13,
                    "kind".into(),
                    rusqlite::types::Type::Text,
                ));
            }
        },
    })
}
/// Connection-aware: safe to call within the application's Transaction B.
pub fn load(
    conn: &Connection,
    key: &ReceiveKey,
    digest: &[u8],
) -> Result<Option<ReceiveArtifact>, StorageError> {
    key.validate()?;
    let result=conn.query_row(&format!("{SELECT} WHERE local_account=?1 AND local_device=?2 AND transport=?3 AND scope=?4 AND envelope_id=?5"),params![key.local_account,key.local_device,key.transport,key.scope,key.envelope_id],row).optional()?;
    if let Some(a) = &result {
        if a.key != *key || a.digest != digest {
            return Err(StorageError::InvalidData(
                "receive envelope binding conflict".into(),
            ));
        }
    }
    Ok(result)
}
pub fn load_by_id(conn: &Connection, id: &str) -> Result<Option<ReceiveArtifact>, StorageError> {
    Ok(conn
        .query_row(&format!("{SELECT} WHERE id=?1"), [id], row)
        .optional()?)
}
/// Only the application's atomic reaction/pending transaction may call this.
pub fn mark_applied(conn: &Connection, artifact: &ReceiveArtifact) -> Result<(), StorageError> {
    if conn.is_autocommit() {
        return Err(StorageError::InvalidData(
            "receive applied marker requires an application transaction".into(),
        ));
    }
    require_current_binding(conn, &artifact.key)?;
    let stored = load(conn, &artifact.key, &artifact.digest)?
        .ok_or_else(|| StorageError::InvalidData("missing receive artifact".into()))?;
    if stored.id != artifact.id
        || stored.sender != artifact.sender
        || stored.sender_device != artifact.sender_device
        || stored.content != artifact.content
        || stored.message_type != artifact.message_type
    {
        return Err(StorageError::InvalidData("receive artifact changed".into()));
    }
    conn.execute(
        "UPDATE reaction_receive_artifacts SET state='applied' WHERE id=?1 AND state='pending'",
        [&artifact.id],
    )?;
    Ok(())
}
pub fn list_pending(
    conn: &Connection,
    local_account: &str,
    local_device: u32,
    limit: usize,
) -> Result<Vec<ReceiveArtifact>, StorageError> {
    require_current_binding(
        conn,
        &ReceiveKey {
            local_account: local_account.into(),
            local_device,
            transport: "anon_dm".into(),
            scope: "recovery".into(),
            envelope_id: "recovery".into(),
            server_timestamp_ns: 1,
        },
    )?;
    let mut stmt=conn.prepare(&format!("{SELECT} WHERE local_account=?1 AND local_device=?2 AND state='pending' ORDER BY server_timestamp_ns,id LIMIT ?3"))?;
    let result = stmt
        .query_map(
            params![
                local_account,
                local_device,
                limit.min(MAX_RECOVERY_BATCH) as i64
            ],
            row,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(result)
}
/// Called only after upstream ACK success for this exact original binding.
/// Must share a writer transaction with current account/device validation.
pub fn mark_acknowledged(
    conn: &Connection,
    key: &ReceiveKey,
    digest: &[u8],
) -> Result<(), StorageError> {
    if conn.is_autocommit() {
        return Err(StorageError::InvalidData(
            "receive ACK receipt requires a transaction".into(),
        ));
    }
    require_current_binding(conn, key)?;
    let Some(a) = load(conn, key, digest)? else {
        return Ok(());
    };
    if !a.applied {
        return Err(StorageError::InvalidData(
            "cannot acknowledge unapplied receive artifact".into(),
        ));
    }
    conn.execute("UPDATE reaction_receive_artifacts SET state='acknowledged',content=X'',acknowledged_at=COALESCE(acknowledged_at,?2) WHERE id=?1 AND state IN ('applied','acknowledged')",params![a.id,chrono::Utc::now().timestamp_millis()])?;
    Ok(())
}
/// A mailbox ACK may precede content application. Retain the plaintext until
/// application succeeds; a later small transaction can then clear it.
pub fn mark_content_upstream_acked(
    conn: &Connection,
    key: &ReceiveKey,
    digest: &[u8],
) -> Result<(), StorageError> {
    if conn.is_autocommit() {
        return Err(StorageError::InvalidData(
            "receive ACK receipt requires a transaction".into(),
        ));
    }
    require_current_binding(conn, key)?;
    let Some(artifact) = load(conn, key, digest)? else {
        return Ok(());
    };
    if artifact.kind != ReceiveKind::Content {
        return Err(StorageError::InvalidData("receive kind mismatch".into()));
    }
    conn.execute(
        "UPDATE reaction_receive_artifacts SET upstream_acked_at=COALESCE(upstream_acked_at,?2) WHERE id=?1",
        params![artifact.id, chrono::Utc::now().timestamp_millis()],
    )?;
    conn.execute(
        "DELETE FROM reaction_receive_artifacts WHERE id=?1 AND state='applied'",
        [&artifact.id],
    )?;
    Ok(())
}
/// Called after awaited, idempotent content application, in its own small
/// writer transaction. The application may have committed before this call.
pub fn mark_content_applied(
    conn: &Connection,
    artifact: &ReceiveArtifact,
) -> Result<(), StorageError> {
    if conn.is_autocommit() || artifact.kind != ReceiveKind::Content {
        return Err(StorageError::InvalidData(
            "content applied marker requires a transaction".into(),
        ));
    }
    require_current_binding(conn, &artifact.key)?;
    let stored = load(conn, &artifact.key, &artifact.digest)?
        .ok_or_else(|| StorageError::InvalidData("missing receive artifact".into()))?;
    if stored.id != artifact.id
        || stored.sender != artifact.sender
        || stored.sender_device != artifact.sender_device
        || stored.content != artifact.content
        || stored.message_type != artifact.message_type
        || stored.kind != artifact.kind
    {
        return Err(StorageError::InvalidData("receive artifact changed".into()));
    }
    if stored.acknowledged {
        return Err(StorageError::InvalidData(
            "content artifact already retired".into(),
        ));
    }
    let upstream_acked: bool = conn.query_row(
        "SELECT upstream_acked_at IS NOT NULL FROM reaction_receive_artifacts WHERE id=?1",
        [&artifact.id],
        |row| row.get(0),
    )?;
    if upstream_acked {
        conn.execute(
            "DELETE FROM reaction_receive_artifacts WHERE id=?1",
            [&artifact.id],
        )?;
    } else {
        conn.execute(
            "UPDATE reaction_receive_artifacts SET state='applied' WHERE id=?1 AND state='pending'",
            [&artifact.id],
        )?;
    }
    Ok(())
}
pub(crate) fn require_current_binding(
    conn: &Connection,
    key: &ReceiveKey,
) -> Result<(), StorageError> {
    key.validate()?;
    let account: String = conn.query_row(
        "SELECT value FROM settings WHERE key=?1",
        [crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID],
        |r| r.get(0),
    )?;
    let device: String = conn.query_row(
        "SELECT value FROM settings WHERE key=?1",
        [crate::storage::repositories::settings_repo::keys::CURRENT_DEVICE_ID],
        |r| r.get(0),
    )?;
    if serde_json::from_str::<String>(&account).ok().as_deref() != Some(key.local_account.as_str())
        || serde_json::from_str::<u32>(&device).ok() != Some(key.local_device)
    {
        return Err(StorageError::InvalidData(
            "receive account/device changed".into(),
        ));
    }
    Ok(())
}
/// Only confirmed ACK receipts can be reclaimed. Pending or applied-unacked
/// artifacts never age out, and their content remains available for replay.
pub(crate) fn reclaim_acknowledged(conn: &Connection) -> Result<(), StorageError> {
    let total: i64 =
        conn.query_row("SELECT count(*) FROM reaction_receive_artifacts", [], |r| {
            r.get(0)
        })?;
    if total >= MAX_RETAINED_ARTIFACTS {
        conn.execute("DELETE FROM reaction_receive_artifacts WHERE id IN (SELECT id FROM reaction_receive_artifacts WHERE state='acknowledged' ORDER BY acknowledged_at,id LIMIT 100)",[])?;
    }
    let total: i64 =
        conn.query_row("SELECT count(*) FROM reaction_receive_artifacts", [], |r| {
            r.get(0)
        })?;
    if total >= MAX_RETAINED_ARTIFACTS {
        return Err(StorageError::InvalidData(
            "receive journal has no reclaimable ACK receipts".into(),
        ));
    }
    Ok(())
}
pub(crate) fn insert(conn: &Connection, a: &ReceiveArtifact) -> Result<(), StorageError> {
    conn.execute("INSERT INTO reaction_receive_artifacts(id,local_account,local_device,transport,scope,envelope_id,server_timestamp_ns,digest,sender,sender_device,content,state,message_type,kind) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,'pending',?12,?13)",params![a.id,a.key.local_account,a.key.local_device,a.key.transport,a.key.scope,a.key.envelope_id,a.key.server_timestamp_ns as i64,a.digest,a.sender,a.sender_device,a.content,a.message_type,a.kind.as_str()])?;
    Ok(())
}
#[derive(Clone, Debug)]
pub(crate) struct Observed {
    pub table: &'static str,
    pub key: String,
    pub column: &'static str,
    pub value: Option<Vec<u8>>,
}
#[derive(Clone, Debug, Default)]
pub(crate) struct ReceiveChanges {
    pub observed: Vec<Observed>,
    pub sessions: Vec<(String, Vec<u8>)>,
    pub identities: Vec<(String, Vec<u8>)>,
    pub prekeys: Vec<(u32, Option<Vec<u8>>)>,
    pub signed: Vec<(u32, Vec<u8>)>,
    pub kyber: Vec<(u32, Vec<u8>)>,
    pub usage: Vec<(u32, u32, Vec<u8>)>,
}
