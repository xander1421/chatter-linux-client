//! V051: Fix FTS5 search JSON path and table type.
//!
//! Previous migrations used a contentless FTS5 table (`content=''`), which
//! does not store column values and returns NULL for everything except rowid.
//! This migration recreates the table as a regular FTS5 table so that
//! `message_id` and `conversation_id` can be retrieved in search results.
//! It also fixes the JSON path for Text variant to `$.Text`.

use super::Migration;

pub struct V051FixSearchJsonPath;

impl Migration for V051FixSearchJsonPath {
    fn version(&self) -> u32 {
        51
    }

    fn description(&self) -> &'static str {
        "Fix FTS5 search JSON path and recreate table to store IDs"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Drop old triggers and table
        DROP TRIGGER IF EXISTS messages_fts_insert;
        DROP TRIGGER IF EXISTS messages_fts_update;
        DROP TRIGGER IF EXISTS messages_fts_delete;
        DROP TABLE IF EXISTS messages_fts;

        -- Create regular FTS5 table (not contentless) to store IDs
        CREATE VIRTUAL TABLE messages_fts USING fts5(
            message_id UNINDEXED,
            conversation_id UNINDEXED,
            content
        );

        -- Create fixed triggers
        CREATE TRIGGER messages_fts_insert AFTER INSERT ON messages
        WHEN NEW.is_deleted = 0 AND NEW.content_type IN (0, 5, 6)
        BEGIN
            INSERT INTO messages_fts(message_id, conversation_id, content)
            SELECT
                NEW.id,
                NEW.conversation_id,
                CASE NEW.content_type
                    WHEN 0 THEN json_extract(NEW.content_json, '$.Text')
                    WHEN 5 THEN json_extract(NEW.content_json, '$.AI.text')
                    WHEN 6 THEN json_extract(NEW.content_json, '$.Mixed.text')
                END
            WHERE COALESCE(
                CASE NEW.content_type
                    WHEN 0 THEN json_extract(NEW.content_json, '$.Text')
                    WHEN 5 THEN json_extract(NEW.content_json, '$.AI.text')
                    WHEN 6 THEN json_extract(NEW.content_json, '$.Mixed.text')
                END, ''
            ) != '';
        END;

        CREATE TRIGGER messages_fts_update AFTER UPDATE ON messages
        WHEN (OLD.content_json != NEW.content_json OR OLD.is_deleted != NEW.is_deleted)
        BEGIN
            -- Delete old index
            DELETE FROM messages_fts WHERE message_id = OLD.id;

            -- Insert new index if not deleted and has content
            INSERT INTO messages_fts(message_id, conversation_id, content)
            SELECT
                NEW.id,
                NEW.conversation_id,
                CASE NEW.content_type
                    WHEN 0 THEN json_extract(NEW.content_json, '$.Text')
                    WHEN 5 THEN json_extract(NEW.content_json, '$.AI.text')
                    WHEN 6 THEN json_extract(NEW.content_json, '$.Mixed.text')
                END
            WHERE NEW.is_deleted = 0 AND NEW.content_type IN (0, 5, 6)
              AND COALESCE(
                  CASE NEW.content_type
                      WHEN 0 THEN json_extract(NEW.content_json, '$.Text')
                      WHEN 5 THEN json_extract(NEW.content_json, '$.AI.text')
                      WHEN 6 THEN json_extract(NEW.content_json, '$.Mixed.text')
                  END, ''
              ) != '';
        END;

        CREATE TRIGGER messages_fts_delete AFTER DELETE ON messages
        BEGIN
            DELETE FROM messages_fts WHERE message_id = OLD.id;
        END;

        -- Rebuild the index completely from existing messages
        INSERT INTO messages_fts(message_id, conversation_id, content)
        SELECT
            id,
            conversation_id,
            CASE content_type
                WHEN 0 THEN json_extract(content_json, '$.Text')
                WHEN 5 THEN json_extract(content_json, '$.AI.text')
                WHEN 6 THEN json_extract(content_json, '$.Mixed.text')
            END
        FROM messages
        WHERE is_deleted = 0 AND content_type IN (0, 5, 6)
          AND COALESCE(
              CASE content_type
                  WHEN 0 THEN json_extract(content_json, '$.Text')
                  WHEN 5 THEN json_extract(content_json, '$.AI.text')
                  WHEN 6 THEN json_extract(content_json, '$.Mixed.text')
              END, ''
          ) != '';
        "#
    }
}
