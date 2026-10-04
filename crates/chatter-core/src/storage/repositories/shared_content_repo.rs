//! Shared-content index repository.
//!
//! Backs the profile screen's Media / Files / Links / Voice / GIFs / Music
//! tabs. `shared_content` (migration v064) is a per-conversation index of
//! every shareable item a message carried, so a tab is one range scan instead
//! of deserializing every message's bincode attachment list.
//!
//! Two write points, because the two producers disagree on order:
//!
//! - [`index_message`] runs inside `UnifiedMessageRepository`'s inserts. It
//!   knows the conversation and `sent_at`, and looks each attachment's
//!   category up in `attachments` — writing `'unsorted'` when that row has not
//!   landed yet (the receive path inserts the message first).
//! - [`resolve_attachment`] runs inside `AttachmentRepository`'s inserts and
//!   fixes up any `'unsorted'` row naming the attachment just written.
//!
//! Both are `pub(crate)` free functions taking a `&Connection` so they run on
//! the caller's already-open write connection rather than taking the write
//! lock twice.

use crate::domain::attachment::{extract_links, FileMetadata, SharedCategory};
use crate::domain::ids::AttachmentId;
use crate::domain::message::{ConversationType, Message};
use crate::error::StorageError;
use crate::storage::serialization::db_deserialize;
use crate::storage::Database;
use crate::ui::chat_data::extract_content_text;
use rusqlite::{params, Connection};
use std::sync::Arc;

/// One row of a shared-content tab, straight off the index.
///
/// Shaping into display strings happens in `crate::ui::shared_content_data`;
/// this is the storage-level record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedContentRow {
    /// Message the item arrived in — the UI jumps to it when a tile is tapped.
    pub message_id: String,
    /// Attachment id, or empty for a link.
    pub attachment_id: String,
    /// `SharedCategory::as_str`.
    pub category: String,
    /// Unix nanoseconds, same clock as `messages.sent_at`.
    pub sent_at: u64,
    /// URL for a link, file name otherwise.
    pub title: String,
    /// File size in bytes; 0 for links.
    pub size_bytes: u64,
    /// Decrypted thumbnail JPEG, when the attachment has one. This is what
    /// the media grid paints — no CDN fetch, no decryption on the UI thread.
    pub thumbnail: Option<Vec<u8>>,
}

/// Read side of the shared-content index.
pub struct SharedContentRepository {
    db: Arc<Database>,
}

impl SharedContentRepository {
    /// Create a new `SharedContentRepository`.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// One page of a conversation's tab, newest first.
    ///
    /// `offset` pages backwards through the same ordering. Keyset paging
    /// (`sent_at < cursor`) would be sturdier under concurrent inserts, but
    /// this index only ever grows at the newest end, and the screen reloads
    /// from zero when it does.
    pub fn list(
        &self,
        conversation_type: &ConversationType,
        conversation_id: &str,
        category: SharedCategory,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<SharedContentRow>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT s.message_id, s.attachment_id, s.category, s.sent_at,
                    s.title, s.size_bytes, a.thumbnail_bytes
             FROM shared_content s
             LEFT JOIN attachments a ON a.id = s.attachment_id
             WHERE s.conversation_type = ?1 AND s.conversation_id = ?2
               AND s.category = ?3
             ORDER BY s.sent_at DESC, s.rowid DESC
             LIMIT ?4 OFFSET ?5",
        )?;
        let rows = stmt.query_map(
            params![
                conversation_type.as_str(),
                conversation_id,
                category.as_str(),
                limit,
                offset,
            ],
            |row| {
                Ok(SharedContentRow {
                    message_id: row.get(0)?,
                    attachment_id: row.get(1)?,
                    category: row.get(2)?,
                    sent_at: row.get::<_, i64>(3)? as u64,
                    title: row.get(4)?,
                    size_bytes: row.get::<_, i64>(5)? as u64,
                    thumbnail: row.get(6)?,
                })
            },
        )?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::Database)
    }

    /// One-time backfill for a conversation whose messages predate the index.
    ///
    /// `shared_content` is only written on insert, so history that was already
    /// on disk when v064 landed has no rows. Rather than a startup pass over
    /// every message in the database, each conversation pays for itself the
    /// first time its profile is opened: O(messages in ONE conversation),
    /// once ever, on a blocking worker.
    ///
    /// Idempotent — the index's UNIQUE constraint makes re-running a no-op —
    /// but the caller still gates it on a `settings` marker rather than on
    /// "the index has no rows for this conversation": a conversation that
    /// genuinely has nothing to share would otherwise be rescanned on every
    /// open, forever.
    ///
    /// O(messages) once per conversation. If a conversation ever
    /// gets big enough for that pause to show, move it to a background pass
    /// over all conversations at first launch after upgrade.
    pub fn backfill(
        &self,
        messages: &super::UnifiedMessageRepository,
        conversation_type: &ConversationType,
        conversation_id: &crate::domain::ids::ChannelId,
    ) -> Result<usize, StorageError> {
        const BATCH: u32 = 500;
        let mut cursor: Option<u64> = None;
        let mut indexed = 0usize;
        loop {
            let batch = messages.list(conversation_id, conversation_type, cursor, BATCH)?;
            if batch.is_empty() {
                break;
            }
            cursor = batch.last().map(|m| m.sent_at);
            {
                // Scoped so the write lock is released between batches.
                let conn = self.db.connection()?;
                for message in &batch {
                    index_message(&conn, message)?;
                }
            }
            indexed += batch.len();
            if (batch.len() as u32) < BATCH {
                break;
            }
        }
        Ok(indexed)
    }

    /// Item count per category for one conversation, so the tab strip can hide
    /// tabs that would open onto an empty list.
    ///
    /// Returns `(category, count)` pairs; categories with no items are absent.
    pub fn counts(
        &self,
        conversation_type: &ConversationType,
        conversation_id: &str,
    ) -> Result<Vec<(String, u32)>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT category, COUNT(*) FROM shared_content
             WHERE conversation_type = ?1 AND conversation_id = ?2
               AND category != 'unsorted'
             GROUP BY category",
        )?;
        let rows = stmt.query_map(
            params![conversation_type.as_str(), conversation_id],
            |row| Ok((row.get(0)?, row.get::<_, i64>(1)? as u32)),
        )?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::Database)
    }
}

// ============================================================================
// Write side — called from the two repositories that own the source rows
// ============================================================================

/// Index everything shareable in a message: its attachments and any URLs in
/// its text.
///
/// Best-effort by design. A failure here loses a row from a media drawer; it
/// must never fail the message insert that the conversation depends on, so
/// callers log and continue.
pub(crate) fn index_message(conn: &Connection, message: &Message) -> Result<(), StorageError> {
    let conv_type = message.conversation_type.as_str();
    let conv_id = message.conversation_id.to_string();
    let sent_at = message.sent_at as i64;
    let msg_id = message.id.to_string();

    for att_id in &message.attachments {
        // `attachments` may not have this row yet on the receive path, where
        // `create_from_pointers` runs after the message insert. Park it as
        // 'unsorted'; `resolve_attachment` picks it up.
        let (category, title, size) = match load_metadata(conn, att_id)? {
            Some(meta) => (
                SharedCategory::of(&meta),
                meta.name.clone(),
                meta.size as i64,
            ),
            None => (SharedCategory::Unsorted, String::new(), 0),
        };
        insert_row(
            conn,
            &msg_id,
            &att_id.to_string(),
            conv_type,
            &conv_id,
            category,
            sent_at,
            &title,
            size,
        )?;
    }

    for url in extract_links(&extract_content_text(&message.content)) {
        insert_row(
            conn,
            &msg_id,
            "",
            conv_type,
            &conv_id,
            SharedCategory::Link,
            sent_at,
            &url,
            0,
        )?;
    }

    Ok(())
}

/// Fill in any `'unsorted'` row naming this attachment, now that its metadata
/// exists. No-op when the message side has not indexed yet — that order is the
/// send path, where the metadata was already available.
pub(crate) fn resolve_attachment(
    conn: &Connection,
    id: &AttachmentId,
    meta: &FileMetadata,
) -> Result<(), StorageError> {
    conn.execute(
        "UPDATE shared_content
         SET category = ?2, title = ?3, size_bytes = ?4
         WHERE attachment_id = ?1 AND category = 'unsorted'",
        params![
            id.to_string(),
            SharedCategory::of(meta).as_str(),
            meta.name,
            meta.size as i64,
        ],
    )?;
    Ok(())
}

/// Read one attachment's `FileMetadata`, or `None` when the row is absent.
fn load_metadata(
    conn: &Connection,
    id: &AttachmentId,
) -> Result<Option<FileMetadata>, StorageError> {
    let mut stmt = conn.prepare("SELECT metadata_blob FROM attachments WHERE id = ?1")?;
    let blob: Option<Vec<u8>> = match stmt.query_row(params![id.to_string()], |row| row.get(0)) {
        Ok(b) => Some(b),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => return Err(StorageError::Database(e)),
    };
    // A corrupt blob is not worth failing a message insert over — the item
    // just stays out of the drawer.
    Ok(blob.and_then(|b| db_deserialize::<FileMetadata>(&b).ok()))
}

#[allow(clippy::too_many_arguments)]
fn insert_row(
    conn: &Connection,
    message_id: &str,
    attachment_id: &str,
    conversation_type: &str,
    conversation_id: &str,
    category: SharedCategory,
    sent_at: i64,
    title: &str,
    size_bytes: i64,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT OR IGNORE INTO shared_content (
            message_id, attachment_id, conversation_type, conversation_id,
            category, sent_at, title, size_bytes
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            message_id,
            attachment_id,
            conversation_type,
            conversation_id,
            category.as_str(),
            sent_at,
            title,
            size_bytes,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::attachment::{Attachment, AttachmentStatus, ContentLocation};
    use crate::domain::ids::{ChannelId, MessageId, UserId};
    use crate::domain::message::{MessageContent, MixedContent};
    use crate::storage::repositories::{AttachmentRepository, UnifiedMessageRepository};

    fn setup_db() -> Arc<Database> {
        // Same tempdir+SQLCipher shape as the call-history repo tests: the
        // `Database` handle assumes a real path.
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("shared_content_test.db");
        let db = Arc::new(Database::open(&db_path, "testpass").unwrap());
        db.run_migrations().unwrap();
        crate::test_tempdir::hold(temp_dir);
        db
    }

    fn attachment(name: &str, mime: &str, waveform: bool) -> Attachment {
        let mut meta = FileMetadata::new(name.to_string(), mime.to_string(), 4096);
        if waveform {
            meta.waveform_samples = Some(vec![1; 32]);
        }
        Attachment {
            id: AttachmentId::new(),
            metadata: meta,
            status: AttachmentStatus::Ready,
            content_location: ContentLocation::NotDownloaded,
            local_path: None,
            remote_url: None,
            encryption_key: None,
            encryption_iv: None,
            created_at: chrono::Utc::now(),
        }
    }

    fn message(conv: &ChannelId, sent_at: u64, content: MessageContent, atts: Vec<AttachmentId>) -> Message {
        let mut m = Message::new_text(
            *conv,
            ConversationType::ServerDm,
            Some(UserId::new()),
            String::new(),
            sent_at,
        );
        m.id = MessageId::new();
        m.content = content;
        m.attachments = atts;
        m
    }

    /// The categorisation table itself — the thing the tabs are built on.
    #[test]
    fn categorises_by_mime_waveform_and_extension() {
        let cases: &[(&str, &str, bool, SharedCategory)] = &[
            ("cat.jpg", "image/jpeg", false, SharedCategory::Media),
            ("clip.mp4", "video/mp4", false, SharedCategory::Media),
            ("wave.gif", "image/gif", false, SharedCategory::Gif),
            // GIF wins over Media even when the MIME lies.
            ("wave.gif", "application/octet-stream", false, SharedCategory::Gif),
            ("note.ogg", "audio/ogg", true, SharedCategory::Voice),
            ("song.mp3", "audio/mpeg", false, SharedCategory::Music),
            ("spec.pdf", "application/pdf", false, SharedCategory::File),
            ("main.rs", "text/plain", false, SharedCategory::File),
        ];
        for (name, mime, waveform, expected) in cases {
            let att = attachment(name, mime, *waveform);
            assert_eq!(
                SharedCategory::of(&att.metadata),
                *expected,
                "{name} ({mime}, waveform={waveform})"
            );
        }
    }

    #[test]
    fn extracts_only_tappable_urls() {
        assert_eq!(
            extract_links("see https://example.com/a. and http://b.dev, not ftp://c or bare.com"),
            vec!["https://example.com/a".to_string(), "http://b.dev".to_string()],
        );
        assert!(extract_links("no links here").is_empty());
    }

    /// The query the whole feature exists for: photos in one conversation,
    /// newest first, paged — and NOT the other conversation's photos, nor the
    /// other categories'.
    #[test]
    fn lists_one_categorys_items_newest_first_scoped_to_conversation() {
        let db = setup_db();
        let atts = AttachmentRepository::new(db.clone());
        let msgs = UnifiedMessageRepository::new(db.clone());
        let shared = SharedContentRepository::new(db.clone());

        let conv = ChannelId::new();
        let other_conv = ChannelId::new();

        // Send order: attachment row first, then the message (the send path).
        let mut ids = Vec::new();
        for (i, (name, mime)) in [
            ("a.jpg", "image/jpeg"),
            ("b.png", "image/png"),
            ("c.mp4", "video/mp4"),
        ]
        .iter()
        .enumerate()
        {
            let att = attachment(name, mime, false);
            atts.insert(&att).unwrap();
            ids.push(att.id);
            msgs.insert(&message(
                &conv,
                100 + i as u64,
                MessageContent::Mixed(MixedContent::attachments_only(vec![att.id])),
                vec![att.id],
            ))
            .unwrap();
        }

        // A doc in the same conversation, and a photo in a different one.
        let doc = attachment("spec.pdf", "application/pdf", false);
        atts.insert(&doc).unwrap();
        msgs.insert(&message(
            &conv,
            200,
            MessageContent::Mixed(MixedContent::attachments_only(vec![doc.id])),
            vec![doc.id],
        ))
        .unwrap();

        let elsewhere = attachment("z.jpg", "image/jpeg", false);
        atts.insert(&elsewhere).unwrap();
        msgs.insert(&message(
            &other_conv,
            300,
            MessageContent::Mixed(MixedContent::attachments_only(vec![elsewhere.id])),
            vec![elsewhere.id],
        ))
        .unwrap();

        let page = shared
            .list(&ConversationType::ServerDm, &conv.to_string(), SharedCategory::Media, 10, 0)
            .unwrap();
        assert_eq!(page.len(), 3, "only this conversation's media");
        assert_eq!(page[0].title, "c.mp4", "newest first");
        assert_eq!(page[2].title, "a.jpg");

        // Paging.
        let page2 = shared
            .list(&ConversationType::ServerDm, &conv.to_string(), SharedCategory::Media, 2, 2)
            .unwrap();
        assert_eq!(page2.len(), 1);
        assert_eq!(page2[0].title, "a.jpg");

        // Files tab sees the doc and nothing else.
        let files = shared
            .list(&ConversationType::ServerDm, &conv.to_string(), SharedCategory::File, 10, 0)
            .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].title, "spec.pdf");
        assert_eq!(files[0].size_bytes, 4096);

        let counts = shared.counts(&ConversationType::ServerDm, &conv.to_string()).unwrap();
        assert_eq!(counts.iter().find(|(c, _)| c == "media").unwrap().1, 3);
        assert_eq!(counts.iter().find(|(c, _)| c == "file").unwrap().1, 1);
    }

    /// The receive path indexes the message BEFORE the attachment row exists.
    /// The item must still land in the right tab once it does.
    #[test]
    fn resolves_unsorted_when_attachment_lands_after_the_message() {
        let db = setup_db();
        let atts = AttachmentRepository::new(db.clone());
        let msgs = UnifiedMessageRepository::new(db.clone());
        let shared = SharedContentRepository::new(db.clone());
        let conv = ChannelId::new();

        let att = attachment("late.jpg", "image/jpeg", false);
        msgs.insert(&message(
            &conv,
            100,
            MessageContent::Mixed(MixedContent::attachments_only(vec![att.id])),
            vec![att.id],
        ))
        .unwrap();

        assert!(
            shared
                .list(&ConversationType::ServerDm, &conv.to_string(), SharedCategory::Media, 10, 0)
                .unwrap()
                .is_empty(),
            "unresolved item must not render"
        );

        atts.insert(&att).unwrap();

        let page = shared
            .list(&ConversationType::ServerDm, &conv.to_string(), SharedCategory::Media, 10, 0)
            .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].title, "late.jpg");
    }

    /// History that predates the index has no rows until the one-time
    /// conversation backfill runs.
    #[test]
    fn backfill_indexes_pre_existing_history() {
        let db = setup_db();
        let atts = AttachmentRepository::new(db.clone());
        let msgs = UnifiedMessageRepository::new(db.clone());
        let shared = SharedContentRepository::new(db.clone());
        let conv = ChannelId::new();

        let att = attachment("old.jpg", "image/jpeg", false);
        atts.insert(&att).unwrap();
        msgs.insert(&message(
            &conv,
            100,
            MessageContent::Mixed(MixedContent::attachments_only(vec![att.id])),
            vec![att.id],
        ))
        .unwrap();

        // Simulate a device that had this history before v064.
        db.connection()
            .unwrap()
            .execute("DELETE FROM shared_content", [])
            .unwrap();
        assert!(shared
            .list(&ConversationType::ServerDm, &conv.to_string(), SharedCategory::Media, 10, 0)
            .unwrap()
            .is_empty());

        assert_eq!(
            shared.backfill(&msgs, &ConversationType::ServerDm, &conv).unwrap(),
            1
        );
        let page = shared
            .list(&ConversationType::ServerDm, &conv.to_string(), SharedCategory::Media, 10, 0)
            .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].title, "old.jpg");

        // Idempotent — running it twice must not duplicate tiles.
        shared.backfill(&msgs, &ConversationType::ServerDm, &conv).unwrap();
        assert_eq!(
            shared
                .list(&ConversationType::ServerDm, &conv.to_string(), SharedCategory::Media, 10, 0)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn indexes_urls_from_message_text() {
        let db = setup_db();
        let msgs = UnifiedMessageRepository::new(db.clone());
        let shared = SharedContentRepository::new(db.clone());
        let conv = ChannelId::new();

        msgs.insert(&message(
            &conv,
            100,
            MessageContent::Text("read https://rust-lang.org today".into()),
            vec![],
        ))
        .unwrap();
        msgs.insert(&message(&conv, 101, MessageContent::Text("no link".into()), vec![]))
            .unwrap();

        let links = shared
            .list(&ConversationType::ServerDm, &conv.to_string(), SharedCategory::Link, 10, 0)
            .unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].title, "https://rust-lang.org");
    }
}
