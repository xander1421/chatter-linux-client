use super::Migration;

pub struct V145PrivateChannelReceipts;

impl Migration for V145PrivateChannelReceipts {
    fn version(&self) -> u32 {
        145
    }

    fn description(&self) -> &'static str {
        "Remove private channel delivery and read state"
    }

    fn up(&self) -> &'static str {
        r#"
        DELETE FROM channel_delivery_outbox
          WHERE conversation_id IN (SELECT id FROM channels WHERE channel_type=1);
        DELETE FROM pending_message_receipts
          WHERE conversation_type='channel'
            AND conversation_id IN (SELECT id FROM channels WHERE channel_type=1);
        -- Preserve the original destinations used to authorize a sender-key resend.
        UPDATE message_receipt_targets SET delivered_at=NULL, read_at=NULL
          WHERE conversation_type='channel'
            AND conversation_id IN (SELECT id FROM channels WHERE channel_type=1);
        DROP TRIGGER messages_delivery_monotonic;
        UPDATE messages SET delivery_status='sent'
          WHERE conversation_type='channel' AND delivery_status IN ('delivered', 'read')
            AND conversation_id IN (SELECT id FROM channels WHERE channel_type=1);
        CREATE TRIGGER messages_delivery_monotonic
        AFTER UPDATE OF delivery_status ON messages
        WHEN (OLD.delivery_status = 'read' AND NEW.delivery_status != 'read')
          OR (OLD.delivery_status = 'delivered' AND NEW.delivery_status NOT IN ('delivered', 'read'))
        BEGIN
            UPDATE messages SET delivery_status = OLD.delivery_status WHERE id = NEW.id;
        END;
        "#
    }
}
