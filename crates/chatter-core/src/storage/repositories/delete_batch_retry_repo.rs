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
        let mut stmt = conn.prepare("SELECT scope,prepared_json FROM delete_batch_retry WHERE account_id=?1 AND paused=0 AND next_attempt<=unixepoch() ORDER BY next_attempt,batch_id LIMIT 4")?;
        let rows = stmt.query_map([account.to_string()], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (scope, json) = row?;
            let batch = serde_json::from_str(&json)
                .map_err(|e| StorageError::InvalidData(e.to_string()))?;
            Ok((scope, batch))
        })
        .collect()
    }
}
