//! Immutable original batch membership, independent of live message rows.
//! Minimal routing/proof/control data lives only in the encrypted database.
use crate::{
    domain::{
        UserId,
        ids::{ChannelId, MessageId},
        message::ConversationType,
    },
    error::StorageError,
    storage::Database,
};
use rusqlite::{OptionalExtension, params};
use std::sync::Arc;

pub(crate) static DELETE_BATCH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct PreparedDelete {
    pub author: UserId,
    pub timestamp: u64,
    pub message_id: MessageId,
    pub conversation_id: ChannelId,
    pub conversation_type: ConversationType,
    pub call_id: Option<String>,
    pub control: Vec<u8>,
    pub control_sent: bool,
    pub local_completed: bool,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct PreparedBatch {
    pub id: String,
    pub targets: Vec<PreparedDelete>,
    pub server_committed: bool,
}
pub(crate) struct DeleteBatchRetryRepository {
    db: Arc<Database>,
}
impl DeleteBatchRetryRepository {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }
    pub fn selection_key(targets: &[(UserId, u64)]) -> Result<String, StorageError> {
        let mut keys: Vec<_> = targets
            .iter()
            .map(|(author, time)| (author.to_string(), *time))
            .collect();
        keys.sort();
        keys.dedup();
        serde_json::to_string(&keys).map_err(|e| StorageError::InvalidData(e.to_string()))
    }
    pub fn get(
        &self,
        account: &UserId,
        scope: &str,
        selection: &str,
    ) -> Result<Option<PreparedBatch>, StorageError> {
        let conn = self.db.connection()?;
        let json: Option<String> = conn.query_row("SELECT prepared_json FROM delete_batch_retry WHERE account_id=?1 AND scope=?2 AND selection_key=?3", params![account.to_string(),scope,selection], |r| r.get(0)).optional()?;
        json.map(|json| {
            serde_json::from_str(&json).map_err(|e| StorageError::InvalidData(e.to_string()))
        })
        .transpose()
    }
    // One row contains the complete immutable original selected set. No FK to
    // message rows and no scope regrouping/chunking on recovery.
    pub fn insert(
        &self,
        account: &UserId,
        scope: &str,
        selection: &str,
        batch: &PreparedBatch,
    ) -> Result<(), StorageError> {
        let json =
            serde_json::to_string(batch).map_err(|e| StorageError::InvalidData(e.to_string()))?;
        let conn = self.db.connection()?;
        conn.execute("INSERT INTO delete_batch_retry(batch_id,account_id,scope,selection_key,prepared_json) VALUES (?1,?2,?3,?4,?5)",params![batch.id,account.to_string(),scope,selection,json])?;
        Ok(())
    }
    pub fn progress(
        &self,
        account: &UserId,
        scope: &str,
        batch: &PreparedBatch,
    ) -> Result<(), StorageError> {
        let json =
            serde_json::to_string(batch).map_err(|e| StorageError::InvalidData(e.to_string()))?;
        let conn = self.db.connection()?;
        let updated = conn.execute("UPDATE delete_batch_retry SET prepared_json=?4 WHERE batch_id=?1 AND account_id=?2 AND scope=?3",params![batch.id,account.to_string(),scope,json])?;
        if updated != 1 {
            return Err(StorageError::NotFound(
                "Deletion batch retry missing".into(),
            ));
        }
        Ok(())
    }
    pub fn finish(
        &self,
        account: &UserId,
        scope: &str,
        batch: &PreparedBatch,
    ) -> Result<(), StorageError> {
        if !batch.targets.iter().all(|t| t.local_completed) {
            return Err(StorageError::InvalidData(
                "Deletion batch incomplete".into(),
            ));
        }
        let conn = self.db.connection()?;
        conn.execute(
            "DELETE FROM delete_batch_retry WHERE batch_id=?1 AND account_id=?2 AND scope=?3",
            params![batch.id, account.to_string(), scope],
        )?;
        Ok(())
    }
    pub fn defer(
        &self,
        account: &UserId,
        scope: &str,
        id: &str,
        permanent: bool,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute("UPDATE delete_batch_retry SET attempts=attempts+1, next_attempt=unixepoch()+MIN(3600,30*(1 << MIN(attempts,7))), paused=CASE WHEN ?4 OR attempts>=11 THEN 1 ELSE 0 END WHERE batch_id=?1 AND account_id=?2 AND scope=?3",params![id,account.to_string(),scope,permanent])?;
        Ok(())
    }
    // Bound each recovery tick to four ORIGINAL batches; exponential backoff
    // caps automatic attempts at twelve, and denied authorization pauses at once.
    pub fn due(&self, account: &UserId) -> Result<Vec<(String, PreparedBatch)>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare("SELECT batch_id,scope,prepared_json FROM delete_batch_retry WHERE account_id=?1 AND paused=0 AND next_attempt<=unixepoch() ORDER BY next_attempt,batch_id LIMIT 4")?;
        let mut rows = stmt.query([account.to_string()])?;
        super::repo_utils::collect_decoded_rows(&mut rows, "delete_batch_retry", |row| {
            let scope: String = row.get(1)?;
            let json: String = row.get(2)?;
            let batch = serde_json::from_str(&json)
                .map_err(|_| StorageError::InvalidData("Invalid deletion batch JSON".into()))?;
            Ok((scope, batch))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn corrupt_retry_batch_does_not_block_valid_batch() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open(&dir.path().join("retry.db"), "fixture").unwrap());
        db.run_migrations().unwrap();
        let repo = DeleteBatchRetryRepository::new(db.clone());
        let account = UserId::new();
        let batch = PreparedBatch {
            id: "good".into(),
            targets: vec![],
            server_committed: false,
        };
        repo.insert(&account, "scope", "good", &batch).unwrap();
        let bad = PreparedBatch {
            id: "bad".into(),
            ..batch.clone()
        };
        repo.insert(&account, "scope", "bad", &bad).unwrap();
        db.connection()
            .unwrap()
            .execute(
                "UPDATE delete_batch_retry SET prepared_json='broken' WHERE batch_id='bad'",
                [],
            )
            .unwrap();
        let due = repo.due(&account).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].1.id, "good");
        assert!(repo.get(&account, "scope", "bad").is_err());
    }
}
