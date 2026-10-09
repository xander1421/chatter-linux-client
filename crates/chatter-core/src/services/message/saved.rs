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
use crate::proto::content_v1::{
    AttachmentPointer, Quote, QuotedAttachment, SavedMessageSync, quote,
};
use crate::services::attachment::AttachmentService;
use crate::services::current_user::CurrentUserProvider;
use crate::services::gateway::{QuoteIntent, build_saved_message_sync_content};
use crate::storage::repositories::{PinWrite, SavedReply, SavedSyncRepository};
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
        /// A row stored without its quote (its parent's snapshot had not
        /// arrived) got its reply context from this one.
        quote_hydrated: bool,
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
    let digest = att.encryption_iv.clone().filter(|d| !d.is_empty())?;
    let m = &att.metadata;
    Some(AttachmentPointer {
        cdn_key,
        encryption_key,
        digest,
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
#[cfg(test)]
pub(crate) fn body_supersedes(stored_edited_at: u64, snapshot: &SavedMessageSync) -> bool {
    snapshot.edited_at > stored_edited_at && !snapshot.text.trim().is_empty()
}

/// The stored body after a synced edit: text replaced, a `Mixed` body keeps
/// its attachment ids (the record's attachments never change).
#[cfg(test)]
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
    // Every clock is stored in a signed column.
    let clocks = [sync.sent_at, sync.edited_at, sync.pin_changed_at];
    if clocks.iter().any(|c| *c > i64::MAX as u64)
        || sync.quote.as_ref().is_some_and(|q| q.id > i64::MAX as u64)
    {
        return Err(MessageError::InvalidContent(
            "SavedMessageSync clock out of range".into(),
        ));
    }
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
    if sync.quote.is_some() {
        let parent = quoted_parent(sync)?;
        if parent == id {
            return Err(MessageError::InvalidContent(
                "SavedMessageSync quotes itself".into(),
            ));
        }
    }
    Ok(id)
}

/// The quoted record's real id, required of every snapshot with a quote.
pub(crate) fn quoted_parent(sync: &SavedMessageSync) -> Result<MessageId, MessageError> {
    sync.quoted_message_id.parse().map_err(|_| {
        MessageError::InvalidContent(format!(
            "SavedMessageSync quote without a valid quoted_message_id: {:?}",
            sync.quoted_message_id
        ))
    })
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
        self.message_repo.insert_saved_outgoing(&message)?;

        // The local row and intent are committed. A failed immediate send
        // remains queued for startup/reconnect recovery.
        let stored = self
            .message_repo
            .get(&message.id)?
            .unwrap_or_else(|| message.clone());
        self.sync_saved_record(&stored).await;

        Ok(message)
    }

    /// The snapshot of one stored Saved record as [`SavedMessageSync`]:
    /// its body, real id and clock, attachments as fetchable pointers (and
    /// their CDN keys, for a receiver that only knows those), its quote as
    /// the parent's real id plus the stored preview and thumbnail, and the
    /// edit and pin clocks a receiver orders by. `pin_changed_at` is this
    /// device's watermark for the record (`SavedSyncRepository`): 0 until
    /// its pin state ever changed.
    ///
    /// Whole or not at all: an attachment the row names that cannot be
    /// read, has no record, or has no CDN key and key material is an error,
    /// never a snapshot quietly missing media.
    fn saved_snapshot(
        &self,
        message: &Message,
        pin_changed_at: u64,
    ) -> Result<SavedMessageSync, MessageError> {
        let mut attachments = Vec::with_capacity(message.attachments.len());
        let mut attachment_ids = Vec::with_capacity(message.attachments.len());
        for id in &message.attachments {
            let att = self.attachment_repo.get(id)?.ok_or_else(|| {
                MessageError::InvalidContent(format!(
                    "Saved record {} names attachment {id} that has no record",
                    message.id
                ))
            })?;
            let pointer = attachment_pointer_from_record(&att).ok_or_else(|| {
                MessageError::InvalidContent(format!(
                    "Saved record {} attachment {id} was never uploaded (no CDN key)",
                    message.id
                ))
            })?;
            attachment_ids.push(pointer.cdn_key.clone());
            attachments.push(pointer);
        }
        // The quote is the row's own reply context, as an ordinary message
        // keeps it after its parent is gone: the parent's real id, the stored
        // preview and thumbnail. The parent's clock is added when we still
        // hold it; a receiver needs none of it to store the reply.
        let (quote, quoted_message_id) = match message.reply_to {
            Some(parent_id) => {
                let parent_sent_at = self
                    .message_repo
                    .get(&parent_id)?
                    .map(|p| p.sent_at)
                    .unwrap_or_default();
                let attachments = match (
                    &message.reply_to_thumbnail,
                    &message.reply_to_thumbnail_mime,
                ) {
                    (Some(thumb), mime) if !thumb.is_empty() => vec![QuotedAttachment {
                        content_type: mime.clone().unwrap_or_default(),
                        file_name: String::new(),
                        thumbnail: thumb.clone(),
                    }],
                    _ => vec![],
                };
                let quote = Quote {
                    id: parent_sent_at,
                    text: message.reply_to_text.clone().unwrap_or_default(),
                    attachments,
                    author_aci: message
                        .reply_to_author_id
                        .or(message.sender_id)
                        .map(|a| a.to_string())
                        .unwrap_or_default(),
                    r#type: quote::Type::Normal as i32,
                    author_aci_binary: vec![],
                };
                (Some(quote), parent_id.to_string())
            }
            None => (None, String::new()),
        };
        if message.is_pinned && pin_changed_at == 0 {
            return Err(MessageError::InvalidContent(
                "Pinned Saved row without watermark".into(),
            ));
        }
        Ok(SavedMessageSync {
            text: message.as_text().unwrap_or_default().to_string(),
            message_id: message.id.to_string(),
            sent_at: message.sent_at,
            attachment_ids,
            attachments,
            quote,
            quoted_message_id,
            edited_at: if message.is_edited {
                nanos(message.edited_at)
            } else {
                0
            },
            // v092 seeds clocks for legacy pins; never silently advertise an
            // inconsistent pinned row as unpinned.
            pinned: message.is_pinned,
            pin_changed_at,
        })
    }

    /// Send the current full row; retain the durable obligation until every
    /// own device is acknowledged. Incoming snapshots never enqueue intent.
    pub(crate) async fn sync_saved_record(&self, message: &Message) -> usize {
        let _gate = self.saved_sync_gate.lock().await;
        let Ok(owner) = self.current_user_id() else {
            return 0;
        };
        let journal = SavedSyncRepository::new(self.message_repo.db());
        let generation = match journal.pending(&owner, &message.id) {
            Ok(Some(generation)) => generation,
            _ => return 0,
        };
        let (current, pin_changed_at) = match self
            .message_repo
            .saved_sync_snapshot_row(&owner, &message.id)
        {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => {
                let _ = journal.finish(&owner, &message.id, generation, true);
                return 0;
            }
            Err(error) => {
                tracing::warn!(%error, "Saved recovery row unavailable");
                let _ = journal.finish(&owner, &message.id, generation, false);
                return 0;
            }
        };
        let snapshot = match self.saved_snapshot(&current, pin_changed_at) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::warn!(message_id = %current.id, %error, "Saved snapshot remains pending");
                let _ = journal.finish(&owner, &current.id, generation, false);
                return 0;
            }
        };
        // If a local write raced snapshot construction, its generation
        // supersedes this attempt and stays pending for the next pass.
        if journal.pending(&owner, &current.id).ok().flatten() != Some(generation)
            || crate::services::gateway::submission::check_send_session().is_err()
        {
            return 0;
        }
        let content = build_saved_message_sync_content(snapshot);
        let (synced, acknowledged) = match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.sync_to_own_devices_acknowledged(&mut self.auth_client(), content),
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(_) => (0, false),
        };
        if self.current_user_id().ok() == Some(owner)
            && crate::services::gateway::submission::check_send_session().is_ok()
        {
            if let Err(error) = journal.finish(&owner, &current.id, generation, acknowledged) {
                tracing::warn!(%error, "Saved sync acknowledgement could not be persisted");
            }
        }
        tracing::debug!(message_id = %current.id, synced, acknowledged, "Saved own-device fanout");
        synced
    }

    /// Serialize with snapshot fanout; deletion intent wins before network submission.
    /// `prepare` is false for recovery and already-recorded user retries.
    pub(crate) async fn submit_saved_delete(
        &self,
        owner: UserId,
        id: MessageId,
        sent_at: u64,
        prepare: bool,
    ) -> Result<bool, MessageError> {
        let _gate = self.saved_sync_gate.lock().await;
        crate::services::gateway::submission::check_send_session()?;
        if self.current_user_id()? != owner {
            return Err(MessageError::NotLoggedIn);
        }
        if prepare {
            self.message_repo
                .prepare_own_saved_delete(&owner, &id, sent_at)?;
        }
        let journal = SavedSyncRepository::new(self.database.clone());
        if !prepare && journal.deleted_identity(&owner, sent_at)? != Some(id) {
            return Err(MessageError::InvalidContent(
                "Saved deletion identity changed".into(),
            ));
        }
        let Some((generation, clock)) = journal.pending_delete(&owner, &id)? else {
            return Ok(true);
        };
        if clock != sent_at {
            return Err(MessageError::InvalidContent(
                "Saved deletion clock changed".into(),
            ));
        }
        let mut content = crate::services::build_delete_for_me_content(
            &owner.to_string(),
            &owner.to_string(),
            sent_at,
        );
        if let Some(crate::proto::content_v1::content::Content::DeleteForMeSync(sync)) =
            content.content.as_mut()
        {
            sync.saved_message_id = id.to_string();
            sync.saved_messages_scope = true;
        }
        let acknowledged = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.sync_to_own_devices_acknowledged(&mut self.auth_client(), content),
        )
        .await
        .is_ok_and(|(_, acknowledged)| acknowledged);
        if crate::services::gateway::submission::check_send_session().is_err()
            || self.current_user_id().ok() != Some(owner)
        {
            return Ok(false);
        }
        Ok(journal.finish_delete(&owner, &id, generation, sent_at, acknowledged)?)
    }

    /// Apply a snapshot through the shared storage-only receive path.
    pub fn apply_saved_message_sync(
        &self,
        sync: &SavedMessageSync,
    ) -> Result<SavedSyncOutcome, MessageError> {
        SavedSnapshotStore {
            message_repo: self.message_repo.clone(),
            attachment_repo: self.attachment_repo.clone(),
            settings_repo: self.settings_repo.clone(),
        }
        .apply_saved_message_sync(sync)
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

/// The synchronous writer shared by the service and device inbox: receipt
/// must be durable before that inbox acknowledges the encrypted envelope.
struct SavedSnapshotStore {
    message_repo: std::sync::Arc<crate::storage::MessageRepository>,
    attachment_repo: std::sync::Arc<crate::storage::AttachmentRepository>,
    settings_repo: std::sync::Arc<crate::storage::SettingsRepository>,
}

pub(crate) fn persist_saved_device_snapshot(
    gateway: &crate::services::gateway::MessageGateway,
    sender: UserId,
    sync: &SavedMessageSync,
) -> Result<SavedSyncOutcome, crate::error::ServiceError> {
    use crate::error::ServiceError;
    let own = gateway.settings_repo.current_user_id()?;
    if sender != own {
        return Err(ServiceError::Validation(
            "Saved snapshot from another user".into(),
        ));
    }
    SavedSnapshotStore {
        message_repo: gateway.message_repo.clone(),
        attachment_repo: std::sync::Arc::new(crate::storage::AttachmentRepository::new(
            gateway.message_repo.db(),
        )),
        settings_repo: gateway.settings_repo.clone(),
    }
    .apply_saved_message_sync(sync)
    .map_err(|e| match e {
        MessageError::Storage(e) => ServiceError::Storage(e),
        MessageError::NotLoggedIn => ServiceError::NotLoggedIn,
        MessageError::InvalidContent(e) => ServiceError::Validation(e),
        e => ServiceError::Internal(e.to_string()),
    })
}

impl SavedSnapshotStore {
    /// The reply context a snapshot's quote gives a Saved row here. The
    /// parent is named by its real id: held here, the preview and thumbnail
    /// come from it, as a local reply's do (`fill_reply_context`); not held
    /// (its own snapshot not here yet, or deleted here), the snapshot's
    /// preview and thumbnail are kept with the dangling id, as an ordinary
    /// message keeps its quote after the parent is gone — so a reply never
    /// loses its quote to ordering, and reload shows the parent once it is
    /// here. A row under the parent's id that is not our Saved record is
    /// refused: `reply_to` must never point into another conversation.
    fn reply_for_quote(
        &self,
        own: UserId,
        scope: &ReactionScope,
        sync: &SavedMessageSync,
    ) -> Result<Option<SavedReply>, MessageError> {
        let Some(q) = &sync.quote else {
            return Ok(None);
        };
        let parent = quoted_parent(sync)?;
        let held = match self.message_repo.get(&parent)? {
            Some(p)
                if p.conversation_id == scope.id
                    && p.conversation_type == scope.kind
                    && p.sender_id == Some(own) =>
            {
                Some(p)
            }
            Some(_) => {
                return Err(MessageError::InvalidContent(format!(
                    "SavedMessageSync quotes a row outside our Saved conversation: {parent}"
                )));
            }
            None => None,
        };
        let reply = match held {
            Some(p) if !p.is_deleted => {
                let atts: Vec<_> = p
                    .attachments
                    .iter()
                    .filter_map(|id| self.attachment_repo.get(id).ok().flatten())
                    .collect();
                let preview = crate::services::gateway::shared::message_reply_preview(&p, &atts);
                let quoted = crate::services::gateway::shared::build_quoted_attachments(&atts);
                let (thumbnail, thumbnail_mime) = quoted
                    .first()
                    .map(|qa| {
                        (
                            (!qa.thumbnail.is_empty()).then(|| qa.thumbnail.clone()),
                            (!qa.content_type.is_empty()).then(|| qa.content_type.clone()),
                        )
                    })
                    .unwrap_or((None, None));
                SavedReply {
                    parent,
                    text: Some(preview)
                        .filter(|t| !t.is_empty())
                        .or_else(|| Some(q.text.clone()).filter(|t| !t.is_empty())),
                    thumbnail,
                    thumbnail_mime,
                }
            }
            _ => {
                let first = q.attachments.first();
                SavedReply {
                    parent,
                    text: Some(q.text.clone()).filter(|t| !t.is_empty()),
                    thumbnail: first
                        .map(|qa| qa.thumbnail.clone())
                        .filter(|t| !t.is_empty()),
                    thumbnail_mime: first
                        .map(|qa| qa.content_type.clone())
                        .filter(|t| !t.is_empty()),
                }
            }
        };
        Ok(Some(reply))
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
    /// is newer than ours (lexically greater text breaks an equal edit clock),
    /// and the pin only when `pin_changed_at` is newer
    /// than the last pin change applied here, each written with its clock
    /// so the next snapshot orders against it. Identity and quote identity of an existing row never change; missing
    /// legacy attachment records can be repaired from compatible full pointers; an absent quote is hydrated. Nothing here sends: a
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

        if self.message_repo.is_tombstoned(&id)?
            || self.message_repo.is_saved_tombstoned(&own, &id)?
        {
            tracing::debug!(message_id = %id, "SavedMessages sync: record deleted here, snapshot ignored");
            return Ok(SavedSyncOutcome::Unchanged);
        }
        let pins = SavedSyncRepository::new(self.message_repo.db());

        let Some(stored) = self.own_saved_row(own, &id)? else {
            return self.store_saved_snapshot(own, id, &scope, sync);
        };
        if stored.is_deleted {
            return Ok(SavedSyncOutcome::Unchanged);
        }
        if stored.sent_at != sync.sent_at {
            return Err(MessageError::InvalidContent(
                "Saved snapshot changed sent_at".into(),
            ));
        }
        // Replays also register full pointers: a legacy sender could have
        // named the CDN key without its crypto record.
        self.register_saved_attachments(sync)?;
        let full_attachments: Vec<_> = sync
            .attachments
            .iter()
            .map(|pointer| AttachmentId::from_cdn_key(&pointer.cdn_key))
            .collect();

        // A row stored before its parent's snapshot arrived, or by a sender
        // that sent no quote, takes this snapshot's quote once; a row that
        // has its parent keeps it.
        let mut quote_hydrated = false;
        if stored.reply_to.is_none() {
            if let Some(reply) = self.reply_for_quote(own, &scope, sync)? {
                quote_hydrated = pins.set_reply_once(&scope, &own, stored.sent_at, &id, &reply)?;
            }
        }

        let Some((body_changed, attachments_changed)) = self.message_repo.write_saved_body(
            &scope,
            &own,
            stored.sent_at,
            &id,
            sync.text.trim(),
            sync.edited_at,
            false,
            &full_attachments,
        )?
        else {
            return Ok(SavedSyncOutcome::Unchanged);
        };

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

        if !body_changed && pin_changed.is_none() && !quote_hydrated && !attachments_changed {
            return Ok(SavedSyncOutcome::Unchanged);
        }
        let message = self
            .message_repo
            .get(&id)?
            .ok_or(MessageError::MessageNotFound(id))?;
        tracing::info!(
            message_id = %id,
            body_changed,
            attachments_changed,
            ?pin_changed,
            quote_hydrated,
            "SavedMessages: applied snapshot from another device"
        );
        Ok(SavedSyncOutcome::Updated {
            message,
            // Attachment repair also changes the bubble's visible body.
            body_changed: body_changed || attachments_changed,
            pin_changed,
            quote_hydrated,
        })
    }

    /// Register every pointer and refuse to publish a row with missing records.
    /// Stable ids are derived from real CDN keys, never internal sender ids.
    fn register_saved_attachments(
        &self,
        sync: &SavedMessageSync,
    ) -> Result<Vec<AttachmentId>, MessageError> {
        if sync
            .attachments
            .iter()
            .any(|p| p.cdn_key.is_empty() || p.encryption_key.is_empty() || p.digest.is_empty())
        {
            return Err(MessageError::InvalidContent(
                "Incomplete Saved attachment pointer".into(),
            ));
        }
        let mut ids = AttachmentService::new_local(self.attachment_repo.clone())
            .create_from_pointers(&sync.attachments);
        for pointer in &sync.attachments {
            let id = AttachmentId::from_cdn_key(&pointer.cdn_key);
            if self.attachment_repo.get(&id)?.is_none() {
                return Err(MessageError::Storage(
                    crate::error::StorageError::InvalidData(format!(
                        "Saved attachment {id} was not registered"
                    )),
                ));
            }
        }
        for key in sync.attachment_ids.iter().filter(|k| !k.is_empty()) {
            let legacy = AttachmentId::from_cdn_key(key);
            if !ids.contains(&legacy) {
                ids.push(legacy);
            }
        }
        Ok(ids)
    }

    /// Store a snapshot of a record we do not hold, whole.
    fn store_saved_snapshot(
        &self,
        own: UserId,
        id: MessageId,
        scope: &ReactionScope,
        sync: &SavedMessageSync,
    ) -> Result<SavedSyncOutcome, MessageError> {
        let attachment_ids = self.register_saved_attachments(sync)?;

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

        if let Some(reply) = self.reply_for_quote(own, scope, sync)? {
            message.reply_to = Some(reply.parent);
            message.reply_to_author_id = Some(own);
            message.reply_to_text = reply.text;
            message.reply_to_thumbnail = reply.thumbnail;
            message.reply_to_thumbnail_mime = reply.thumbnail_mime;
        }

        if !self
            .message_repo
            .insert_saved_snapshot(&message, sync.pin_changed_at)?
        {
            return Ok(SavedSyncOutcome::Unchanged);
        }
        let stored = self
            .message_repo
            .get(&id)?
            .ok_or(MessageError::MessageNotFound(id))?;
        tracing::info!(message_id = %id, "SavedMessages: stored snapshot from another device");
        Ok(SavedSyncOutcome::Created(stored))
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
        assert!(
            validate_snapshot(&bad).is_err(),
            "quote without its parent id"
        );
        bad.quoted_message_id = bad.message_id.clone();
        assert!(validate_snapshot(&bad).is_err(), "quotes itself");
        bad.quoted_message_id = MessageId::new().to_string();
        assert!(
            validate_snapshot(&bad).is_ok(),
            "parent id is all a quote needs"
        );
        let mut bad = snapshot();
        bad.pin_changed_at = u64::MAX;
        bad.pinned = true;
        assert!(
            validate_snapshot(&bad).is_err(),
            "clock past the signed column"
        );
        let mut bad = snapshot();
        bad.sent_at = i64::MAX as u64 + 1;
        assert!(
            validate_snapshot(&bad).is_err(),
            "sent_at past the signed column"
        );
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
        let pins = SavedSyncRepository::new(db.clone());
        let own = UserId::new();
        crate::storage::repositories::settings_repo::SettingsRepository::new(db.clone())
            .set(
                crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
                &own.to_string(),
            )
            .unwrap();
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
        // `pinned_at` is the pin's own clock, not the receipt time, so the
        // pinned list orders the same on every device.
        let pinned_at = |id: &MessageId| -> Option<i64> {
            db.read_connection()
                .unwrap()
                .query_row(
                    "SELECT pinned_at FROM messages WHERE id = ?1",
                    rusqlite::params![id.to_string()],
                    |row| row.get(0),
                )
                .unwrap()
        };
        assert_eq!(pinned_at(&saved.id), None, "unpin clears it");
        assert_eq!(
            pins.apply_synced_pin(&scope, &own, sent_at, &saved.id, true, 12_000)
                .unwrap(),
            PinWrite::Applied { changed_at: 12_000 }
        );
        assert_eq!(pinned_at(&saved.id), Some(12_000));
        // Same-clock conflict: unpin wins, regardless of delivery order.
        pins.record_pin_watermark(&saved.id, 13_000).unwrap();
        assert!(repo.get(&saved.id).unwrap().unwrap().is_pinned);
        assert_eq!(
            pins.apply_synced_pin(&scope, &own, sent_at, &saved.id, true, 13_000)
                .unwrap(),
            PinWrite::Stale,
            "equal clock, same state: nothing to repair"
        );
        assert_eq!(
            pins.apply_synced_pin(&scope, &own, sent_at, &saved.id, false, 13_000)
                .unwrap(),
            PinWrite::Applied { changed_at: 13_000 }
        );
        assert!(!repo.get(&saved.id).unwrap().unwrap().is_pinned);
        // Back to the unpinned state for the local-clock check below.
        assert_eq!(pins.pin_watermark(&saved.id).unwrap(), 13_000);

        // A local pin whose clock reads before the watermark still orders after it.
        let skewed = pins
            .apply_local_pin(&scope, &own, sent_at, &saved.id, true, 7_000)
            .unwrap();
        assert_eq!(skewed, PinWrite::Applied { changed_at: 13_001 });

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
        assert!(pins.record_pin_watermark(&ghost, u64::MAX).is_err());

        // A reply stored before its parent takes its quote once, dangling
        // parent id included; a row that has a parent is left alone.
        let reply = SavedReply {
            parent: ghost,
            text: Some("quoted".into()),
            thumbnail: Some(vec![9, 9]),
            thumbnail_mime: Some("image/jpeg".into()),
        };
        assert!(
            pins.set_reply_once(&scope, &own, sent_at, &saved.id, &reply)
                .unwrap()
        );
        let row = repo.get(&saved.id).unwrap().unwrap();
        assert_eq!(row.reply_to, Some(ghost));
        assert_eq!(row.reply_to_author_id, Some(own));
        assert_eq!(row.reply_to_text.as_deref(), Some("quoted"));
        assert_eq!(row.reply_to_thumbnail, Some(vec![9, 9]));
        assert_eq!(row.reply_to_thumbnail_mime.as_deref(), Some("image/jpeg"));
        assert!(
            !pins
                .set_reply_once(&scope, &own, sent_at, &saved.id, &reply)
                .unwrap()
        );
        assert!(repo.get(&twin.id).unwrap().unwrap().reply_to.is_none());
    }
}

/// Session-owned startup/reconnect recovery with a bounded batch and durable backoff.
pub(crate) fn spawn_saved_sync_recovery(
    client: &std::sync::Arc<crate::client::ChatterClient>,
) -> tokio::task::JoinHandle<()> {
    let weak = std::sync::Arc::downgrade(client);
    let mut connectivity = client.offline_state().subscribe();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut previous = None;
        loop {
            interval.tick().await;
            let Some(client) = weak.upgrade() else {
                break;
            };
            let mut reconnected = false;
            loop {
                match connectivity.try_recv() {
                    Ok(state) => reconnected |= state.is_online,
                    Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                        reconnected = true
                    }
                    Err(_) => break,
                }
            }
            use crate::services::gateway::submission::{SEND_SESSION, SendSession};
            let send_session = SendSession::capture(client.auth_state());
            let state = client.auth_state().get();
            let Some(owner) = state
                .current_user_id
                .filter(|_| client.offline_state().connection_state().is_connected())
            else {
                previous = None;
                continue;
            };
            let session = (owner, state.session_revision);
            let messages = client.messages();
            let journal = SavedSyncRepository::new(messages.database.clone());
            let due = match journal.due(&owner, reconnected || previous != Some(session)) {
                Ok(due) => due,
                Err(error) => {
                    tracing::warn!(%error, "Saved recovery journal unavailable");
                    continue;
                }
            };
            let deletes = match SEND_SESSION
                .scope(send_session.clone(), async {
                    journal.due_deletes(&owner, reconnected || previous != Some(session))
                })
                .await
            {
                Ok(due) => due,
                Err(error) => {
                    tracing::warn!(%error, "Saved deletion journal unavailable");
                    Vec::new()
                }
            };
            previous = Some(session);
            for id in deletes {
                let now = client.auth_state().get();
                if now.current_user_id != Some(owner)
                    || now.session_revision != session.1
                    || !client.offline_state().connection_state().is_connected()
                {
                    break;
                }
                if let Ok(Some((_, sent_at))) = journal.pending_delete(&owner, &id) {
                    match SEND_SESSION
                        .scope(
                            send_session.clone(),
                            messages.submit_saved_delete(owner, id, sent_at, false),
                        )
                        .await
                    {
                        Ok(true) => {
                            client.event_bus().emit_domain(
                                crate::events::DomainEvent::DeleteForMeSync {
                                    conversation_id: ChannelId::from(owner),
                                    target_author_id: owner,
                                    target_message_id: id,
                                    timestamp: TimestampNanos::now().as_u64(),
                                    saved_deletion_persisted: true,
                                },
                            );
                        }
                        Ok(false) => {}
                        Err(error) => tracing::warn!(%error, %id, "Saved deletion remains pending"),
                    }
                }
            }
            for id in due {
                let now = client.auth_state().get();
                if now.current_user_id != Some(owner)
                    || now.session_revision != session.1
                    || !client.offline_state().connection_state().is_connected()
                {
                    break;
                }
                if let Ok(Some(row)) = messages.message_repo.get(&id) {
                    SEND_SESSION
                        .scope(send_session.clone(), messages.sync_saved_record(&row))
                        .await;
                }
            }
        }
    })
}
