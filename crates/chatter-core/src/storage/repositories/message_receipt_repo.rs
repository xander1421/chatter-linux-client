//! Receipt evidence is scoped to an outgoing message and its send-time recipients.
use crate::domain::ids::{ChannelId, DeviceId, MessageId, UserId};
use crate::domain::message::ConversationType;
use crate::error::StorageError;
use crate::storage::Database;
use rusqlite::{OptionalExtension, params};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceReceipt {
    pub recipient_id: UserId,
    pub device_id: DeviceId,
    pub delivered_at: Option<u64>,
    pub read_at: Option<u64>,
}

pub struct MessageReceiptRepository {
    db: Arc<Database>,
}

impl MessageReceiptRepository {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    pub fn targets_for_send(
        &self,
        conversation: ChannelId,
        kind: ConversationType,
        author: UserId,
        sent_at: u64,
    ) -> Result<Vec<(UserId, DeviceId)>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT recipient_id, device_id FROM message_receipt_targets
            WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND sent_at=?4
            ORDER BY recipient_id, device_id",
        )?;
        let rows = stmt.query_map(
            params![
                conversation.to_string(),
                kind.as_str(),
                author.to_string(),
                sent_at as i64
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?)),
        )?;
        rows.map(|row| {
            let (user, device) = row?;
            Ok((
                user.parse().map_err(|_| {
                    StorageError::InvalidData("invalid receipt recipient ID".into())
                })?,
                DeviceId(device),
            ))
        })
        .collect()
    }

    /// Our own other devices a send was for. Kept apart from the receipt
    /// targets: they read our messages through the same sender key, but their
    /// read state travels by ReadSync and must not hold back the aggregate.
    /// Only a resend within its 24h window reads these, so older rows go.
    pub fn snapshot_own_devices_for_send(
        &self,
        conversation: ChannelId,
        kind: ConversationType,
        author: UserId,
        sent_at: u64,
        devices: &[DeviceId],
    ) -> Result<(), StorageError> {
        const RETENTION_NS: u64 = 24 * 60 * 60 * 1_000_000_000;
        if devices.iter().any(|device| device.0 == 0) {
            return Err(StorageError::InvalidData("invalid own device".into()));
        }
        let sent_at = i64::try_from(sent_at)
            .map_err(|_| StorageError::InvalidData("message timestamp out of range".into()))?;
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM own_device_send_targets WHERE sent_at < ?1",
            params![sent_at.saturating_sub(RETENTION_NS as i64)])?;
        for device in devices {
            tx.execute("INSERT OR IGNORE INTO own_device_send_targets
                (conversation_id, conversation_type, author_id, sent_at, device_id) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![conversation.to_string(), kind.as_str(), author.to_string(), sent_at, device.0])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn own_devices_for_send(
        &self,
        conversation: ChannelId,
        kind: ConversationType,
        author: UserId,
        sent_at: u64,
    ) -> Result<Vec<DeviceId>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT device_id FROM own_device_send_targets
            WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND sent_at=?4
            ORDER BY device_id",
        )?;
        let rows = stmt.query_map(
            params![conversation.to_string(), kind.as_str(), author.to_string(), sent_at as i64],
            |row| row.get::<_, u32>(0).map(DeviceId),
        )?;
        rows.map(|row| row.map_err(StorageError::from)).collect()
    }

    /// Convenience for an already-stored outgoing message.
    pub fn snapshot_targets(
        &self,
        message_id: MessageId,
        author: UserId,
        targets: &[(UserId, DeviceId)],
    ) -> Result<(), StorageError> {
        let message = super::unified_message_repo::UnifiedMessageRepository::new(self.db.clone())
            .get(&message_id)?
            .ok_or_else(|| StorageError::InvalidData("missing outgoing message".into()))?;
        if message.sender_id != Some(author) {
            return Err(StorageError::InvalidData("not the message author".into()));
        }
        self.snapshot_for_send(
            message.conversation_id,
            message.conversation_type,
            author,
            message.sent_at,
            targets,
        )
    }

    /// Capture all recipients before transport. The logical message key allows
    /// receipts to arrive before the server-assigned message row is inserted.
    pub fn snapshot_for_send(
        &self,
        conversation: ChannelId,
        kind: ConversationType,
        author: UserId,
        sent_at: u64,
        targets: &[(UserId, DeviceId)],
    ) -> Result<(), StorageError> {
        if targets
            .iter()
            .any(|(user, device)| *user == author || device.0 == 0)
        {
            return Err(StorageError::InvalidData(
                "invalid receipt recipient".into(),
            ));
        }
        let sent_at = i64::try_from(sent_at)
            .map_err(|_| StorageError::InvalidData("message timestamp out of range".into()))?;
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM message_receipt_targets WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND sent_at=?4)",
            params![conversation.to_string(), kind.as_str(), author.to_string(), sent_at], |r| r.get(0))?;
        {
            for (user, device) in targets {
                if exists {
                    let original_recipient: bool = tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM message_receipt_targets WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND sent_at=?4 AND recipient_id=?5)",
                        params![conversation.to_string(), kind.as_str(), author.to_string(), sent_at, user.to_string()], |row| row.get(0))?;
                    if !original_recipient {
                        continue;
                    }
                }
                tx.execute("INSERT OR IGNORE INTO message_receipt_targets
                    (message_id, conversation_id, conversation_type, author_id, sent_at, recipient_id, device_id)
                    VALUES ((SELECT id FROM messages WHERE conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at=?4), ?1, ?2, ?3, ?4, ?5, ?6)",
                    params![conversation.to_string(), kind.as_str(), author.to_string(), sent_at, user.to_string(), device.0])?;
            }
        }
        tx.commit()?;
        drop(conn);
        // An authenticated peer receipt can beat the own-device transcript.
        // Only evidence matching the now-known snapshot is materialized.
        self.bind_send_timestamp(conversation, kind, author, sent_at as u64, sent_at as u64)
    }

    /// Replace a public-channel send's provisional clock with the server clock.
    pub fn bind_send_timestamp(
        &self,
        conversation: ChannelId,
        kind: ConversationType,
        author: UserId,
        provisional: u64,
        actual: u64,
    ) -> Result<(), StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        tx.execute("UPDATE message_receipt_targets SET sent_at=?5 WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND sent_at=?4 AND message_id IS NULL",
            params![conversation.to_string(), kind.as_str(), author.to_string(), provisional as i64, actual as i64])?;
        tx.execute("UPDATE OR IGNORE own_device_send_targets SET sent_at=?5 WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND sent_at=?4",
            params![conversation.to_string(), kind.as_str(), author.to_string(), provisional as i64, actual as i64])?;
        tx.execute("UPDATE message_receipt_targets AS r SET
            delivered_at=COALESCE(r.delivered_at, (SELECT p.delivered_at FROM pending_message_receipts p WHERE p.conversation_id=r.conversation_id AND p.conversation_type=r.conversation_type AND p.author_id=r.author_id AND p.sent_at=r.sent_at AND p.recipient_id=r.recipient_id AND p.device_id=r.device_id)),
            read_at=COALESCE(r.read_at, (SELECT p.read_at FROM pending_message_receipts p WHERE p.conversation_id=r.conversation_id AND p.conversation_type=r.conversation_type AND p.author_id=r.author_id AND p.sent_at=r.sent_at AND p.recipient_id=r.recipient_id AND p.device_id=r.device_id))
            WHERE r.conversation_id=?1 AND r.conversation_type=?2 AND r.author_id=?3 AND r.sent_at=?4",
            params![conversation.to_string(), kind.as_str(), author.to_string(), actual as i64])?;
        tx.execute("UPDATE message_receipt_targets SET message_id=(SELECT id FROM messages WHERE conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at=?4)
            WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND sent_at=?4",
            params![conversation.to_string(), kind.as_str(), author.to_string(), actual as i64])?;
        tx.execute("UPDATE messages SET delivery_status=CASE
            WHEN (SELECT MIN(read) FROM (SELECT MAX(read_at IS NOT NULL) AS read FROM message_receipt_targets WHERE message_id=messages.id GROUP BY recipient_id))=1 THEN 'read'
            WHEN (SELECT MIN(delivered) FROM (SELECT MAX(delivered_at IS NOT NULL) AS delivered FROM message_receipt_targets WHERE message_id=messages.id GROUP BY recipient_id))=1 THEN 'delivered'
            ELSE delivery_status END WHERE conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at=?4",
            params![conversation.to_string(), kind.as_str(), author.to_string(), actual as i64])?;
        tx.execute("DELETE FROM pending_message_receipts WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND sent_at=?4",
            params![conversation.to_string(), kind.as_str(), author.to_string(), actual as i64])?;
        tx.commit()?;
        Ok(())
    }

    /// Accept only authenticated, snapshotted devices; never infer a recipient
    /// from a timestamp or let one group member receipt another member's copy.
    /// A recipient has read once any of their devices has read; all recipients
    /// must reach a stage before the message's aggregate reaches that stage.
    pub fn apply(
        &self,
        conversation: ChannelId,
        kind: ConversationType,
        author: UserId,
        recipient: UserId,
        device: DeviceId,
        timestamps: &[u64],
        read: bool,
        received_at: u64,
    ) -> Result<Vec<MessageId>, StorageError> {
        if recipient == author || device.0 == 0 {
            return Ok(Vec::new());
        }
        let received_at = i64::try_from(received_at)
            .map_err(|_| StorageError::InvalidData("receipt timestamp out of range".into()))?;
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let mut changed = Vec::new();
        for timestamp in timestamps {
            let Ok(timestamp) = i64::try_from(*timestamp) else {
                continue;
            };
            let rows = tx.execute(
                "UPDATE message_receipt_targets SET delivered_at=COALESCE(delivered_at, ?7),
                 read_at=CASE WHEN ?8 THEN COALESCE(read_at, ?7) ELSE read_at END
                 WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND sent_at=?4
                   AND recipient_id=?5 AND device_id=?6",
                params![
                    conversation.to_string(),
                    kind.as_str(),
                    author.to_string(),
                    timestamp,
                    recipient.to_string(),
                    device.0,
                    received_at,
                    read
                ],
            )?;
            if rows == 0 {
                tx.execute(
                    "DELETE FROM pending_message_receipts WHERE delivered_at < ?1",
                    [received_at.saturating_sub(86_400_000_000_000)],
                )?;
                tx.execute("INSERT INTO pending_message_receipts
                    SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, CASE WHEN ?8 THEN ?7 ELSE NULL END
                    WHERE (EXISTS(SELECT 1 FROM message_receipt_targets WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND recipient_id=?5 AND device_id=?6 AND message_id IS NULL)
                        OR (?9 AND NOT EXISTS(SELECT 1 FROM message_receipt_targets WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND sent_at=?4)))
                    AND (SELECT COUNT(*) FROM pending_message_receipts) < 4096
                    ON CONFLICT(conversation_id, conversation_type, author_id, sent_at, recipient_id, device_id)
                    DO UPDATE SET read_at=COALESCE(pending_message_receipts.read_at, excluded.read_at)",
                    params![conversation.to_string(), kind.as_str(), author.to_string(), timestamp, recipient.to_string(), device.0, received_at, read,
                        kind == ConversationType::ServerDm && conversation == ChannelId::from(recipient)])?;
                continue;
            }
            let id: Option<String> = tx.query_row(
                "SELECT id FROM messages WHERE conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at=?4",
                params![conversation.to_string(), kind.as_str(), author.to_string(), timestamp], |r| r.get(0)).optional()?;
            // The insert trigger will apply evidence if the local echo is not here yet.
            let Some(id) = id else { continue };
            let (delivered, read): (bool, bool) = tx.query_row(
                "SELECT MIN(delivered), MIN(read) FROM (
                    SELECT MAX(delivered_at IS NOT NULL) AS delivered, MAX(read_at IS NOT NULL) AS read
                    FROM message_receipt_targets WHERE message_id = ?1 GROUP BY recipient_id
                 )", [&id], |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let status = if read {
                "read"
            } else if delivered {
                "delivered"
            } else {
                "sent"
            };
            tx.execute(
                "UPDATE messages SET delivery_status = ?1 WHERE id = ?2",
                params![status, id],
            )?;
            changed.push(
                id.parse()
                    .map_err(|_| StorageError::InvalidData("invalid receipt message ID".into()))?,
            );
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Message-info rows, including devices which have not acknowledged yet.
    pub fn for_message(&self, message_id: MessageId) -> Result<Vec<DeviceReceipt>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare("SELECT recipient_id, device_id, delivered_at, read_at FROM message_receipt_targets WHERE message_id = ?1 ORDER BY recipient_id, device_id")?;
        let rows = stmt.query_map([message_id.to_string()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, u32>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        })?;
        rows.map(|row| {
            let (user, device, delivered, read) = row?;
            Ok(DeviceReceipt {
                recipient_id: user.parse().map_err(|_| {
                    StorageError::InvalidData("invalid receipt recipient ID".into())
                })?,
                device_id: DeviceId(device),
                delivered_at: delivered.map(|v| v as u64),
                read_at: read.map(|v| v as u64),
            })
        })
        .collect()
    }
}
