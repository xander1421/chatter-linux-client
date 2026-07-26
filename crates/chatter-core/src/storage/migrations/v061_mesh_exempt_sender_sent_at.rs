//! V061: exempt mesh conversations from the Signal `(sender, sent_at)` dedup key.
//!
//! `idx_messages_sender_sent_at` is a unique index on
//! `(conversation_type, conversation_id, sender_id, sent_at)`, added with the
//! unified message table as Signal's dedup rule: on the server path, a sender
//! plus a send timestamp *is* a message's identity, so a re-delivery of the same
//! message carries the same pair and collapses onto the existing row. That is
//! correct there.
//!
//! It is not correct for the mesh, which identifies a message by the ID on the
//! wire. `MeshTimeline::record` relies on exactly that: the row's primary key is
//! `wire_message_id(wire_id)`, so a packet that arrives twice — flooded, relayed,
//! and again out of a peer's gossip archive — collapses by primary key, which is
//! the intended dedup and is unaffected by this change.
//!
//! What the extra index did instead was drop *distinct* messages. `sent_at` is
//! milliseconds widened to nanoseconds (`mapping::millis_to_nanos`), so any two
//! mesh messages from one sender to one conversation within the same millisecond
//! were the same key, and `INSERT OR IGNORE` discarded the second one silently:
//! no error, no event, no row. The mesh has no server echo, so that row is the
//! only copy of the message — there is nothing to reconcile it back from later.
//!
//! A millisecond is too narrow for a person typing and wide enough for the
//! machine: two sends in one pass of the runtime, a burst arriving off one radio
//! read, or a relayed pair from the same peer all land inside it. It surfaced as
//! `a_sent_dm_is_on_disk_by_the_time_send_returns` failing under load, where two
//! back-to-back sends of the same text produced one row instead of two.
//!
//! The index is recreated with the two mesh conversation types excluded, so
//! server-path dedup is untouched and mesh rows are deduped only by the wire ID
//! they were always keyed on. The values match
//! [`ConversationType::as_str`](crate::domain::message::ConversationType::as_str).

use super::Migration;

pub struct V061MeshExemptSenderSentAt;

impl Migration for V061MeshExemptSenderSentAt {
    fn version(&self) -> u32 {
        61
    }

    fn description(&self) -> &'static str {
        "Mesh conversations are keyed by wire ID, not (sender, sent_at)"
    }

    fn up(&self) -> &'static str {
        r#"
        DROP INDEX IF EXISTS idx_messages_sender_sent_at;

        CREATE UNIQUE INDEX idx_messages_sender_sent_at
            ON messages(conversation_type, conversation_id, sender_id, sent_at)
            WHERE sender_id IS NOT NULL
              AND sent_at > 0
              AND conversation_type NOT IN ('mesh', 'mesh_dm');
        "#
    }
}
