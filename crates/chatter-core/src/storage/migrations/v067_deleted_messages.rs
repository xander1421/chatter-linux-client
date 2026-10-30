//! V067: Remember which messages were deleted locally.
//!
//! Local deletes (delete-for-me, a peer's delete-for-everyone, disappearing
//! timers) hard-delete the row so no plaintext lingers. The server still holds
//! channel messages, though, and the catch-up sync re-fetches them: with the
//! row gone the re-fetched copy looked new and was stored again. This table is
//! the tombstone that ingest checks instead — ids only, no content.

use super::Migration;

pub struct V067DeletedMessages;

impl Migration for V067DeletedMessages {
    fn version(&self) -> u32 {
        67
    }

    fn description(&self) -> &'static str {
        "Tombstones for locally deleted messages"
    }

    fn up(&self) -> &'static str {
        r#"
        CREATE TABLE IF NOT EXISTS deleted_messages (
            id TEXT PRIMARY KEY NOT NULL
        );
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some("DROP TABLE IF EXISTS deleted_messages;")
    }
}
