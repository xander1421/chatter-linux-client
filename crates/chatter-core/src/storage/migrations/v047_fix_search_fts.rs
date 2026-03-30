//! V047: Fix FTS5 search triggers after unified messages migration.
//!
//! v045 renamed `channel_id` to `conversation_id` in the messages table,
//! but the FTS triggers from v009 still reference `NEW.channel_id`.
//! This migration drops the broken triggers, recreates the FTS table
//! with `conversation_id`, and rebuilds the index.

use super::Migration;

/// V047: Fix FTS5 search index for unified messages table.
pub struct V047FixSearchFts;

impl Migration for V047FixSearchFts {
    fn version(&self) -> u32 {
        47
    }

    fn description(&self) -> &'static str {
        "Fix FTS5 search triggers for unified messages (channel_id -> conversation_id)"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Drop broken triggers that reference non-existent channel_id
        DROP TRIGGER IF EXISTS messages_fts_insert;
        DROP TRIGGER IF EXISTS messages_fts_update;
        DROP TRIGGER IF EXISTS messages_fts_delete;

        -- Drop and recreate FTS table with conversation_id
        DROP TABLE IF EXISTS messages_fts;

        CREATE VIRTUAL TABLE messages_fts USING fts5(
            message_id UNINDEXED,
            conversation_id UNINDEXED,
            content,
            content='',
            contentless_delete=1
        );

        -- Recreate triggers using conversation_id from unified messages table
        CREATE TRIGGER messages_fts_insert AFTER INSERT ON messages
        WHEN NEW.content_type IN (0, 5, 6)
        BEGIN
            INSERT INTO messages_fts(message_id, conversation_id, content)
            SELECT
                NEW.id,
                NEW.conversation_id,
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

        CREATE TRIGGER messages_fts_update AFTER UPDATE ON messages
        WHEN OLD.content_type IN (0, 5, 6) OR NEW.content_type IN (0, 5, 6)
        BEGIN
            DELETE FROM messages_fts WHERE message_id = OLD.id;

            INSERT INTO messages_fts(message_id, conversation_id, content)
            SELECT
                NEW.id,
                NEW.conversation_id,
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

        CREATE TRIGGER messages_fts_delete AFTER DELETE ON messages
        WHEN OLD.content_type IN (0, 5, 6)
        BEGIN
            DELETE FROM messages_fts WHERE message_id = OLD.id;
        END;

        -- Rebuild index from existing messages
        INSERT INTO messages_fts(message_id, conversation_id, content)
        SELECT
            id,
            conversation_id,
            CASE content_type
                WHEN 0 THEN json_extract(content_json, '$')
                WHEN 5 THEN json_extract(content_json, '$.AI.text')
                WHEN 6 THEN json_extract(content_json, '$.Mixed.text')
            END
        FROM messages
        WHERE content_type IN (0, 5, 6)
          AND COALESCE(
              CASE content_type
                  WHEN 0 THEN json_extract(content_json, '$')
                  WHEN 5 THEN json_extract(content_json, '$.AI.text')
                  WHEN 6 THEN json_extract(content_json, '$.Mixed.text')
              END, ''
          ) != '';
        "#
    }

    fn down(&self) -> Option<&'static str> {
        None
    }
}
