//! DMs held back while a first-contact bootstrap with the peer is in flight.
//!
//! Queued by `services/message/dm_bootstrap.rs` when a send would have to wait
//! for the peer's bootstrap, drained once it completes — by the initiator
//! (`MessageService::send_pending_messages`) or the responder
//! (`inbox/streaming/dm_bootstrap_handler.rs`).

use std::sync::Arc;

use rusqlite::params;

use crate::domain::UserId;
use crate::domain::ids::{AttachmentId, ChannelId};
use crate::error::StorageError;
use crate::storage::Database;

/// A DM waiting for its peer's session.
#[derive(Debug, Clone)]
pub struct PendingDm {
    /// Serialized `content_v1::Content`.
    pub content: Vec<u8>,
    /// Local attachment IDs to record once sent.
    pub attachment_ids: Vec<AttachmentId>,
    /// DM channel the message is stored under.
    pub channel_id: Option<ChannelId>,
}

/// SQLite-backed pending-DM queue (`pending_dm_messages`, migration v077).
pub struct PendingDmRepository {
    db: Arc<Database>,
}

impl PendingDmRepository {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Queue a message for `peer_id` until a session exists.
    pub fn queue(
        &self,
        peer_id: &UserId,
        content: Vec<u8>,
        attachment_ids: Vec<AttachmentId>,
        channel_id: Option<ChannelId>,
    ) -> Result<(), StorageError> {
        let attachment_ids_blob = crate::storage::serialization::db_serialize(&attachment_ids)?;
        self.db.connection()?.execute(
            "INSERT INTO pending_dm_messages (peer_id, content, attachment_ids_blob, channel_id)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                peer_id.to_string(),
                content,
                attachment_ids_blob,
                channel_id.map(|c| c.to_string())
            ],
        )?;
        Ok(())
    }

    /// Take every message queued for `peer_id`, oldest first.
    ///
    /// SELECT and DELETE share a transaction: a crash between them would
    /// otherwise send the same messages twice.
    pub fn drain(&self, peer_id: &UserId) -> Result<Vec<PendingDm>, StorageError> {
        let conn = self.db.connection()?;
        let peer = peer_id.to_string();
        let tx = conn
            .unchecked_transaction()
            .map_err(StorageError::Database)?;

        let mut stmt = tx.prepare(
            "SELECT id, content, attachment_ids_blob, channel_id FROM pending_dm_messages
             WHERE peer_id = ?1 ORDER BY id ASC",
        )?;
        let messages = {
            let mut rows = stmt.query(params![peer])?;
            super::repo_utils::collect_decoded_rows(&mut rows, "pending_dm", |row| {
                let blob: Option<Vec<u8>> = row.get(2)?;
                let channel_id: Option<String> = row.get(3)?;
                Ok(PendingDm {
                    content: row.get(1)?,
                    attachment_ids: blob
                        .map(|blob| crate::storage::serialization::db_deserialize(&blob))
                        .transpose()?
                        .unwrap_or_default(),
                    channel_id: channel_id
                        .map(|id| {
                            id.parse()
                                .map_err(|_| StorageError::InvalidData("Invalid channel ID".into()))
                        })
                        .transpose()?,
                })
            })?
        };
        drop(stmt);

        tx.execute(
            "DELETE FROM pending_dm_messages WHERE peer_id = ?1",
            params![peer],
        )?;
        tx.commit().map_err(StorageError::Database)?;
        Ok(messages)
    }
}
