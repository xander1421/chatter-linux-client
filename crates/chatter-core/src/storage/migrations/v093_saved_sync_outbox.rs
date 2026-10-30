//! Durable coalesced sync intent for locally mutated own Saved records.
use super::Migration;
pub struct V093SavedSyncOutbox;
impl Migration for V093SavedSyncOutbox {
    fn version(&self) -> u32 {
        93
    }
    fn description(&self) -> &'static str {
        "Durable Saved sync outbox and upgrade recovery"
    }
    fn up(&self) -> &'static str {
        "CREATE TABLE saved_sync_outbox (
            owner_id TEXT NOT NULL,
            message_id TEXT NOT NULL,
            generation INTEGER NOT NULL DEFAULT 1,
            attempts INTEGER NOT NULL DEFAULT 0,
            next_attempt_at INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY(owner_id, message_id)
        );
        CREATE INDEX saved_sync_outbox_due ON saved_sync_outbox(owner_id, next_attempt_at, message_id);
        CREATE INDEX saved_sync_outbox_message ON saved_sync_outbox(message_id);
        CREATE TRIGGER saved_sync_outbox_on_delete AFTER DELETE ON messages BEGIN
            DELETE FROM saved_sync_outbox WHERE message_id = OLD.id;
        END;
        CREATE TRIGGER saved_sync_outbox_on_soft_delete AFTER UPDATE OF is_deleted ON messages
        WHEN NEW.is_deleted = 1 BEGIN
            DELETE FROM saved_sync_outbox WHERE message_id = NEW.id;
        END;
        CREATE TRIGGER saved_sync_outbox_on_tombstone AFTER INSERT ON deleted_messages BEGIN
            DELETE FROM saved_sync_outbox WHERE message_id = NEW.id;
        END;
        INSERT INTO saved_sync_outbox (owner_id, message_id)
        SELECT sender_id, id FROM messages
        WHERE conversation_type = 'saved_messages' AND conversation_id = sender_id
          AND sender_id IS NOT NULL AND sent_at > 0 AND is_deleted = 0
          AND NOT EXISTS (SELECT 1 FROM deleted_messages WHERE id = messages.id);"
    }
}
