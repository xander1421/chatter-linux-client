//! Unified message repository.
//!
//! Single repository for all message types (channel, DM, anon DM, anon group,
//! saved messages). All queries use the `conversation_type` discriminant.
//!
//! Also owns reaction operations via the `unified_reactions` table, keeping
//! `reaction_counts_blob` and `current_user_reactions_blob` on message rows
//! in sync.

use crate::domain::ids::{AttachmentId, ChannelId, IntegrationId, MessageId, ReactionId, UserId};
use crate::domain::message::{
    ConversationType, Message, MessageContent, ReactionCount, Thread, UnreadAnchor,
};
use crate::domain::reaction::{Emoji, Reaction, ReactionScope};
use crate::error::StorageError;
use crate::storage::Database;
use crate::storage::serialization::{db_deserialize, db_serialize};
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use std::collections::HashMap;
use std::sync::Arc;

/// Unified message repository for all conversation types.
///
/// Replaces the three separate message repos (`message_repo`, `anon_dm_repo` messages,
/// `anon_group_repo` messages) with a single implementation that uses the
/// `conversation_type` discriminant.
pub struct UnifiedMessageRepository {
    db: Arc<Database>,
}

impl UnifiedMessageRepository {
    /// The database this repository writes, for sibling repositories that
    /// must write a row of theirs in the same transaction as a message
    /// (`SavedSyncRepository`).
    pub(crate) fn db(&self) -> Arc<Database> {
        Arc::clone(&self.db)
    }
}

/// Visible projection changes and exact outgoing completion are separate contracts.
pub(crate) struct ReactionWriteOutcome {
    pub changed: bool,
    pub completed: bool,
}

/// Add a freshly-inserted message's attachments and links to the
/// shared-content index (the profile screen's media drawer).
///
/// Best-effort: a failure here costs one tile in a media tab, and must never
/// take the message insert down with it.
fn index_shared_content(conn: &rusqlite::Connection, message: &Message) {
    if let Err(e) = super::shared_content_repo::index_message(conn, message) {
        tracing::warn!(message_id = %message.id, error = %e, "shared-content index failed");
    }
}

#[path = "../reaction_clock.rs"]
mod reaction_clock;

#[path = "../reaction_projection.rs"]
pub(crate) mod reaction_projection;

#[cfg(test)]
#[path = "../reaction_storage_regressions.rs"]
mod reaction_storage_regressions;

impl UnifiedMessageRepository {
    /// Create a new UnifiedMessageRepository.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// `is_read` as written. The local user's own messages — a send from
    /// another of their devices, a SyncSent copy, a backfill, a group message
    /// healed after its SKDM — are never unread to them. Every writer routes
    /// through `insert` / `insert_or_ignore` / `update`, so the rule lives here
    /// once instead of in each receive path (F41 put it in one, and the rest
    /// still badged the user's own messages).
    ///
    /// Call records and system notices are never unread either: they are
    /// history, not messages, and badging them lit the Chats tab for a
    /// conversation that showed nothing new (Signal counts neither).
    fn stored_is_read(&self, message: &Message) -> bool {
        use crate::services::current_user::CurrentUserProvider;
        message.is_read
            || matches!(
                message.content,
                MessageContent::System(_) | MessageContent::Call(_)
            )
            || message.sender_id.is_some_and(|sender| {
                super::settings_repo::SettingsRepository::new(self.db.clone())
                    .current_user_id()
                    .is_ok_and(|me| me == sender)
            })
    }

}

mod writes;
mod deletion;
mod pins_read;
mod reactions;
mod queries_edits;
mod threads_listing;
mod read_delivery;
mod reaction_projection_write;

#[cfg(test)]
mod tests;

// ============================================================================
// Column list & row mappers
// ============================================================================

// Timestamp-only quote IDs are hashes persisted with each new message.
fn resolve_timestamp_quote(
    conn: &rusqlite::Connection,
    parent: &MessageId,
    author: &UserId,
    conversation: &ChannelId,
    kind: &ConversationType,
) -> Result<Option<MessageId>, StorageError> {
    let id: Option<String> = conn
        .query_row(
            "SELECT id FROM messages WHERE conversation_type = ?1 AND conversation_id = ?2 AND sender_id = ?3 AND timestamp_quote_key = ?4 LIMIT 1",
            params![
                kind.as_str(),
                conversation.to_string(),
                author.to_string(),
                parent.to_string()
            ],
            |row| row.get(0),
        )
        .optional()?;
    Ok(id.and_then(|id| id.parse().ok()))
}

/// SELECT column list for unified messages — must match `row_to_message` column indices.
/// How long a server DM receive tombstone is kept after the deletion. A
/// second copy of one message comes from the sender's DecryptionError resend
/// (`dm_resend::RESEND_MAX_AGE_NS`, 14 days after sending), its sealed outbox
/// replay (hours) or a retry; past this, an old message could come back.
const DM_RECEIVE_TOMBSTONE_SECONDS: i64 = 30 * 24 * 60 * 60;
const CURRENT_USER_KEY: &str = crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID;

/// Before `id` is hard-deleted: if it is a server DM, remember its
/// (conversation, sender, sent_at) for the signed-in account, which is what a
/// later copy of it shares (its row id is random per receipt). Every local
/// deletion goes through here: delete-for-me and its sync, expiry, deleting
/// the chat, and a delete-for-everyone. Prunes past the retention bound.
fn tombstone_dm_receipt(conn: &rusqlite::Connection, id: &MessageId) -> Result<(), StorageError> {
    let now = Utc::now().timestamp();
    conn.execute(
        "INSERT OR IGNORE INTO dm_receive_tombstones (owner_id, conversation_id, sender_id, sent_at, deleted_at)
         SELECT o.owner, m.conversation_id, m.sender_id, m.sent_at, ?3
         FROM messages m, (SELECT json_extract(value, '$') AS owner FROM settings WHERE key = ?2) o
         WHERE m.id = ?1 AND m.conversation_type = 'server_dm' AND m.sender_id IS NOT NULL AND o.owner IS NOT NULL",
        params![id.to_string(), CURRENT_USER_KEY, now],
    )?;
    conn.execute(
        "DELETE FROM dm_receive_tombstones WHERE deleted_at < ?1",
        params![now - DM_RECEIVE_TOMBSTONE_SECONDS],
    )?;
    Ok(())
}

const MSG_COLS: &str = "id, conversation_id, conversation_type, sender_id, device_id, \
     content_json, encrypted_content, \
     thread_id, reply_to, attachments_blob, \
     is_pinned, is_edited, is_deleted, is_read, \
     sent_at, created_at, updated_at, edited_at, \
     reaction_counts_blob, current_user_reactions_blob, \
     mentioned_integrations_blob, targets_ai_bot, delivery_status, \
     reply_to_author_id, reply_to_text, \
     reply_to_thumbnail, reply_to_thumbnail_mime, failure_reason";

fn message_columns(alias: &str) -> String {
    format!("{alias}.{}", MSG_COLS.replace(", ", &format!(", {alias}.")))
}

/// Parse an RFC3339 datetime, falling back to now.
fn parse_datetime(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

/// Content type → integer discriminant.
fn content_type_discriminant(content: &MessageContent) -> i32 {
    match content {
        MessageContent::Text(_) => 0,
        MessageContent::Encrypted(_) => 1,
        MessageContent::SealedSender(_) => 2,
        MessageContent::System(_) => 3,
        MessageContent::Call(_) => 4,
        MessageContent::AI(_) => 5,
        MessageContent::Mixed(_) => 6,
    }
}

/// Map a row to Message.
///
/// Column order: id(0), conversation_id(1), conversation_type(2), sender_id(3),
/// device_id(4), content_json(5), encrypted_content(6), thread_id(7), reply_to(8),
/// attachments_blob(9), is_pinned(10), is_edited(11), is_deleted(12), is_read(13),
/// sent_at(14), created_at(15), updated_at(16), edited_at(17),
/// reaction_counts_blob(18), current_user_reactions_blob(19),
/// mentioned_integrations_blob(20), targets_ai_bot(21), delivery_status(22),
/// reply_to_author_id(23), reply_to_text(24)
fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Message, StorageError>> {
    let content_json: String = row.get(5)?;
    let attachments_blob: Vec<u8> = row.get(9)?;
    let reaction_counts_blob: Vec<u8> = row.get(18)?;
    let current_user_reactions_blob: Vec<u8> = row.get(19)?;
    let mentioned_integrations_blob: Vec<u8> = row.get(20)?;

    let conversation_type_str: String = row.get(2)?;
    let sender_id: Option<String> = row.get(3)?;
    let device_id: Option<i64> = row.get(4)?;
    let encrypted_content: Option<Vec<u8>> = row.get(6)?;
    let thread_id: Option<String> = row.get(7)?;
    let reply_to: Option<String> = row.get(8)?;
    let edited_at: Option<String> = row.get(17)?;

    let content: MessageContent = match serde_json::from_str(&content_json) {
        Ok(c) => c,
        Err(e) => {
            return Ok(Err(StorageError::InvalidData(format!(
                "Invalid content JSON: {}",
                e
            ))));
        }
    };

    let attachments: Vec<AttachmentId> = match db_deserialize(&attachments_blob) {
        Ok(a) => a,
        Err(e) => return Ok(Err(e)),
    };
    let reaction_counts: Vec<ReactionCount> = match db_deserialize(&reaction_counts_blob) {
        Ok(r) => r,
        Err(e) => return Ok(Err(e)),
    };
    let current_user_reactions: Vec<String> = match db_deserialize(&current_user_reactions_blob) {
        Ok(r) => r,
        Err(e) => return Ok(Err(e)),
    };
    let mentioned_integrations: Vec<IntegrationId> =
        match db_deserialize(&mentioned_integrations_blob) {
            Ok(m) => m,
            Err(e) => return Ok(Err(e)),
        };

    Ok(Ok(Message {
        id: row
            .get::<_, String>(0)?
            .parse()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        conversation_id: row
            .get::<_, String>(1)?
            .parse()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        conversation_type: match ConversationType::from_str_value(&conversation_type_str) {
            Ok(ct) => ct,
            Err(e) => return Ok(Err(e)),
        },
        sender_id: sender_id
            .map(|s| s.parse())
            .transpose()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        device_id: device_id.map(|d| d as u32),
        content,
        encrypted_content,
        thread_id: thread_id
            .map(|s| s.parse())
            .transpose()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        reply_to: reply_to
            .map(|s| s.parse())
            .transpose()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        reply_to_author_id: row
            .get::<_, Option<String>>(23)?
            .and_then(|s| s.parse().ok()),
        reply_to_text: row.get::<_, Option<String>>(24)?,
        reply_to_thumbnail: row.get::<_, Option<Vec<u8>>>(25)?,
        reply_to_thumbnail_mime: row.get::<_, Option<String>>(26)?,
        attachments,
        is_pinned: row.get::<_, i32>(10)? != 0,
        is_edited: row.get::<_, i32>(11)? != 0,
        is_deleted: row.get::<_, i32>(12)? != 0,
        is_read: row.get::<_, i32>(13)? != 0,
        sent_at: row.get::<_, i64>(14)? as u64,
        created_at: parse_datetime(&row.get::<_, String>(15)?),
        updated_at: parse_datetime(&row.get::<_, String>(16)?),
        edited_at: edited_at.map(|s| parse_datetime(&s)),
        reaction_counts,
        current_user_reactions,
        mentioned_integrations,
        targets_ai_bot: row.get::<_, i32>(21)? != 0,
        delivery_status: row.get::<_, String>(22).unwrap_or_default(),
        failure_reason: row.get::<_, Option<String>>(27)?,
    }))
}

/// Map a unified_reactions row to Reaction.
/// Expected columns: id, sender_id, sent_at_ns, user_id, emoji, created_at
fn row_to_reaction(row: &rusqlite::Row<'_>) -> Result<Reaction, StorageError> {
    let id_str: String = row.get(0).map_err(StorageError::Database)?;
    let sender_id_str: String = row.get(1).map_err(StorageError::Database)?;
    let sent_at_ns: i64 = row.get(2).map_err(StorageError::Database)?;
    let user_id_str: String = row.get(3).map_err(StorageError::Database)?;
    let emoji_str: String = row.get(4).map_err(StorageError::Database)?;
    let created_at_str: String = row.get(5).map_err(StorageError::Database)?;

    Ok(Reaction {
        id: id_str
            .parse()
            .map_err(|e| StorageError::InvalidData(format!("Invalid reaction ID: {}", e)))?,
        sender_id: sender_id_str
            .parse()
            .map_err(|e| StorageError::InvalidData(format!("Invalid sender ID: {}", e)))?,
        sent_at_ns: sent_at_ns as u64,
        user_id: user_id_str
            .parse()
            .map_err(|e| StorageError::InvalidData(format!("Invalid user ID: {}", e)))?,
        emoji: Emoji::unicode(&emoji_str),
        created_at: parse_datetime(&created_at_str),
    })
}

/// Map a row to Thread.
fn row_to_thread(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Thread, StorageError>> {
    let participant_ids_blob: Vec<u8> = row.get(4)?;
    let last_reply_at: Option<String> = row.get(3)?;

    let participant_ids: Vec<UserId> = match db_deserialize(&participant_ids_blob) {
        Ok(p) => p,
        Err(e) => return Ok(Err(e)),
    };

    Ok(Ok(Thread {
        root_message_id: row
            .get::<_, String>(0)?
            .parse()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        conversation_id: row
            .get::<_, String>(1)?
            .parse()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        reply_count: row.get(2)?,
        last_reply_at: last_reply_at.map(|s| parse_datetime(&s)),
        participant_ids,
    }))
}

/// Collect messages from a Rows iterator.
fn collect_messages(rows: &mut rusqlite::Rows<'_>) -> Result<Vec<Message>, StorageError> {
    super::repo_utils::collect_decoded_rows(rows, "message", |row| row_to_message(row)?)
}
