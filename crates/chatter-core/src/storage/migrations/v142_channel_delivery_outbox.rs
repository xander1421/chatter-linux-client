use super::Migration;

pub struct V142ChannelDeliveryOutbox;

impl Migration for V142ChannelDeliveryOutbox {
    fn version(&self) -> u32 { 142 }
    fn description(&self) -> &'static str { "Durable bounded channel delivery dispatch" }
    fn up(&self) -> &'static str {
        "CREATE TABLE channel_delivery_outbox (
            owner_id TEXT NOT NULL,
            message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
            conversation_id TEXT NOT NULL,
            sent_at INTEGER NOT NULL,
            attempts INTEGER NOT NULL DEFAULT 0,
            retry_after_ms INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (owner_id, message_id)
        );
        CREATE INDEX channel_delivery_outbox_due
            ON channel_delivery_outbox(owner_id, retry_after_ms);"
    }
}
