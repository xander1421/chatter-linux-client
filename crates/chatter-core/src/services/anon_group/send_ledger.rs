//! Exact outgoing row and original recipient completion, retained across restart.
use crate::{domain::ids::MessageId, error::StorageError, storage::database::Database};
use prost::Message as _;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
struct Ledger {
    message_id: String,
    initialized: bool,
    recipients: BTreeMap<String, String>,
    #[serde(default)]
    held_group: Option<String>,
    #[serde(default)]
    held_content: Option<Vec<u8>>,
}

pub(super) fn key(owner: &str, group: &str, inner: &crate::proto::content_v1::Content) -> String {
    let mut hash = Sha256::new();
    hash.update(owner.as_bytes());
    hash.update([0]);
    hash.update(group.as_bytes());
    hash.update([0]);
    hash.update(inner.encode_to_vec());
    format!("send.group.completion.v1:{}", hex::encode(hash.finalize()))
}
fn encode(ledger: &Ledger) -> Result<String, StorageError> {
    serde_json::to_string(ledger)
        .map_err(|_| StorageError::InvalidData("Invalid group send ledger".into()))
}
fn decode(raw: &str) -> Result<Ledger, StorageError> {
    serde_json::from_str(raw)
        .map_err(|_| StorageError::InvalidData("Invalid group send ledger".into()))
}
pub(super) fn register(db: &Database, key: &str, message_id: &str) -> Result<(), StorageError> {
    let conn = db.connection()?;
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO settings(key,value,updated_at) VALUES (?1,?2,?3)",
        params![
            key,
            encode(&Ledger {
                message_id: message_id.into(),
                initialized: false,
                recipients: BTreeMap::new(),
                held_group: None,
                held_content: None,
            })?,
            chrono::Utc::now().to_rfc3339()
        ],
    )?;
    if inserted != 1 {
        return Err(StorageError::InvalidData(
            "Group send identity already retained".into(),
        ));
    }
    Ok(())
}

/// Preserve the exact post while the joining member awaits its first roster.
pub(super) fn hold(
    db: &Database,
    key: &str,
    group: &str,
    content: &crate::proto::content_v1::Content,
) -> Result<Option<String>, StorageError> {
    let first_hold = mutate(db, key, |ledger| {
        if !ledger.initialized {
            let first_hold = ledger.held_group.is_none();
            ledger.held_group = Some(group.to_owned());
            ledger.held_content = Some(content.encode_to_vec());
            if first_hold {
                return Some(ledger.message_id.clone());
            }
        }
        None
    })?
    .ok_or_else(|| StorageError::InvalidData("Group post has no retained row".into()))?;
    Ok(first_hold)
}

pub(super) fn held_posts(
    db: &Database,
    group: &str,
) -> Result<Vec<(String, crate::proto::content_v1::Content)>, StorageError> {
    let conn = db.read_connection()?;
    let mut stmt = conn.prepare(
        "SELECT value FROM settings WHERE key LIKE 'send.group.completion.v1:%' \
         AND json_extract(value,'$.held_group')=?1 \
         AND json_extract(value,'$.initialized')=0 ORDER BY updated_at",
    )?;
    let rows = stmt.query_map([group], |row| row.get::<_, String>(0))?;
    let mut posts = Vec::new();
    for row in rows {
        let ledger = decode(&row?)?;
        if let Some(bytes) = ledger.held_content {
            posts.push((
                ledger.message_id,
                crate::proto::content_v1::Content::decode(bytes.as_slice()).map_err(|_| {
                    StorageError::InvalidData("Invalid held anonymous group post".into())
                })?,
            ));
        }
    }
    Ok(posts)
}

/// The exact retained post still carries its original wire timestamp after
/// release; sender-side recipient revalidation uses the owner admission clock.
pub(super) fn was_held(db: &Database, key: &str, group: &str) -> Result<bool, StorageError> {
    let conn = db.read_connection()?;
    let found: i64 = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM settings WHERE key=?1 AND json_extract(value,'$.held_group')=?2)",
        params![key, group],
        |row| row.get(0),
    )?;
    Ok(found != 0)
}

// None means an unrelated control/legacy send without a registered outgoing row.
pub(super) fn initialize(
    db: &Database,
    key: &str,
    recipients: &[String],
) -> Result<Option<bool>, StorageError> {
    mutate(db, key, |ledger| {
        if ledger.initialized {
            return false;
        }
        ledger.initialized = true;
        ledger.recipients = recipients
            .iter()
            .map(|id| (id.clone(), "pending".into()))
            .collect();
        true
    })
}

pub(super) fn record(
    db: &Database,
    key: &str,
    recipient: &str,
    status: &str,
) -> Result<Option<MessageId>, StorageError> {
    mutate(db, key, |ledger| {
        // A later failure cannot undo a definitive original receipt.
        if let Some(previous) = ledger.recipients.get_mut(recipient) {
            if previous != "confirmed" {
                *previous = status.into();
            }
        }
        (ledger.initialized && ledger.recipients.values().all(|s| s == "confirmed"))
            .then(|| ledger.message_id.parse::<MessageId>().ok())
            .flatten()
    })
    .map(Option::flatten)
}

pub(super) fn enqueue(
    db: &Database,
    repo: &crate::storage::repositories::AnonGroupRepository,
    key: &str,
    group: &crate::domain::anon_group::AnonGroupId,
    recipient: &crate::domain::ids::UserId,
    content: &[u8],
) -> Result<(), StorageError> {
    if let Err(error) = repo.enqueue_pending_message(group, recipient, content) {
        record(db, key, &recipient.to_string(), "failed")?;
        return Err(error);
    }
    record(db, key, &recipient.to_string(), "queued")?;
    Ok(())
}

/// A per-peer read/delivery receipt is not proof that every queued original
/// recipient has received the group post. Keep admission until aggregate proof.
pub(crate) fn row_complete(db: &Database, id: &str) -> Result<Option<bool>, StorageError> {
    let conn = db.read_connection()?;
    let mut statement = conn.prepare("SELECT value FROM settings WHERE key LIKE 'send.group.completion.v1:%' AND json_extract(value,'$.message_id')=?1")?;
    let mut rows = statement.query(params![id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let ledger = decode(&row.get::<_, String>(0)?)?;
    Ok(Some(
        ledger.initialized && ledger.recipients.values().all(|s| s == "confirmed"),
    ))
}

fn mutate<T>(
    db: &Database,
    key: &str,
    change: impl FnOnce(&mut Ledger) -> T,
) -> Result<Option<T>, StorageError> {
    let mut conn = db.connection()?;
    let tx = conn.transaction()?;
    let raw = match tx.query_row(
        "SELECT value FROM settings WHERE key=?1",
        params![key],
        |row| row.get::<_, String>(0),
    ) {
        Ok(raw) => raw,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut ledger = decode(&raw)?;
    let result = change(&mut ledger);
    tx.execute(
        "UPDATE settings SET value=?2 WHERE key=?1",
        params![key, encode(&ledger)?],
    )?;
    tx.commit()?;
    Ok(Some(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn queue_write_failure_propagates_and_does_not_confirm_partial_fanout() {
        use crate::{
            domain::{anon_group::AnonGroupId, ids::UserId},
            storage::repositories::AnonGroupRepository,
        };
        let dir = tempfile::tempdir().unwrap();
        let db = std::sync::Arc::new(
            Database::open(&dir.path().join("queue-failure.db"), "test-only").unwrap(),
        );
        db.run_migrations()
            .expect("migrate throwaway test database");
        let repo = AnonGroupRepository::new(db.clone());
        let group = AnonGroupId::new();
        let first = UserId::new();
        let queued = UserId::new();
        repo.insert_group(&crate::domain::anon_group::AnonGroup::from_invite(
            group,
            "fixture".into(),
            first,
        ))
        .unwrap();
        repo.add_member(&crate::domain::anon_group::AnonGroupMember::new(
            group,
            queued,
            crate::domain::anon_group::AnonGroupRole::Member,
        ))
        .unwrap();
        let original_content = crate::proto::content_v1::Content {
            timestamp: 7,
            ..Default::default()
        }
        .encode_to_vec();
        let id = MessageId::new();
        let key = "send.group.completion.v1:queue-failure";
        register(&db, key, &id.to_string()).unwrap();
        initialize(&db, key, &[first.to_string(), queued.to_string()]).unwrap();
        record(&db, key, &first.to_string(), "confirmed").unwrap();
        db.connection().unwrap().execute_batch("CREATE TRIGGER reject_pending BEFORE INSERT ON pending_anon_group_messages BEGIN SELECT RAISE(FAIL,'injected queue write failure'); END;").unwrap();
        assert!(enqueue(&db, &repo, key, &group, &queued, &original_content).is_err());
        assert_eq!(row_complete(&db, &id.to_string()).unwrap(), Some(false));
        assert!(
            repo.get_pending_messages_for_member(&group, &queued)
                .unwrap()
                .is_empty()
        );
        db.connection()
            .unwrap()
            .execute_batch("DROP TRIGGER reject_pending;")
            .unwrap();
        enqueue(&db, &repo, key, &group, &queued, &original_content).unwrap();
        assert_eq!(row_complete(&db, &id.to_string()).unwrap(), Some(false));
        let rows = repo
            .get_pending_messages_for_member(&group, &queued)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1.as_slice(), original_content.as_slice());
        assert_eq!(
            record(&db, key, &queued.to_string(), "confirmed").unwrap(),
            Some(id)
        );
    }

    #[test]
    fn all_joinack_waiters_and_missing_tokens_stay_queued_until_each_original_deposit() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("queue.db"), "test-only").unwrap();
        db.run_migrations()
            .expect("migrate throwaway test database");
        let id = MessageId::new();
        register(&db, "queued-original", &id.to_string()).unwrap();
        initialize(
            &db,
            "queued-original",
            &["awaiting-joinack".into(), "awaiting-token".into()],
        )
        .unwrap();
        assert_eq!(
            record(&db, "queued-original", "awaiting-joinack", "queued").unwrap(),
            None
        );
        assert_eq!(
            record(&db, "queued-original", "awaiting-token", "queued").unwrap(),
            None
        );
        assert_eq!(
            record(&db, "queued-original", "awaiting-token", "unknown").unwrap(),
            None
        );
        assert_eq!(
            record(&db, "queued-original", "awaiting-joinack", "confirmed").unwrap(),
            None
        );
        assert_eq!(
            record(&db, "queued-original", "awaiting-token", "confirmed").unwrap(),
            Some(id)
        );
    }

    #[test]
    fn queued_partial_and_permanent_failure_never_confirm_before_all_original_receipts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("group.db");
        let id = MessageId::new();
        let db = Database::open(&path, "test-only").unwrap();
        db.run_migrations()
            .expect("migrate throwaway test database");
        register(&db, "send.group.completion.v1:original", &id.to_string()).unwrap();
        assert_eq!(
            initialize(
                &db,
                "send.group.completion.v1:original",
                &["A".into(), "B".into()]
            )
            .unwrap(),
            Some(true)
        );
        assert_eq!(
            record(&db, "send.group.completion.v1:original", "A", "queued").unwrap(),
            None
        );
        assert_eq!(
            record(&db, "send.group.completion.v1:original", "B", "confirmed").unwrap(),
            None
        );
        assert_eq!(row_complete(&db, &id.to_string()).unwrap(), Some(false));
        // Re-entry must not snapshot a new recipient or re-deposit to B.
        assert_eq!(
            initialize(
                &db,
                "send.group.completion.v1:original",
                &["new-member".into()]
            )
            .unwrap(),
            Some(false)
        );
        drop(db);
        let db = Database::open(&path, "test-only").unwrap();
        db.run_migrations()
            .expect("migrate throwaway test database");
        assert_eq!(
            record(&db, "send.group.completion.v1:original", "A", "failed").unwrap(),
            None
        );
        assert_eq!(
            record(
                &db,
                "send.group.completion.v1:original",
                "new-member",
                "confirmed"
            )
            .unwrap(),
            None
        );
        assert_eq!(
            record(&db, "send.group.completion.v1:original", "A", "confirmed").unwrap(),
            Some(id)
        );
        assert_eq!(row_complete(&db, &id.to_string()).unwrap(), Some(true));
        assert_eq!(
            record(&db, "send.group.completion.v1:original", "B", "unknown").unwrap(),
            Some(id)
        );
    }
}
