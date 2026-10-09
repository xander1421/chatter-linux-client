//! Saved messages (Note to Self) operations.
//!
//! Follows Signal's approach:
//! - Store locally on the sending device
//! - If multi-device: send SavedMessageSync to other devices via Signal Protocol
//! - No application-level encryption (Signal Protocol device sessions provide it)
//! - No profile key usage (profile key is shared with friends — wrong threat model)
//!
//! Every sync is a full snapshot of one Saved record ([`saved_snapshot`]):
//! creation, edit and pin send the same shape, and a sibling device upserts it
//! ([`MessageService::apply_saved_message_sync`]). Each mutable part carries
//! its own clock — `edited_at` for the body, `pin_changed_at` for the pin —
//! so a replayed or reordered snapshot is idempotent and never rolls a newer
//! change back.

use crate::domain::attachment::Attachment;
use crate::domain::ids::{AttachmentId, ChannelId, MessageId, UserId};
use crate::domain::message::{ConversationType, Message, MessageContent, MixedContent};
use crate::domain::reaction::ReactionScope;
use crate::network::grpc::message_client::MessageClient;
use crate::proto::content_v1::{AttachmentPointer, Quote, SavedMessageSync, quote};
use crate::services::attachment::AttachmentService;
use crate::services::current_user::CurrentUserProvider;
use crate::services::gateway::{QuoteIntent, build_saved_message_sync_content};
use crate::storage::repositories::{PinWrite, SavedSyncRepository};
use crate::util::time::TimestampNanos;

use super::core::proto_to_domain_message_with_sender;
use super::{MessageError, MessageService};

/// What applying one snapshot changed here, for the events the inbox emits.
#[derive(Clone, Debug, PartialEq)]
pub enum SavedSyncOutcome {
    /// The record did not exist: stored whole, as the snapshot describes it.
    Created(Message),
    /// The record existed: the parts whose clock was newer were applied.
    Updated {
        message: Message,
        /// The body was replaced (the stored row is the truth).
        body_changed: bool,
        /// The pin state changed, to this value.
        pin_changed: Option<bool>,
    },
    /// Nothing to do: every part was already applied, or the record was
    /// deleted here (a tombstone or a deleted row is never resurrected).
    Unchanged,
}

fn nanos(at: Option<chrono::DateTime<chrono::Utc>>) -> u64 {
    at.and_then(|t| t.timestamp_nanos_opt())
        .map(|n| n.max(0) as u64)
        .unwrap_or_default()
}

/// The pointer a sibling device needs to fetch and show one of our stored
/// attachments: the reverse of `AttachmentService::create_from_pointers`.
/// `None` for a record with no CDN key or key material (never uploaded):
/// nothing a sibling could fetch.
pub(crate) fn attachment_pointer_from_record(att: &Attachment) -> Option<AttachmentPointer> {
    let cdn_key = att.remote_url.clone().filter(|k| !k.is_empty())?;
    let encryption_key = att.encryption_key.clone().filter(|k| !k.is_empty())?;
    let m = &att.metadata;
    Some(AttachmentPointer {
        cdn_key,
        encryption_key,
        digest: att.encryption_iv.clone().unwrap_or_default(),
        plaintext_size: m.size,
        content_type: m.mime_type.clone(),
        file_name: m.name.clone(),
        thumbnail_cdn_key: m.thumbnail_cdn_key.clone().unwrap_or_default(),
        thumbnail_key: m.thumbnail_key.clone().unwrap_or_default(),
        thumbnail_digest: m.thumbnail_digest.clone().unwrap_or_default(),
        thumbnail_plaintext_size: m.thumbnail_plaintext_size.unwrap_or_default(),
        thumbnail_width: m.thumbnail_width.unwrap_or_default(),
        thumbnail_height: m.thumbnail_height.unwrap_or_default(),
        width: m.width.unwrap_or_default(),
        height: m.height.unwrap_or_default(),
        duration_ms: m
            .duration_seconds
            .map(|s| (s * 1000.0).round().max(0.0) as u64)
            .unwrap_or_default(),
        waveform: m.waveform_samples.clone().unwrap_or_default(),
        ..Default::default()
    })
}

/// Whether a snapshot's body supersedes the stored row's: a newer edit
/// clock with a body to write. Equal clocks are the same edit replayed.
pub(crate) fn body_supersedes(stored_edited_at: u64, snapshot: &SavedMessageSync) -> bool {
    snapshot.edited_at > stored_edited_at && !snapshot.text.trim().is_empty()
}

/// The stored body after a synced edit: text replaced, a `Mixed` body keeps
/// its attachment ids (the record's attachments never change).
pub(crate) fn edited_body(stored: &MessageContent, text: &str) -> MessageContent {
    match stored {
        MessageContent::Mixed(mixed) => MessageContent::Mixed(MixedContent {
            text: Some(text.to_string()),
            attachment_ids: mixed.attachment_ids.clone(),
        }),
        _ => MessageContent::Text(text.to_string()),
    }
}

/// Reject a snapshot whose clocks or identity cannot be right, before any
/// lookup: a receiver must not store what it cannot order.
pub(crate) fn validate_snapshot(sync: &SavedMessageSync) -> Result<MessageId, MessageError> {
    let id: MessageId = sync.message_id.parse().map_err(|_| {
        MessageError::InvalidContent(format!("invalid message ID: {}", sync.message_id))
    })?;
    if sync.sent_at == 0 {
        return Err(MessageError::InvalidContent(
            "SavedMessageSync without sent_at".into(),
        ));
    }
    if sync.edited_at != 0 && sync.edited_at < sync.sent_at {
        return Err(MessageError::InvalidContent(
            "SavedMessageSync edited before it was sent".into(),
        ));
    }
    if sync.pin_changed_at != 0 && sync.pin_changed_at < sync.sent_at {
        return Err(MessageError::InvalidContent(
            "SavedMessageSync pinned before it was sent".into(),
        ));
    }
    if sync.pin_changed_at == 0 && sync.pinned {
        return Err(MessageError::InvalidContent(
            "SavedMessageSync pinned without a pin clock".into(),
        ));
    }
    if sync.text.trim().is_empty() && sync.attachments.is_empty() && sync.attachment_ids.is_empty()
    {
        return Err(MessageError::InvalidContent(
            "SavedMessageSync must have text or attachments".into(),
        ));
    }
    if let Some(q) = &sync.quote {
        if q.id == 0 {
            return Err(MessageError::InvalidContent(
                "SavedMessageSync quote without a target clock".into(),
            ));
        }
    }
    Ok(id)
}

impl MessageService {
    /// Send a message to the SavedMessages channel (Note to Self).
    ///
    /// Follows Signal Desktop's approach:
    /// - Always store locally first, complete: a reply's quote fields are on
    ///   the row before its insert, since this local row is the one every
    ///   reload reads and nothing patches it afterwards
    /// - Snapshot the stored row into a SavedMessageSync Content proto and
    ///   send it to our other devices like any sync transcript
    ///   (`sync_to_own_devices`)
    ///
    /// A `quote` is resolved only inside this user's Saved conversation
    /// (`ConversationType::SavedMessages` under their own id): a channel or
    /// DM row sharing the quoted (sender, sent_at) pair is never the parent,
    /// and the preview comes from that parent, never from the caller's
    /// `target_preview`, which may have been looked up without a scope.
    /// Storage errors, lookup and insert alike, are returned: no row is
    /// written without its quote. A parent that is absent or deleted in
    /// that scope rejects the send as `InvalidContent` instead of storing
    /// a timestamp-derived `reply_to`: that id is an ordinary UUID another
    /// conversation's row can carry, which reload would then follow.
    pub async fn save_message(
        &self,
        text: &str,
        attachment_ids: Vec<AttachmentId>,
        quote: Option<&QuoteIntent>,
    ) -> Result<Message, MessageError> {
        let sender_id = self.settings_repo.current_user_id()?;

        // Validate content
        let trimmed_text = text.trim();
        if trimmed_text.is_empty() && attachment_ids.is_empty() {
            return Err(MessageError::InvalidContent(
                "message must have text or attachments".to_string(),
            ));
        }

        // Create and store the message locally
        let channel_id = ChannelId::from(sender_id); // SavedMessages channel_id = user_id
        let sent_at = TimestampNanos::now().as_u64();
        let mut message = Message::new_text(
            channel_id,
            ConversationType::SavedMessages,
            Some(sender_id),
            trimmed_text.to_string(),
            sent_at,
        );
        message.attachments = attachment_ids.clone();
        if let Some(quote) = quote {
            let scope = ReactionScope::new(channel_id, ConversationType::SavedMessages);
            let original = self
                .message_repo
                .get_by_reaction_scope(&scope, &quote.target_author_id, quote.target_timestamp)?
                .ok_or_else(|| {
                    MessageError::InvalidContent(
                        "the quoted Saved message no longer exists".to_string(),
                    )
                })?;
            let scoped = QuoteIntent {
                target_author_id: quote.target_author_id,
                target_timestamp: quote.target_timestamp,
                target_preview: None,
            };
            self.fill_reply_context(&mut message, &scoped, Some(original));
        }
        self.message_repo.insert_outgoing(&message)?;

        // Sync to other devices. Failures are logged there and non-fatal:
        // the message is stored locally.
        self.sync_saved_record(&message).await;

        Ok(message)
    }

    /// The snapshot of one stored Saved record as [`SavedMessageSync`]:
    /// its body, real id and clock, attachments as fetchable pointers (and
    /// their CDN keys, for a receiver that only knows those), its quote as
    /// the parent's clock plus the stored preview, and the edit and pin
    /// clocks a receiver orders by. `pin_changed_at` is this device's
    /// watermark for the record (`SavedSyncRepository`): 0 until its pin
    /// state ever changed.
    fn saved_snapshot(&self, message: &Message) -> SavedMessageSync {
        let mut attachments = Vec::with_capacity(message.attachments.len());
        let mut attachment_ids = Vec::with_capacity(message.attachments.len());
        for id in &message.attachments {
            match self.attachment_repo.get(id) {
                Ok(Some(att)) => match attachment_pointer_from_record(&att) {
                    Some(pointer) => {
                        attachment_ids.push(pointer.cdn_key.clone());
                        attachments.push(pointer);
                    }
                    None => tracing::warn!(
                        message_id = %message.id,
                        attachment_id = %id,
                        "SavedMessages sync: attachment has no CDN key, left out of the snapshot"
                    ),
                },
                Ok(None) => tracing::warn!(
                    message_id = %message.id,
                    attachment_id = %id,
                    "SavedMessages sync: attachment record missing, left out of the snapshot"
                ),
                Err(error) => tracing::warn!(
                    message_id = %message.id,
                    attachment_id = %id,
                    %error,
                    "SavedMessages sync: attachment read failed, left out of the snapshot"
                ),
            }
        }
        // The quoted parent is named by its clock, as every Quote is. A
        // parent deleted here since has no clock to send: the snapshot then
        // carries no quote rather than a preview pointing nowhere.
        let quote = message
            .reply_to
            .and_then(|parent| self.message_repo.get(&parent).ok().flatten())
            .map(|parent| Quote {
                id: parent.sent_at,
                text: message.reply_to_text.clone().unwrap_or_default(),
                attachments: vec![],
                author_aci: message
                    .reply_to_author_id
                    .or(parent.sender_id)
                    .map(|a| a.to_string())
                    .unwrap_or_default(),
                r#type: quote::Type::Normal as i32,
                author_aci_binary: vec![],
            });
        let pin_changed_at = SavedSyncRepository::new(self.message_repo.db())
            .pin_watermark(&message.id)
            .unwrap_or_else(|error| {
                tracing::warn!(message_id = %message.id, %error, "SavedMessages sync: pin watermark read failed; sending 0");
                0
            });
        SavedMessageSync {
            text: message.as_text().unwrap_or_default().to_string(),
            message_id: message.id.to_string(),
            sent_at: message.sent_at,
            attachment_ids,
            attachments,
            quote,
            edited_at: if message.is_edited {
                nanos(message.edited_at)
            } else {
                0
            },
            // A pinned row whose watermark was never written (pinned before
            // v092) cannot be ordered by a receiver; it is sent unpinned
            // until its pin state changes here again.
            pinned: message.is_pinned && pin_changed_at != 0,
            pin_changed_at,
        }
    }

    /// Sync one of our stored Saved records to the user's other linked
    /// devices, through the one own-device path every sync transcript
    /// takes ([`Self::sync_to_own_devices`]): padded, this device
    /// excluded, the device set checked by the server and reconciled when
    /// it changed, and in Shield only devices we already hold a session
    /// with. Best effort, once: a device that was unreachable gets no
    /// retry from here, and a Shield device without a session is skipped;
    /// returns how many devices it reached. `message` is the row as stored
    /// — the caller re-reads it after its write, so the snapshot is what
    /// this device holds, not what it meant to write.
    pub(crate) async fn sync_saved_record(&self, message: &Message) -> usize {
        let snapshot = self.saved_snapshot(message);
        let content = build_saved_message_sync_content(snapshot);
        let synced = self
            .sync_to_own_devices(&mut self.auth_client(), content)
            .await;
        tracing::debug!(message_id = %message.id, synced, "SavedMessages: own-device sync done");
        synced
    }

    /// The stored Saved record `id` if it is ours: in our Saved conversation,
    /// of the Saved kind, authored by us. Any other row under that id — a
    /// channel or DM row, a peer's message — is not it, and a sync naming
    /// it is refused rather than applied to a foreign scope.
    fn own_saved_row(&self, own: UserId, id: &MessageId) -> Result<Option<Message>, MessageError> {
        let Some(stored) = self.message_repo.get(id)? else {
            return Ok(None);
        };
        let scope = ReactionScope::new(ChannelId::from(own), ConversationType::SavedMessages);
        if stored.conversation_id != scope.id
            || stored.conversation_type != scope.kind
            || stored.sender_id != Some(own)
        {
            return Err(MessageError::InvalidContent(format!(
                "SavedMessageSync names a row outside our Saved conversation: {id}"
            )));
        }
        Ok(Some(stored))
    }

    /// Apply a Saved snapshot received from another of our devices.
    ///
    /// Called by the inbox processor, which has already checked that the
    /// sealed sender is our own account. Upsert: a record we do not hold is
    /// stored whole — body, attachments (records created from the pointers,
    /// as a received message's are), quote resolved in our Saved scope, edit
    /// and pin state — unless we deleted it (tombstone), which it stays. A
    /// record we hold takes the body only when the snapshot's `edited_at`
    /// is newer than ours, and the pin only when `pin_changed_at` is newer
    /// than the last pin change applied here, each written with its clock
    /// so the next snapshot orders against it. Identity, attachments and
    /// quote of an existing row never change. Nothing here sends: a
    /// received snapshot is applied, never re-synced, so two devices cannot
    /// echo one change back and forth.
    pub fn apply_saved_message_sync(
        &self,
        sync: &SavedMessageSync,
    ) -> Result<SavedSyncOutcome, MessageError> {
        let own = self.settings_repo.current_user_id()?;
        let id = validate_snapshot(sync)?;
        let scope = ReactionScope::new(ChannelId::from(own), ConversationType::SavedMessages);
        if let Some(q) = &sync.quote {
            if !q.author_aci.is_empty() && q.author_aci.parse::<UserId>().ok() != Some(own) {
                return Err(MessageError::InvalidContent(
                    "SavedMessageSync quote names another author".into(),
                ));
            }
        }

        if self.message_repo.is_tombstoned(&id)? {
            tracing::debug!(message_id = %id, "SavedMessages sync: record deleted here, snapshot ignored");
            return Ok(SavedSyncOutcome::Unchanged);
        }
        let pins = SavedSyncRepository::new(self.message_repo.db());

        let Some(stored) = self.own_saved_row(own, &id)? else {
            return self.store_saved_snapshot(own, id, &scope, sync, &pins);
        };
        if stored.is_deleted {
            return Ok(SavedSyncOutcome::Unchanged);
        }

        let mut body_changed = false;
        if body_supersedes(nanos(stored.edited_at), sync) {
            let content = edited_body(&stored.content, sync.text.trim());
            let edited_at = chrono::DateTime::from_timestamp_nanos(sync.edited_at as i64);
            body_changed = self.message_repo.set_body_scoped(
                &scope,
                &own,
                stored.sent_at,
                &content,
                edited_at,
            )?;
        }

        let mut pin_changed = None;
        if sync.pin_changed_at != 0 {
            match pins.apply_synced_pin(
                &scope,
                &own,
                stored.sent_at,
                &id,
                sync.pinned,
                sync.pin_changed_at,
            )? {
                PinWrite::Applied { .. } => pin_changed = Some(sync.pinned),
                PinWrite::Stale | PinWrite::NoRow => {}
            }
        }

        if !body_changed && pin_changed.is_none() {
            return Ok(SavedSyncOutcome::Unchanged);
        }
        let message = self
            .message_repo
            .get(&id)?
            .ok_or(MessageError::MessageNotFound(id))?;
        tracing::info!(
            message_id = %id,
            body_changed,
            ?pin_changed,
            "SavedMessages: applied snapshot from another device"
        );
        Ok(SavedSyncOutcome::Updated {
            message,
            body_changed,
            pin_changed,
        })
    }

    /// Store a snapshot of a record we do not hold, whole.
    fn store_saved_snapshot(
        &self,
        own: UserId,
        id: MessageId,
        scope: &ReactionScope,
        sync: &SavedMessageSync,
        pins: &SavedSyncRepository,
    ) -> Result<SavedSyncOutcome, MessageError> {
        // Attachment records from the pointers, exactly as a received
        // message's are; a CDN key without its pointer (an older sender)
        // keeps the id so the row still names it.
        let mut attachment_ids = AttachmentService::new_local(self.attachment_repo.clone())
            .create_from_pointers(&sync.attachments);
        for key in sync.attachment_ids.iter().filter(|k| !k.is_empty()) {
            let legacy = AttachmentId::from_cdn_key(key);
            if !attachment_ids.contains(&legacy) {
                attachment_ids.push(legacy);
            }
        }

        let mut message = Message::new_text(
            scope.id,
            ConversationType::SavedMessages,
            Some(own),
            sync.text.trim().to_string(),
            sync.sent_at,
        );
        message.id = id;
        message.attachments = attachment_ids;
        if sync.edited_at != 0 {
            message.is_edited = true;
            message.edited_at = Some(chrono::DateTime::from_timestamp_nanos(
                sync.edited_at as i64,
            ));
        }
        message.is_pinned = sync.pinned;

        // The quote resolves in our Saved scope only, like a local reply
        // (`save_message`). A parent we do not hold (its own snapshot not
        // here yet, or deleted) leaves the row without reply context rather
        // than with a clock-derived id another conversation's row may carry.
        if let Some(q) = &sync.quote {
            match self.message_repo.get_by_reaction_scope(scope, &own, q.id)? {
                Some(original) => {
                    let intent = QuoteIntent {
                        target_author_id: own,
                        target_timestamp: q.id,
                        target_preview: None,
                    };
                    self.fill_reply_context(&mut message, &intent, Some(original));
                }
                None => tracing::warn!(
                    message_id = %id,
                    quoted_sent_at = q.id,
                    "SavedMessages sync: quoted record not held here, stored without reply context"
                ),
            }
        }

        // Watermark first, then the row: an orphan watermark only keeps an
        // older pin from applying later; a pinned row without its watermark
        // could be unpinned by one.
        pins.record_pin_watermark(&id, sync.pin_changed_at)?;
        self.message_repo.insert(&message)?;
        let stored = self
            .message_repo
            .get(&id)?
            .ok_or(MessageError::MessageNotFound(id))?;
        tracing::info!(message_id = %id, "SavedMessages: stored snapshot from another device");
        Ok(SavedSyncOutcome::Created(stored))
    }

    /// Get messages from a channel.
    ///
    /// Fetches messages from the server and stores them locally.
    pub async fn get_messages(
        &self,
        message_client: &mut MessageClient,
        channel_id: &ChannelId,
        limit: u32,
        before: Option<&MessageId>,
    ) -> Result<Vec<Message>, MessageError> {
        use crate::proto::chat_v1::MessageSortDirection;

        // Check logged in
        let _ = self.settings_repo.current_user_id()?;

        // Fetch from server
        let response = message_client
            .get_messages(
                &channel_id.to_string(),
                limit as i32,
                None, // thread_root_id
                None, // page_token
                MessageSortDirection::SortNewestFirst,
                before.map(|id| id.to_string()).as_deref(),
                None, // reference_time
            )
            .await?;

        // Convert proto messages to domain messages and store locally
        let mut messages = Vec::with_capacity(response.messages.len());
        for proto in &response.messages {
            // Parse sender ID from proto author_id
            let sender_id: Option<UserId> = if proto.author_id.is_empty() {
                None
            } else {
                proto.author_id.parse().ok()
            };

            // Saved-messages RPC only returns messages in the current
            // user's personal vault channel — ConversationType is
            // unambiguously SavedMessages regardless of what the channel
            // row says.
            let message = proto_to_domain_message_with_sender(
                proto,
                sender_id,
                crate::domain::message::ConversationType::SavedMessages,
            )?;

            // Upsert to local storage (insert or update)
            match self.message_repo.get(&message.id)? {
                Some(_) => self.message_repo.update(&message)?,
                None => self.message_repo.insert(&message)?,
            }

            messages.push(message);
        }

        Ok(messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::attachment::{Attachment, FileMetadata};
    use crate::storage::{Database, MessageRepository};
    use std::sync::Arc;

    fn snapshot() -> SavedMessageSync {
        SavedMessageSync {
            text: "note".into(),
            message_id: MessageId::new().to_string(),
            sent_at: 1_000,
            edited_at: 0,
            pinned: false,
            pin_changed_at: 0,
            ..Default::default()
        }
    }

    #[test]
    fn snapshot_validation_rejects_what_cannot_be_ordered() {
        assert!(validate_snapshot(&snapshot()).is_ok());
        let mut bad = snapshot();
        bad.message_id = "not-a-uuid".into();
        assert!(validate_snapshot(&bad).is_err());
        let mut bad = snapshot();
        bad.sent_at = 0;
        assert!(validate_snapshot(&bad).is_err());
        let mut bad = snapshot();
        bad.edited_at = 999;
        assert!(validate_snapshot(&bad).is_err(), "edited before sent");
        let mut bad = snapshot();
        bad.pin_changed_at = 999;
        assert!(validate_snapshot(&bad).is_err(), "pinned before sent");
        let mut bad = snapshot();
        bad.pinned = true;
        assert!(validate_snapshot(&bad).is_err(), "pinned without a clock");
        let mut bad = snapshot();
        bad.text = "   ".into();
        assert!(validate_snapshot(&bad).is_err(), "no body at all");
        bad.attachment_ids = vec!["cdn".into()];
        assert!(validate_snapshot(&bad).is_ok(), "attachment-only record");
        let mut bad = snapshot();
        bad.quote = Some(Quote::default());
        assert!(validate_snapshot(&bad).is_err(), "quote without a clock");
    }

    #[test]
    fn body_applies_only_when_newer_and_keeps_mixed_attachments() {
        let mut snap = snapshot();
        snap.edited_at = 2_000;
        assert!(body_supersedes(0, &snap));
        assert!(body_supersedes(1_999, &snap));
        assert!(!body_supersedes(2_000, &snap), "same edit replayed");
        assert!(!body_supersedes(2_001, &snap), "older edit reordered");
        snap.text = " ".into();
        assert!(!body_supersedes(0, &snap), "never blank a body");

        let att = AttachmentId::new();
        let mixed = MessageContent::Mixed(MixedContent {
            text: Some("old".into()),
            attachment_ids: vec![att],
        });
        match edited_body(&mixed, "new") {
            MessageContent::Mixed(m) => {
                assert_eq!(m.text.as_deref(), Some("new"));
                assert_eq!(m.attachment_ids, vec![att]);
            }
            other => panic!("expected Mixed, got {other:?}"),
        }
        assert_eq!(
            edited_body(&MessageContent::Text("old".into()), "new"),
            MessageContent::Text("new".into())
        );
    }

    #[test]
    fn attachment_pointer_carries_what_a_sibling_needs_to_fetch() {
        let mut meta = FileMetadata::new("a.jpg".into(), "image/jpeg".into(), 1234);
        meta.width = Some(640);
        meta.height = Some(480);
        meta.thumbnail_cdn_key = Some("thumb-key".into());
        meta.thumbnail_key = Some(vec![7; 64]);
        meta.thumbnail_digest = Some(vec![8; 32]);
        meta.thumbnail_plaintext_size = Some(99);
        meta.duration_seconds = Some(1.5);
        let mut att = Attachment::new(AttachmentId::new(), meta);
        assert!(
            attachment_pointer_from_record(&att).is_none(),
            "never uploaded: nothing to fetch"
        );
        att.remote_url = Some("cdn-key".into());
        att.encryption_key = Some(vec![1; 64]);
        att.encryption_iv = Some(vec![2; 32]);
        let p = attachment_pointer_from_record(&att).unwrap();
        assert_eq!(p.cdn_key, "cdn-key");
        assert_eq!(p.encryption_key, vec![1; 64]);
        assert_eq!(p.digest, vec![2; 32]);
        assert_eq!((p.plaintext_size, p.width, p.height), (1234, 640, 480));
        assert_eq!(p.content_type, "image/jpeg");
        assert_eq!(p.file_name, "a.jpg");
        assert_eq!(p.thumbnail_cdn_key, "thumb-key");
        assert_eq!(p.thumbnail_plaintext_size, 99);
        assert_eq!(p.duration_ms, 1500);
        // The id a receiver derives from the pointer is the one our row names.
        assert_eq!(
            AttachmentId::from_cdn_key(&p.cdn_key),
            AttachmentId::from_cdn_key("cdn-key")
        );
    }

    #[test]
    fn pin_watermark_orders_reordered_pins_and_moves_with_the_row() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open(&dir.path().join("saved.db"), "test-password").unwrap());
        db.run_migrations().unwrap();
        // v092's table, whether or not the registrar has it yet.
        db.connection()
            .unwrap()
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS saved_pin_watermarks (
                    message_id TEXT PRIMARY KEY, changed_at INTEGER NOT NULL);",
            )
            .unwrap();
        let repo = MessageRepository::new(db.clone());
        let pins = SavedSyncRepository::new(db);
        let own = UserId::new();
        let scope = ReactionScope::new(ChannelId::from(own), ConversationType::SavedMessages);
        let sent_at = 5_000;
        let saved = Message::new_text(scope.id, scope.kind, Some(own), "saved".into(), sent_at);
        let twin = Message::new_text(
            scope.id,
            ConversationType::Channel,
            Some(own),
            "twin".into(),
            sent_at,
        );
        repo.insert(&saved).unwrap();
        repo.insert(&twin).unwrap();
        assert_eq!(pins.pin_watermark(&saved.id).unwrap(), 0);

        // Local pin: dated now, watermark written with the row.
        let local = pins
            .apply_local_pin(&scope, &own, sent_at, &saved.id, true, 9_000)
            .unwrap();
        assert_eq!(local, PinWrite::Applied { changed_at: 9_000 });
        assert!(repo.get(&saved.id).unwrap().unwrap().is_pinned);
        assert!(
            !repo.get(&twin.id).unwrap().unwrap().is_pinned,
            "same key, other kind"
        );
        assert_eq!(pins.pin_watermark(&saved.id).unwrap(), 9_000);

        // A sibling's unpin dated earlier cannot undo it.
        let stale = pins
            .apply_synced_pin(&scope, &own, sent_at, &saved.id, false, 8_000)
            .unwrap();
        assert_eq!(stale, PinWrite::Stale);
        assert!(repo.get(&saved.id).unwrap().unwrap().is_pinned);

        // A newer unpin applies and keeps the clock past the unpin.
        let newer = pins
            .apply_synced_pin(&scope, &own, sent_at, &saved.id, false, 10_000)
            .unwrap();
        assert_eq!(newer, PinWrite::Applied { changed_at: 10_000 });
        assert!(!repo.get(&saved.id).unwrap().unwrap().is_pinned);
        assert_eq!(pins.pin_watermark(&saved.id).unwrap(), 10_000);
        // The reordered old pin arriving now is still stale: no re-pin.
        assert_eq!(
            pins.apply_synced_pin(&scope, &own, sent_at, &saved.id, true, 9_500)
                .unwrap(),
            PinWrite::Stale
        );
        assert!(!repo.get(&saved.id).unwrap().unwrap().is_pinned);
        // The same change replayed is a no-op too.
        assert_eq!(
            pins.apply_synced_pin(&scope, &own, sent_at, &saved.id, false, 10_000)
                .unwrap(),
            PinWrite::Stale
        );

        // A local pin whose clock reads before the watermark still orders after it.
        let skewed = pins
            .apply_local_pin(&scope, &own, sent_at, &saved.id, true, 7_000)
            .unwrap();
        assert_eq!(skewed, PinWrite::Applied { changed_at: 10_001 });

        // No row at that key: nothing written, watermark untouched.
        let ghost = MessageId::new();
        assert_eq!(
            pins.apply_synced_pin(&scope, &own, sent_at, &ghost, true, 20_000)
                .unwrap(),
            PinWrite::NoRow
        );
        assert_eq!(pins.pin_watermark(&ghost).unwrap(), 0);
        // A pre-insert watermark keeps the highest clock.
        pins.record_pin_watermark(&ghost, 20_000).unwrap();
        pins.record_pin_watermark(&ghost, 15_000).unwrap();
        assert_eq!(pins.pin_watermark(&ghost).unwrap(), 20_000);
    }
}
