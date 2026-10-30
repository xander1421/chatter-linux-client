use chrono::{Duration, Utc};

use crate::domain::UserId;
use crate::error::MailboxError;

impl super::DmConversationManager {
    /// Owe `content` to `peer_device_id`: a message some other device of the
    /// peer already got, held until this one answers the bootstrap.
    pub fn queue_for_device(
        &self,
        peer_id: &UserId,
        peer_device_id: u32,
        content: &[u8],
    ) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        // owed copies older than a mailbox lifetime are dropped, so a
        // device that never answers (logged out, keys still published) costs
        // bounded storage; per-device delivery receipts if that matters.
        let cutoff = (Utc::now()
            - Duration::seconds(crate::mailbox::dm::bootstrap::DEFAULT_MAILBOX_TTL_SECONDS as i64))
        .to_rfc3339();
        conn.execute(
            "DELETE FROM dm_device_pending WHERE queued_at < ?1",
            rusqlite::params![cutoff],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;
        conn.execute(
            "INSERT INTO dm_device_pending (conversation_id, peer_device_id, content, queued_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                peer_id.to_string(),
                peer_device_id,
                content,
                Utc::now().to_rfc3339()
            ],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Record that a bootstrap MESSAGE or REQUEST reached `peer_device_id`.
    pub fn mark_device_asked(
        &self,
        peer_id: &UserId,
        peer_device_id: u32,
    ) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        conn.execute(
            "INSERT OR REPLACE INTO dm_device_asked (conversation_id, peer_device_id, asked_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![peer_id.to_string(), peer_device_id, Utc::now().to_rfc3339()],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Whether `peer_device_id` was asked to answer the bootstrap within the
    /// last mailbox lifetime and has not answered since. An ask older than
    /// that is repeated: the mailbox it announced may be gone.
    pub fn was_device_asked(
        &self,
        peer_id: &UserId,
        peer_device_id: u32,
    ) -> Result<bool, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        let cutoff = (Utc::now()
            - Duration::seconds(crate::mailbox::dm::bootstrap::DEFAULT_MAILBOX_TTL_SECONDS as i64))
        .to_rfc3339();
        conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM dm_device_asked
                            WHERE conversation_id = ?1 AND peer_device_id = ?2 AND asked_at >= ?3)",
            rusqlite::params![peer_id.to_string(), peer_device_id, cutoff],
            |row| row.get(0),
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))
    }

    /// Take everything owed to `peer_device_id`, oldest first.
    pub fn drain_device_queue(
        &self,
        peer_id: &UserId,
        peer_device_id: u32,
    ) -> Result<Vec<Vec<u8>>, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        let storage = |e: rusqlite::Error| MailboxError::Storage(e.to_string());
        let params = rusqlite::params![peer_id.to_string(), peer_device_id];
        let owed = conn
            .prepare(
                "SELECT id, content FROM dm_device_pending
                 WHERE conversation_id = ?1 AND peer_device_id = ?2 ORDER BY id",
            )
            .map_err(storage)?
            .query_map(params, |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .map_err(storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage)?;
        // Only what was read: a copy queued meanwhile stays for the next drain.
        if let Some((last, _)) = owed.last() {
            conn.execute(
                "DELETE FROM dm_device_pending WHERE conversation_id = ?1 AND peer_device_id = ?2 AND id <= ?3",
                rusqlite::params![peer_id.to_string(), peer_device_id, last],
            )
            .map_err(storage)?;
        }
        Ok(owed.into_iter().map(|(_, content)| content).collect())
    }
}
