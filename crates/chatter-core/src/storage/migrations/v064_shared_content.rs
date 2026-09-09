//! V064: Shared-content index + peer last-seen.
//!
//! ## Why a table and not a query
//!
//! "Every photo in conversation X, newest first" was not expressible in SQL.
//! `attachments` carries no conversation, no message and no category — its
//! MIME type lives inside the bincode `metadata_blob` — and the message side
//! of the link is a bincode `Vec<AttachmentId>` in `messages.attachments_blob`.
//! Answering the question meant deserializing every message in the
//! conversation, which is O(messages) for a screen that pages 60 items.
//!
//! `shared_content` is the missing index: one row per shareable item, carrying
//! the conversation key, the tab it belongs in and a copy of `sent_at`, so a
//! tab is one index range-scan.
//!
//! ## Why `category` can start out `'unsorted'`
//!
//! The two writers do not agree on order. Sending a file inserts the
//! `attachments` row first (`AttachmentService::upload_to_cdn_encrypted`) and
//! the message after; receiving one inserts the message first and creates the
//! attachment rows after (`inbox/polling/dm.rs`). So whichever side indexes
//! has to tolerate the other not having run yet: the message insert writes
//! `'unsorted'` when the attachment row is missing, and the attachment insert
//! resolves any `'unsorted'` row naming it. Nothing renders `'unsorted'`.
//!
//! Links have no attachment row at all (`attachment_id = ''`) — they are
//! extracted straight from the message text.
//!
//! ## Also here: `profiles.last_seen_at`
//!
//! `PresenceUpdate.timestamp` has been on the wire and thrown away in
//! `events/handlers.rs` since presence shipped. The profile screen's "last
//! seen" line is that value, persisted next to the presence it arrives with.

use super::Migration;

/// V064: Create shared_content index and add profiles.last_seen_at.
pub struct V064SharedContent;

impl Migration for V064SharedContent {
    fn version(&self) -> u32 {
        64
    }

    fn description(&self) -> &'static str {
        "Create shared_content index and profiles.last_seen_at"
    }

    fn up(&self) -> &'static str {
        r#"
        CREATE TABLE shared_content (
            -- The message this item came in. CASCADE so deleting a message
            -- takes its media out of the drawer with it.
            message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
            -- The attachment, or '' for a link (a URL in message text has no
            -- attachment record). Empty string rather than NULL so the UNIQUE
            -- below actually constrains link rows — SQLite treats NULLs as
            -- distinct.
            attachment_id TEXT NOT NULL DEFAULT '',
            -- Conversation key, same vocabulary the messages table routes on.
            conversation_type TEXT NOT NULL,
            conversation_id TEXT NOT NULL,
            -- `SharedCategory::as_str`. 'unsorted' is the transient state for
            -- an attachment whose row had not landed at index time.
            category TEXT NOT NULL,
            -- Copy of messages.sent_at so a tab query never joins to order.
            sent_at INTEGER NOT NULL,
            -- URL for a link, file name otherwise. Denormalised so the Files
            -- and Links tabs render without decoding metadata_blob per row.
            title TEXT NOT NULL DEFAULT '',
            size_bytes INTEGER NOT NULL DEFAULT 0,
            -- Re-indexing the same message (retry, resync) must not duplicate.
            UNIQUE(message_id, attachment_id, title)
        );

        -- The one query this table exists for: a conversation's tab, newest
        -- first, paged. Covers filter + order, so a page is a range scan.
        CREATE INDEX idx_shared_content_tab
            ON shared_content(conversation_type, conversation_id, category, sent_at DESC);

        -- Resolving 'unsorted' rows when the attachment row finally arrives.
        CREATE INDEX idx_shared_content_attachment
            ON shared_content(attachment_id) WHERE attachment_id != '';

        -- Peer last-seen: unix seconds of the last presence update we saw for
        -- this user. NULL = never seen one.
        ALTER TABLE profiles ADD COLUMN last_seen_at INTEGER;
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
            DROP INDEX IF EXISTS idx_shared_content_attachment;
            DROP INDEX IF EXISTS idx_shared_content_tab;
            DROP TABLE IF EXISTS shared_content;
            "#,
        )
    }
}
