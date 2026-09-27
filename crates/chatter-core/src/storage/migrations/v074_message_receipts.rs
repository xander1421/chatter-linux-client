//! Receipt targets exist before transport, including before a server assigns an ID.
use super::Migration;
pub struct V074MessageReceipts;
impl Migration for V074MessageReceipts {
    fn version(&self) -> u32 {
        74
    }
    fn description(&self) -> &'static str {
        "Persist per-device message receipts"
    }
    fn up(&self) -> &'static str {
        r#"
        CREATE TABLE message_receipt_targets (
            message_id TEXT REFERENCES messages(id) ON DELETE CASCADE,
            conversation_id TEXT NOT NULL,
            conversation_type TEXT NOT NULL,
            author_id TEXT NOT NULL,
            sent_at INTEGER NOT NULL,
            recipient_id TEXT NOT NULL,
            device_id INTEGER NOT NULL CHECK (device_id > 0),
            delivered_at INTEGER,
            read_at INTEGER,
            PRIMARY KEY (conversation_id, conversation_type, author_id, sent_at, recipient_id, device_id)
        );
        -- Public-channel timestamps are assigned by the server; retain an
        -- authenticated acknowledgement until its pending send gets that clock.
        CREATE TABLE pending_message_receipts (
            conversation_id TEXT NOT NULL, conversation_type TEXT NOT NULL,
            author_id TEXT NOT NULL, sent_at INTEGER NOT NULL,
            recipient_id TEXT NOT NULL, device_id INTEGER NOT NULL,
            delivered_at INTEGER NOT NULL, read_at INTEGER,
            PRIMARY KEY(conversation_id, conversation_type, author_id, sent_at, recipient_id, device_id)
        );
        CREATE INDEX message_receipt_message ON message_receipt_targets(message_id);
        -- Bind receipts that beat the local echo, then materialize their aggregate.
        CREATE TRIGGER messages_attach_receipts AFTER INSERT ON messages BEGIN
            UPDATE message_receipt_targets SET message_id = NEW.id
             WHERE conversation_id = NEW.conversation_id AND conversation_type = NEW.conversation_type
               AND author_id = NEW.sender_id AND sent_at = NEW.sent_at;
            UPDATE messages SET delivery_status = CASE
                WHEN (SELECT MIN(read) FROM (
                    SELECT MAX(read_at IS NOT NULL) AS read FROM message_receipt_targets
                    WHERE message_id = NEW.id GROUP BY recipient_id)) = 1 THEN 'read'
                WHEN (SELECT MIN(delivered) FROM (
                    SELECT MAX(delivered_at IS NOT NULL) AS delivered FROM message_receipt_targets
                    WHERE message_id = NEW.id GROUP BY recipient_id)) = 1 THEN 'delivered'
                ELSE delivery_status END WHERE id = NEW.id;
        END;
        CREATE TRIGGER messages_delivery_monotonic
        AFTER UPDATE OF delivery_status ON messages
        WHEN (OLD.delivery_status = 'read' AND NEW.delivery_status != 'read')
          OR (OLD.delivery_status = 'delivered' AND NEW.delivery_status NOT IN ('delivered', 'read'))
        BEGIN
            UPDATE messages SET delivery_status = OLD.delivery_status WHERE id = NEW.id;
        END;
        "#
    }
    fn down(&self) -> Option<&'static str> {
        Some("DROP TRIGGER messages_attach_receipts; DROP TRIGGER messages_delivery_monotonic; DROP TABLE message_receipt_targets; DROP TABLE pending_message_receipts;")
    }
}
