//! V009: FTS5 search index migration.
//!
//! Adds full-text search for message content using SQLite FTS5.

use super::Migration;

/// V009: Create FTS5 search index for messages.
pub struct V009SearchIndex;

impl Migration for V009SearchIndex {
    fn version(&self) -> u32 {
        9
    }

    fn description(&self) -> &'static str {
        "Create FTS5 search index for messages"
    }

    fn up(&self) -> &'static str {
        r#"
        -- FTS5 virtual table for message search
        -- content='' makes this a contentless table (we manage content manually)
        -- contentless_delete=1 allows DELETE without knowing the content
        CREATE VIRTUAL TABLE messages_fts USING fts5(
            message_id UNINDEXED,
            channel_id UNINDEXED,
            content,
            content='',
            contentless_delete=1
        );

        -- Trigger to sync FTS on INSERT
        -- Only index searchable content types: 0=Text, 5=AI, 6=Mixed
        CREATE TRIGGER messages_fts_insert AFTER INSERT ON messages
        WHEN NEW.content_type IN (0, 5, 6)
        BEGIN
            INSERT INTO messages_fts(message_id, channel_id, content)
            SELECT
                NEW.id,
                NEW.channel_id,
                CASE NEW.content_type
                    WHEN 0 THEN json_extract(NEW.content_json, '$')
                    WHEN 5 THEN json_extract(NEW.content_json, '$.AI.text')
                    WHEN 6 THEN json_extract(NEW.content_json, '$.Mixed.text')
                END
            WHERE COALESCE(
                CASE NEW.content_type
                    WHEN 0 THEN json_extract(NEW.content_json, '$')
                    WHEN 5 THEN json_extract(NEW.content_json, '$.AI.text')
                    WHEN 6 THEN json_extract(NEW.content_json, '$.Mixed.text')
                END, ''
            ) != '';
        END;

        -- Trigger to sync FTS on UPDATE
        -- Delete old entry and insert new if content changed
        CREATE TRIGGER messages_fts_update AFTER UPDATE ON messages
        WHEN OLD.content_type IN (0, 5, 6) OR NEW.content_type IN (0, 5, 6)
        BEGIN
            -- Delete old entry if it was indexed
            DELETE FROM messages_fts WHERE message_id = OLD.id;

            -- Insert new entry if it should be indexed
            INSERT INTO messages_fts(message_id, channel_id, content)
            SELECT
                NEW.id,
                NEW.channel_id,
                CASE NEW.content_type
                    WHEN 0 THEN json_extract(NEW.content_json, '$')
                    WHEN 5 THEN json_extract(NEW.content_json, '$.AI.text')
                    WHEN 6 THEN json_extract(NEW.content_json, '$.Mixed.text')
                END
            WHERE NEW.content_type IN (0, 5, 6)
              AND COALESCE(
                  CASE NEW.content_type
                      WHEN 0 THEN json_extract(NEW.content_json, '$')
                      WHEN 5 THEN json_extract(NEW.content_json, '$.AI.text')
                      WHEN 6 THEN json_extract(NEW.content_json, '$.Mixed.text')
                  END, ''
              ) != '';
        END;

        -- Trigger to sync FTS on DELETE
        CREATE TRIGGER messages_fts_delete AFTER DELETE ON messages
        WHEN OLD.content_type IN (0, 5, 6)
        BEGIN
            DELETE FROM messages_fts WHERE message_id = OLD.id;
        END;
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
            DROP TRIGGER IF EXISTS messages_fts_insert;
            DROP TRIGGER IF EXISTS messages_fts_update;
            DROP TRIGGER IF EXISTS messages_fts_delete;
            DROP TABLE IF EXISTS messages_fts;
            "#,
        )
    }
}
