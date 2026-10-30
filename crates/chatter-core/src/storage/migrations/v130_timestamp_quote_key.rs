use super::Migration;

pub struct V130TimestampQuoteKey;

impl Migration for V130TimestampQuoteKey {
    fn version(&self) -> u32 {
        130
    }

    fn description(&self) -> &'static str {
        "Index timestamp-derived message quote references"
    }

    fn up(&self) -> &'static str {
        // UUIDv5 is unavailable in SQLCipher SQL; pre-release databases may be dropped.
        "ALTER TABLE messages ADD COLUMN timestamp_quote_key TEXT;
         CREATE INDEX idx_messages_timestamp_quote_key
             ON messages(conversation_type, conversation_id, sender_id, timestamp_quote_key)
             WHERE sender_id IS NOT NULL AND timestamp_quote_key IS NOT NULL;"
    }
}
