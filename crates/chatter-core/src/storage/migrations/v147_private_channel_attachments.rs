use std::collections::HashSet;

use rusqlite::Connection;

use super::Migration;
use crate::domain::AttachmentId;
use crate::error::StorageError;
use crate::storage::serialization::db_deserialize;

pub struct V147PrivateChannelAttachments;

impl Migration for V147PrivateChannelAttachments {
    fn version(&self) -> u32 {
        147
    }

    fn description(&self) -> &'static str {
        "Retain private-channel attachment routing"
    }

    fn up(&self) -> &'static str {
        "ALTER TABLE attachments ADD COLUMN private_channel INTEGER NOT NULL DEFAULT 0;"
    }

    fn apply(&self, conn: &Connection) -> Result<(), StorageError> {
        conn.execute_batch(self.up())?;
        // Message references predate the lazily populated shared-content index.
        let mut select = conn.prepare(
            "SELECT m.attachments_blob FROM messages m
             LEFT JOIN channel_type_pins p ON p.channel_id = m.conversation_id
             WHERE m.conversation_type = 'channel' AND m.attachments_blob IS NOT NULL
               AND (p.channel_type IS NULL OR p.channel_type = 1)",
        )?;
        let mut seen = HashSet::new();
        let mut mark = conn.prepare("UPDATE attachments SET private_channel = 1 WHERE id = ?1")?;
        let mut rows = select.query([])?;
        while let Some(row) = rows.next()? {
            let blob: Vec<u8> = row.get(0)?;
            for id in db_deserialize::<Vec<AttachmentId>>(&blob).unwrap_or_default() {
                if seen.insert(id) {
                    mark.execute([id.to_string()])?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::serialization::db_serialize;
    use rusqlite::params;

    #[test]
    fn backfill_reads_message_references_without_a_shared_content_index() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE attachments(id TEXT PRIMARY KEY);
             CREATE TABLE messages(conversation_id TEXT, conversation_type TEXT, attachments_blob BLOB);
             CREATE TABLE channel_type_pins(channel_id TEXT PRIMARY KEY, channel_type INTEGER);
             INSERT INTO channel_type_pins VALUES ('public', 0);",
        )
        .unwrap();
        for channel in ["unknown", "public"] {
            let attachment = AttachmentId::from_cdn_key(channel);
            conn.execute(
                "INSERT INTO attachments VALUES (?1)",
                [attachment.to_string()],
            )
            .unwrap();
            let blob = db_serialize(&vec![attachment]).unwrap();
            conn.execute(
                "INSERT INTO messages VALUES (?1, 'channel', ?2)",
                params![channel, blob],
            )
            .unwrap();
        }
        V147PrivateChannelAttachments.apply(&conn).unwrap();
        let routed_private = |channel| -> bool {
            conn.query_row(
                "SELECT private_channel FROM attachments WHERE id = ?1",
                [AttachmentId::from_cdn_key(channel).to_string()],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert!(routed_private("unknown"));
        assert!(!routed_private("public"));
    }
}
