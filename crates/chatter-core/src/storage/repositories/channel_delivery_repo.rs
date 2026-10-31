//! Durable receipt obligations; notifications are only coalesced wake hints.
use std::sync::Arc;
use rusqlite::params;
use crate::{domain::ids::{ChannelId, MessageId, UserId}, error::StorageError, storage::Database};

pub(crate) struct PendingDelivery {
    pub message_id: MessageId,
    pub conversation_id: ChannelId,
    pub sent_at: u64,
    pub attempts: u32,
    pub author: UserId,
}

pub(crate) struct ChannelDeliveryRepository {
    db: Arc<Database>,
}

impl ChannelDeliveryRepository {
    pub fn new(db: Arc<Database>) -> Self { Self { db } }

    /// At most fifty lightweight identifiers; encrypted message bodies stay in SQL.
    pub fn due(&self, owner: UserId, now_ms: i64) -> Result<Vec<PendingDelivery>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare_cached(
            "SELECT o.message_id, o.conversation_id, o.sent_at, o.attempts, m.sender_id
             FROM channel_delivery_outbox o JOIN messages m ON m.id=o.message_id
             WHERE o.owner_id=?1 AND o.retry_after_ms<=?2
             ORDER BY o.retry_after_ms, o.rowid LIMIT 50"
        )?;
        let rows = stmt.query_map(params![owner.to_string(), now_ms], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, u32>(3)?, r.get::<_, String>(4)?))
        })?;
        rows.map(|row| {
            let (id, channel, sent_at, attempts, author) = row?;
            Ok(PendingDelivery {
                message_id: id.parse().map_err(|_| StorageError::InvalidData("Invalid delivery message id".into()))?,
                conversation_id: channel.parse().map_err(|_| StorageError::InvalidData("Invalid delivery channel id".into()))?,
                sent_at: sent_at as u64,
                attempts,
                author: author.parse().map_err(|_| StorageError::InvalidData("Invalid delivery author id".into()))?,
            })
        }).collect()
    }

    /// Only the exact attempted IDs are settled; concurrent inserts cannot be lost.
    pub fn settle(&self, owner: UserId, deliveries: &[PendingDelivery], success: bool, now_ms: i64) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let tx = conn.unchecked_transaction()?;
        {
            let mut delete = tx.prepare_cached("DELETE FROM channel_delivery_outbox WHERE owner_id=?1 AND message_id=?2")?;
            let mut retry = tx.prepare_cached("UPDATE channel_delivery_outbox SET attempts=MIN(attempts+1,16), retry_after_ms=?3 WHERE owner_id=?1 AND message_id=?2")?;
            for item in deliveries {
                if success {
                    delete.execute(params![owner.to_string(), item.message_id.to_string()])?;
                } else {
                    let delay_ms = (5_000i64 << item.attempts.min(4)).min(60_000);
                    retry.execute(params![owner.to_string(), item.message_id.to_string(), now_ms.saturating_add(delay_ms)])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{domain::message::{ConversationType, Message}, storage::repositories::{SettingsRepository, MessageRepository}};
    #[test]
    fn receipt_intent_survives_restart_is_owner_scoped_and_settles_exact_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("delivery.db");
        let owner = UserId::new();
        let peer = UserId::new();
        let channel = ChannelId::new();
        let first = Message::new_text(channel, ConversationType::Channel, Some(peer), "first".into(), 100);
        let own = Message::new_text(channel, ConversationType::Channel, Some(owner), "own".into(), 200);
        {
            let db = Arc::new(Database::open(&path, "fixture-only").unwrap());
            db.run_migrations().unwrap();
            SettingsRepository::new(db.clone()).set_string(crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID, &owner.to_string()).unwrap();
            let messages = MessageRepository::new(db.clone());
            messages.insert_or_ignore(&first).unwrap();
            messages.insert_or_ignore(&first).unwrap();
            messages.insert(&own).unwrap();
            assert_eq!(ChannelDeliveryRepository::new(db).due(owner, 0).unwrap().len(), 1);
        }
        let db = Arc::new(Database::open(&path, "fixture-only").unwrap());
        let receipts = ChannelDeliveryRepository::new(db.clone());
        assert!(receipts.due(peer, 0).unwrap().is_empty());
        let batch = receipts.due(owner, 0).unwrap();
        receipts.settle(owner, &batch, false, 1000).unwrap();
        assert!(receipts.due(owner, 5999).unwrap().is_empty());
        let second = Message::new_text(channel, ConversationType::Channel, Some(peer), "later".into(), 300);
        MessageRepository::new(db).insert(&second).unwrap();
        receipts.settle(owner, &batch, true, 6000).unwrap();
        let remaining = receipts.due(owner, 6000).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].message_id, second.id);
    }
    #[test]
    fn failure_to_record_receipt_intent_rolls_back_the_message() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open(&dir.path().join("atomic.db"), "fixture-only").unwrap());
        db.run_migrations().unwrap();
        let owner = UserId::new();
        SettingsRepository::new(db.clone()).set_string(crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID, &owner.to_string()).unwrap();
        db.connection().unwrap().execute_batch("CREATE TRIGGER reject_receipt BEFORE INSERT ON channel_delivery_outbox BEGIN SELECT RAISE(ABORT, 'fixture failure'); END;").unwrap();
        let message = Message::new_text(ChannelId::new(), ConversationType::Channel, Some(UserId::new()), "atomic".into(), 123);
        let messages = MessageRepository::new(db.clone());
        assert!(messages.insert(&message).is_err());
        assert!(messages.get(&message.id).unwrap().is_none(), "a committed receive must never lack its durable receipt obligation");
        assert!(ChannelDeliveryRepository::new(db).due(owner, i64::MAX).unwrap().is_empty());
    }

    #[test]
    fn identical_channel_clocks_keep_distinct_delivery_authors() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open(&dir.path().join("authors.db"), "fixture-only").unwrap());
        db.run_migrations().unwrap();
        let owner = UserId::new();
        SettingsRepository::new(db.clone()).set_string(crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID, &owner.to_string()).unwrap();
        let channel = ChannelId::new();
        let a = Message::new_text(channel, ConversationType::Channel, Some(UserId::new()), "a".into(), 123);
        let b = Message::new_text(channel, ConversationType::Channel, Some(UserId::new()), "b".into(), 123);
        let messages = MessageRepository::new(db.clone());
        messages.insert(&a).unwrap();
        messages.insert(&b).unwrap();
        let pending = ChannelDeliveryRepository::new(db).due(owner, 0).unwrap();
        assert_eq!(pending.len(), 2);
        for item in pending {
            let expected = if item.message_id == a.id { a.sender_id } else { b.sender_id };
            assert_eq!(Some(item.author), expected, "durable ACK must target the exact row author");
        }
    }

}
