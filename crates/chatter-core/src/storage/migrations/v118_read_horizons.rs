//! Persist the last read position across out-of-order own-device sync and message delivery.
use super::Migration;

pub struct V118ReadHorizons;

impl Migration for V118ReadHorizons {
    fn version(&self) -> u32 {
        118
    }

    fn description(&self) -> &'static str {
        "Read horizons for server conversations"
    }

    fn up(&self) -> &'static str {
        "CREATE TABLE conversation_read_horizons (
            conversation_type TEXT NOT NULL CHECK (conversation_type IN ('server_dm', 'channel')),
            conversation_id TEXT NOT NULL,
            sent_at INTEGER NOT NULL,
            PRIMARY KEY (conversation_type, conversation_id)
        );
        INSERT INTO conversation_read_horizons (conversation_type, conversation_id, sent_at)
            SELECT conversation_type, conversation_id, MAX(sent_at) FROM messages
            WHERE conversation_type IN ('server_dm', 'channel')
              AND content_type NOT IN (3, 4)
              AND sender_id = (SELECT json_extract(value, '$') FROM settings
                               WHERE key = 'auth.current_user_id')
            GROUP BY conversation_type, conversation_id;
        UPDATE messages SET is_read = 1 WHERE is_read = 0
          AND conversation_type IN ('server_dm', 'channel')
          AND sent_at <= (SELECT h.sent_at FROM conversation_read_horizons h
                          WHERE h.conversation_type = messages.conversation_type
                            AND h.conversation_id = messages.conversation_id);"
    }
}
