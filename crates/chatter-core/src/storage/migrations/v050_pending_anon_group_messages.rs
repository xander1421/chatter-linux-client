//! Migration v050: Pending anonymous group messages table.
//!
//! Queues outbound anon group messages for members whose mailbox_pk is
//! not yet known (i.e. they haven't sent a JoinAck back to the admin).
//! When the JoinAck arrives and `mailbox_pk` is populated, the queue is
//! drained and messages are delivered via `send_to_anon_mailbox`.

use super::Migration;

pub struct V050PendingAnonGroupMessages;

impl Migration for V050PendingAnonGroupMessages {
    fn version(&self) -> u32 {
        50
    }

    fn description(&self) -> &'static str {
        "Add pending_anon_group_messages table for JoinAck-delayed fanout"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Pending anonymous group messages awaiting recipient's JoinAck.
        -- Rows are enqueued when send_message fans out to a member with
        -- mailbox_pk = NULL; drained in handle_join_ack once the mailbox
        -- public key becomes known.
        CREATE TABLE IF NOT EXISTS pending_anon_group_messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            group_id TEXT NOT NULL,
            recipient_user_id TEXT NOT NULL,
            -- Encoded Content proto bytes (pre-encryption, pre-padding).
            -- Re-encrypted per-recipient when drained since Signal Protocol
            -- sessions are pairwise.
            encrypted_content_blob BLOB NOT NULL,
            created_at TEXT NOT NULL,
            attempts INTEGER NOT NULL DEFAULT 0
        );

        -- Fast drain by (group_id, recipient_user_id) when a JoinAck lands
        CREATE INDEX IF NOT EXISTS idx_pending_anon_group_messages_group_recipient
            ON pending_anon_group_messages(group_id, recipient_user_id);
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
            DROP INDEX IF EXISTS idx_pending_anon_group_messages_group_recipient;
            DROP TABLE IF EXISTS pending_anon_group_messages;
            "#,
        )
    }
}
