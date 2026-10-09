//! Inbound path for server-delivered channel messages.
//!
//! One place decrypts, persists and announces a `chat_v1::Message` that the
//! server handed us for a server-managed channel. Two callers share it:
//!
//! - the live event stream (`services::channel_stream`), and
//! - the durable catch-up fetch (`sync::message_sync`, `GetMessages`).
//!
//! They must agree, or a message reads correctly when it arrives live and as
//! the literal string "[Encrypted message]" when it arrives through a backfill
//! — which is exactly what happened before this module existed: the sync path
//! called `proto_to_domain_message_with_sender` directly, and that maps
//! `Content::Encrypted` to a placeholder.

use std::sync::Arc;

use prost::Message as _;

use crate::domain::ids::{ChannelId, UserId};
use crate::domain::message::Message;
use crate::events::EventBus;
use crate::inbox::DomainEvent;
use crate::proto::chat_v1;
use crate::proto::content_v1::{self, GroupEncrypted};
use crate::services::attachment::AttachmentService;
use crate::services::current_user::CurrentUserProvider;

use super::conversion::{conversation_type_for_channel, proto_to_domain_message_with_sender};
use super::{MessageError, MessageService};

// Scope includes the complete envelope digest, so a new edit or repaired ciphertext
// is immediately eligible. Bounded volatile state only delays work: server recovery
// and message-sync checkpoints remain pending until durable handling succeeds.
pub(crate) type DecryptFailureGate = std::collections::HashMap<
    (ChannelId, UserId, u32, [u8; 32]),
    (std::time::Instant, u32),
>;

/// Outcome of ingesting one server-delivered channel message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ingested {
    /// Stored (or updated) and announced on the domain bus.
    Stored,
    /// Our own message, already rendered optimistically at send time.
    OwnMessage,
    /// Parked waiting for the sender's key distribution message.
    DeferredPendingSenderKey,
    /// Could not be used at all (bad proto, undecryptable, conversion failed).
    Dropped,
    /// Storage or recoverable decrypt failure; stream must not acknowledge it.
    RetryableFailure,
}

fn ensure_resync_ingested(outcome: Ingested) -> Result<(), MessageError> {
    match outcome {
        Ingested::RetryableFailure | Ingested::DeferredPendingSenderKey => {
            Err(MessageError::ResyncPending)
        }
        // Tombstones are handled before ingest. A rejected resync payload is
        // a validation error, never evidence that authoritative truth was applied.
        Ingested::Stored | Ingested::OwnMessage => Ok(()),
        Ingested::Dropped => Err(MessageError::InvalidContent(
            "resync message rejected".into(),
        )),
    }
}

/// Shared decrypt → persist → announce path for channel messages.
pub struct ChannelInbound {
    messages: Arc<MessageService>,
    attachments: Arc<AttachmentService>,
}

impl ChannelInbound {
    pub fn new(messages: Arc<MessageService>, attachments: Arc<AttachmentService>) -> Self {
        Self {
            messages,
            attachments,
        }
    }

    fn event_bus(&self) -> &EventBus {
        &self.messages.event_bus
    }

    /// Decrypt, persist and announce one channel message.
    ///
    /// `emit` controls whether a `DomainEvent::MessageReceived` is published:
    /// the live stream wants it (the UI should react immediately), a bulk
    /// backfill of a hundred old messages does not.
    pub async fn ingest(
        &self,
        proto_msg: &chat_v1::Message,
        channel_id: ChannelId,
        emit: bool,
    ) -> Ingested {
        // THIS device's own messages are skipped only on the LIVE path, where
        // they are already on screen from the optimistic send. On a backfill they must
        // still be stored, or a reinstall loses this user's own half of the
        // conversation. (An own message in an ENCRYPTED channel still drops
        // out below — we cannot decrypt our own sender-key ciphertext — which
        // is what the message-backup path exists for.)
        let (mut message, authenticated_identity) =
            match self.prepare(proto_msg, channel_id, emit).await {
                Ok(m) => m,
                Err(outcome) => return outcome,
            };

        if !self.persist(&mut message, channel_id, authenticated_identity) {
            return Ingested::RetryableFailure;
        }

        if emit {
            let _ = self
                .event_bus()
                .domain_sender()
                .send(DomainEvent::MessageReceived { message });
        }

        Ingested::Stored
    }

    /// Same path, for a message the server says was EDITED
    /// (`message_updated`). Decrypts like any channel message (sender keys
    /// for an encrypted channel), then applies the new body through the one
    /// edit choke point,
    /// [`MessageService::apply_channel_edit`]: only the author's own message
    /// in this channel, ordered and dated by the server's `edited_at`, within
    /// Signal's 48h, held if the message is not here yet — and a re-delivered
    /// edit (a catch-up re-fetch) changes nothing and announces nothing. The
    /// author is the server message's `author_id`, which for an encrypted
    /// channel is also the sender-key sender that decrypted it.
    ///
    /// Announces `MessageEdited` so an open conversation patches the bubble
    /// in place instead of appending a duplicate.
    pub async fn ingest_edit(
        &self,
        proto_msg: &chat_v1::Message,
        channel_id: ChannelId,
    ) -> Ingested {
        let message = match self.prepare(proto_msg, channel_id, true).await {
            Ok((m, _)) => m,
            Err(outcome) => return outcome,
        };
        // An edit with no known author cannot be authorized.
        let Some(author) = message.sender_id else {
            return Ingested::Dropped;
        };
        // The server names the edited row by id; it must be the author's. Its
        // `sent_at` is the target (an encrypted edit's DataMessage clock need
        // not be the original's).
        let target_sent_at = match self.messages.get_message_local(&message.id) {
            Ok(Some(existing)) if existing.sender_id != Some(author) => {
                tracing::warn!(%channel_id, message_id = %message.id, %author,
                    "Server edit names another author's message; dropped");
                return Ingested::Dropped;
            }
            Ok(Some(existing)) => existing.sent_at,
            Ok(None) => message.sent_at,
            Err(e) => {
                tracing::warn!(%channel_id, message_id = %message.id, error = %e,
                    "Failed to read the edited message; dropped");
                return Ingested::RetryableFailure;
            }
        };
        let body = message.as_text().unwrap_or_default();
        let edit_ts = proto_msg.edited_at;
        match self.messages.apply_channel_edit(
            &channel_id,
            &author,
            target_sent_at,
            body,
            edit_ts,
            edit_ts,
        ) {
            Ok(Some(edited)) => {
                let _ = self
                    .event_bus()
                    .domain_sender()
                    .send(DomainEvent::MessageEdited {
                        message_id: edited.id,
                        new_content: edited.content,
                        channel_id,
                        conversation_type: edited.conversation_type,
                        author_id: author,
                        target_sent_timestamp: target_sent_at,
                    });
                Ingested::Stored
            }
            // Held for its message, refused, or already shown.
            Ok(None) => Ingested::Dropped,
            Err(e) => {
                tracing::warn!(%channel_id, error = %e, "Failed to apply a server channel edit");
                Ingested::RetryableFailure
            }
        }
    }

    /// One undeleted message from a resync page (`get_message_changes`).
    ///
    /// Missing sender keys and retryable ingest failures are typed application
    /// gaps. Validation/storage errors remain distinct; only a fully applied
    /// message application may complete recovery. Peer reaction authority is
    /// independent of the server message projection.
    pub async fn ingest_resynced(
        &self,
        proto_msg: &chat_v1::Message,
        channel_id: ChannelId,
    ) -> Result<(), MessageError> {
        let id = proto_msg
            .id
            .parse::<crate::domain::ids::MessageId>()
            .map_err(|_| MessageError::InvalidContent("invalid resync message id".into()))?;
        if self.messages.message_repo.is_tombstoned(&id)?
            || self.cancelled_own_post(proto_msg, channel_id)?
        {
            return Ok(());
        }
        // Validate the server envelope even when ciphertext was already consumed.
        // Empty authors remain valid for anonymous rows; nonempty identities
        // must parse. Conversion validates parent IDs and content without decrypting.
        let author = if proto_msg.author_id.is_empty() {
            None
        } else {
            Some(
                proto_msg
                    .author_id
                    .parse::<UserId>()
                    .map_err(|_| MessageError::InvalidContent("invalid resync author".into()))?,
            )
        };
        let envelope = proto_to_domain_message_with_sender(
            proto_msg,
            author,
            conversation_type_for_channel(&self.messages.channel_repo, &channel_id),
        )?;
        if envelope.conversation_id != channel_id {
            return Err(MessageError::InvalidContent(
                "resync channel mismatch".into(),
            ));
        }
        let content = proto_msg
            .content
            .as_ref()
            .and_then(|content| content.r#type.as_ref())
            .ok_or_else(|| {
                MessageError::InvalidContent("resync message has no content type".into())
            })?;
        let encrypted = if let chat_v1::message::content::Type::Encrypted(encrypted) = content {
            let group = GroupEncrypted::decode(encrypted.ciphertext.as_slice()).map_err(|_| {
                MessageError::InvalidContent("invalid resync encrypted envelope".into())
            })?;
            if group.channel_id != channel_id.to_string() || group.ciphertext.is_empty() {
                return Err(MessageError::InvalidContent(
                    "invalid resync group envelope".into(),
                ));
            }
            if author.is_none() || proto_msg.author_device_id == 0 {
                return Err(MessageError::InvalidContent(
                    "invalid resync encrypted author".into(),
                ));
            }
            true
        } else {
            false
        };
        let existing = self.messages.get_message_local(&id)?;
        if let Some(local) = &existing {
            // Encrypted envelopes carry a transport clock; the cached sent_at
            // is the authenticated DataMessage clock, so do not equate them.
            if !crate::domain::reaction::channel_snapshot_kind_matches(
                &local.conversation_type,
                encrypted,
            ) || local.sender_id != author
                || local.thread_id != envelope.thread_id
                || (!encrypted && local.sent_at != proto_msg.timestamp)
            {
                return Err(MessageError::InvalidContent(
                    "resync message identity mismatch".into(),
                ));
            }
        }
        if existing
            .as_ref()
            .is_some_and(|local| local.conversation_id != channel_id)
        {
            return Err(MessageError::InvalidContent(
                "resync local channel mismatch".into(),
            ));
        }
        match &existing {
            // Never seen: store it as the server has it, edit included.
            None => {
                ensure_resync_ingested(self.ingest(proto_msg, channel_id, false).await)?;
            }
            Some(local) if is_newer_edit(proto_msg, local) => {
                ensure_resync_ingested(self.ingest_edit(proto_msg, channel_id).await)?;
            }
            Some(_) => {}
        }
        self.messages
            .reconcile_channel_reactions(proto_msg, channel_id)
    }

    /// Decrypt + convert, without touching storage.
    ///
    /// `skip_own` drops messages THIS device authored — right for the live
    /// stream (already rendered optimistically), wrong for a backfill.
    ///
    /// `Err(outcome)` is the non-message result (own message, deferred on a
    /// missing sender key, or dropped).
    async fn prepare(
        &self,
        proto_msg: &chat_v1::Message,
        channel_id: ChannelId,
        skip_own: bool,
    ) -> Result<(Message, bool), Ingested> {
        // The server keeps a message this user deleted for themselves (or a
        // peer deleted for everyone); every catch-up re-fetch hands it back.
        if let Ok(id) = proto_msg.id.parse::<crate::domain::ids::MessageId>() {
            match self.messages.message_repo.is_tombstoned(&id) {
                Ok(false) => {}
                Ok(true) => return Err(Ingested::Dropped),
                Err(e) => {
                    tracing::warn!(%channel_id, message_id = %id, error = %e,
                        "Failed to check delete tombstone; dropping");
                    return Err(Ingested::RetryableFailure);
                }
            }
        }

        // A deleted send whose canonical id never came back: held off like a
        // tombstoned one (`channel_send::suppresses_own_post`).
        match self.cancelled_own_post(proto_msg, channel_id) {
            Ok(false) => {}
            Ok(true) => return Err(Ingested::Dropped),
            Err(e) => {
                tracing::warn!(%channel_id, error = %e,
                    "Failed to check cancelled sends; dropping");
                return Err(Ingested::RetryableFailure);
            }
        }

        let sender_id: Option<UserId> = if proto_msg.author_id.is_empty() {
            None
        } else {
            proto_msg
                .author_id
                .parse::<uuid::Uuid>()
                .ok()
                .map(UserId::from)
        };

        if proto_msg.channel_id.parse::<ChannelId>().ok() != Some(channel_id)
            || (!proto_msg.author_id.is_empty() && sender_id.is_none())
        {
            return Err(Ingested::Dropped);
        }
        let current_user = self.messages.settings_repo.current_user_id().ok();
        let is_own_user = sender_id.is_some() && sender_id == current_user;
        // THIS device's send: already on screen from the optimistic send, and
        // we cannot decrypt our own sender-key ciphertext anyway. A send from
        // another of our devices is a message this device has not seen, and is
        // ingested like anyone else's. (No device id = cannot tell; treated as
        // ours, as before.)
        if skip_own && is_own_user {
            let device_id = proto_msg.author_device_id;
            if device_id == 0 || Some(device_id) == self.current_device_id() {
                return Err(Ingested::OwnMessage);
            }
        }

        // Sender-key (group) ciphertext arrives as a GroupEncrypted proto
        // inside `EncryptedData.ciphertext`.
        let mut inbound_quote: Option<content_v1::Quote> = None;
        let mut authenticated_identity = false;
        let decrypted = if let Some(chat_v1::message::content::Type::Encrypted(enc)) =
            proto_msg.content.as_ref().and_then(|c| c.r#type.as_ref())
        {
            let Some(sid) = sender_id else {
                tracing::debug!(%channel_id, "Encrypted channel message missing sender_id; dropping");
                return Err(Ingested::Dropped);
            };
            let group_encrypted = match GroupEncrypted::decode(enc.ciphertext.as_slice()) {
                Ok(ge) => ge,
                Err(e) => {
                    tracing::warn!(%channel_id, error = %e, "Failed to decode GroupEncrypted");
                    return Err(Ingested::Dropped);
                }
            };
            // Sender-keys are per (uuid, device); the proto must carry the
            // sender's source device.
            let device_id = proto_msg.author_device_id;
            if device_id == 0 {
                tracing::warn!(
                    %channel_id, %sid, message_id = %proto_msg.id,
                    "Encrypted channel message missing author_device_id; dropping"
                );
                return Err(Ingested::Dropped);
            }

            // Our own message from THIS device: we never hold receive state for
            // our own sender key, so it cannot decrypt — and the row is already
            // stored from the send. Trying anyway asked ourselves for an SKDM.
            if is_own_user && self.current_device_id() == Some(device_id) {
                return Err(Ingested::OwnMessage);
            }
            // Already held (a catch-up re-fetch): the first decrypt consumed the
            // message key, so a second one fails as a duplicate — which is not a
            // failure, and must not ask the sender for a resend.
            if let Ok(id) = proto_msg.id.parse::<crate::domain::ids::MessageId>() {
                match self.messages.get_message_local(&id) {
                    Ok(Some(existing)) if !is_newer_edit(proto_msg, &existing) => {
                        return Err(Ingested::Dropped);
                    }
                    Err(_) => return Err(Ingested::RetryableFailure),
                    _ => {}
                }
            }

            // Only a member of our VERIFIED membership may speak in the
            // channel (Signal drops group messages from non-members). A sender
            // the server alone vouches for is dropped before decrypting — and
            // before it can draw a SenderKeyRequest or a resend request.
            if !self.messages.group_gate().admits(&channel_id, &sid) {
                tracing::warn!(%channel_id, %sid, message_id = %proto_msg.id,
                    "Group message from a sender outside the verified membership; dropped");
                return Err(Ingested::Dropped);
            }

            use sha2::{Digest, Sha256};
            let failure_key = (
                channel_id,
                sid,
                device_id,
                Sha256::digest(proto_msg.encode_to_vec()).into(),
            );
            if self
                .messages
                .channel_decrypt_failures
                .lock()
                .get(&failure_key)
                .is_some_and(|(due, _)| std::time::Instant::now() < *due)
            {
                return Err(Ingested::RetryableFailure);
            }

            match self
                .messages
                .signal_service
                .decrypt_from_group(&sid, device_id, &group_encrypted)
                .await
            {
                Ok(plaintext) => {
                    let dm = match content_v1::Content::decode(plaintext.as_slice())
                        .ok()
                        .and_then(|c| c.content)
                    {
                        Some(content_v1::content::Content::DataMessage(dm)) => dm,
                        _ => {
                            tracing::warn!(
                                %channel_id, %sid,
                                "Decrypted channel plaintext is not a DataMessage; dropping"
                            );
                            return Err(Ingested::Dropped);
                        }
                    };

                    if dm.channel_id.parse::<ChannelId>().ok() != Some(channel_id)
                        || dm.timestamp == 0
                        || (!dm.message_id.is_empty() && dm.message_id != proto_msg.id)
                        || ((!dm.message_id.is_empty() || !dm.thread_id.is_empty())
                            && dm.thread_id != proto_msg.parent_id)
                    {
                        return Err(Ingested::Dropped);
                    }
                    authenticated_identity = !dm.message_id.is_empty();
                    // The sender's verified (revision, state_hash): fork or
                    // staleness check against ours.
                    self.messages.check_group_stamp(&channel_id, &dm);

                    inbound_quote = dm.quote.clone();

                    if !dm.attachments.is_empty() {
                        self.attachments.create_from_pointers(&dm.attachments);
                    }

                    let files: Vec<chat_v1::FileMetadata> = dm
                        .attachments
                        .iter()
                        .filter(|p| !p.cdn_key.is_empty())
                        .map(|p| chat_v1::FileMetadata {
                            id: crate::domain::AttachmentId::from_cdn_key(&p.cdn_key).to_string(),
                            name: p.file_name.clone(),
                            mime_type: p.content_type.clone(),
                            size: p.plaintext_size,
                            ..Default::default()
                        })
                        .collect();
                    let new_type = if files.is_empty() {
                        chat_v1::message::content::Type::Text(dm.body.clone())
                    } else {
                        chat_v1::message::content::Type::Mixed(chat_v1::message::MixedContent {
                            text: dm.body.clone(),
                            files,
                        })
                    };
                    let mut patched = proto_msg.clone();
                    // Encrypted edits, pins and receipts address the original
                    // DataMessage clock, as the sender stores it. The server's
                    // envelope timestamp is only transport metadata.
                    patched.timestamp = dm.timestamp;
                    patched.content = Some(chat_v1::message::Content {
                        r#type: Some(new_type),
                    });
                    patched
                }
                Err(e) => {
                    {
                        let mut failures = self.messages.channel_decrypt_failures.lock();
                        // Evict one oldest deadline, never clear every sender's gate.
                        if failures.len() >= 512 && !failures.contains_key(&failure_key) {
                            if let Some(oldest) = failures
                                .iter()
                                .min_by_key(|(_, v)| v.0)
                                .map(|(key, _)| *key)
                            {
                                failures.remove(&oldest);
                            }
                        }
                        let now = std::time::Instant::now();
                        let entry = failures.entry(failure_key).or_insert((now, 0));
                        let delay = std::time::Duration::from_secs(30)
                            .saturating_mul(1u32 << entry.1.min(10))
                            .min(std::time::Duration::from_secs(15 * 60));
                        *entry = (now + delay, entry.1.saturating_add(1));
                    }
                    let es = e.to_string();
                    if es.contains("no sender key")
                        || es.contains("missing sender key")
                        || es.contains("SenderKeyRecord")
                        || es.contains("not found")
                    {
                        // A device the server no longer lists (typically our
                        // own previous install) will never send its SKDM:
                        // parking the message only ends in a request to a
                        // device that does not exist.
                        if self
                            .messages
                            .is_retired_device(sid, device_id, proto_msg.timestamp)
                            .await
                        {
                            tracing::debug!(%channel_id, %sid, device_id, message_id = %proto_msg.id,
                                "Channel message from a retired device is unrecoverable; dropped");
                            self.messages
                                .record_channel_delivery_issue(channel_id, sid, proto_msg);
                            return Err(Ingested::Dropped);
                        }
                        tracing::warn!(
                            %channel_id, %sid, device_id,
                            "Missing sender key; deferring message and requesting SKDM"
                        );
                        self.messages.defer_pending_group_decrypt(
                            channel_id,
                            sid,
                            device_id,
                            proto_msg.clone(),
                        );
                        if self
                            .messages
                            .should_request_sender_key(channel_id, sid, device_id)
                        {
                            let msg_svc = self.messages.clone();
                            tokio::spawn(async move {
                                if let Err(e) =
                                    msg_svc.send_sender_key_request(&channel_id, &sid).await
                                {
                                    tracing::warn!(
                                        %channel_id, %sid, error = %e,
                                        "Failed to send SenderKeyRequest"
                                    );
                                }
                            });
                        }
                        return Err(Ingested::DeferredPendingSenderKey);
                    }
                    // We hold a key, but not one that opens this message
                    // (typically "old counter": it predates the SKDM we got).
                    // Only the sender can give it back.
                    tracing::warn!(%channel_id, %sid, error = %es,
                        "Failed to decrypt channel message; asking the sender to resend it");
                    let msg_svc = self.messages.clone();
                    let proto = proto_msg.clone();
                    tokio::spawn(async move {
                        msg_svc
                            .request_group_resend(
                                channel_id,
                                sid,
                                device_id,
                                &proto,
                                &group_encrypted.ciphertext,
                            )
                            .await;
                    });
                    return Err(Ingested::RetryableFailure);
                }
            }
        } else {
            proto_msg.clone()
        };

        let conv_type = conversation_type_for_channel(&self.messages.channel_repo, &channel_id);
        let mut message = match proto_to_domain_message_with_sender(
            &decrypted, sender_id, conv_type,
        ) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(%channel_id, error = %e, "Failed to convert channel message to domain");
                return Err(Ingested::Dropped);
            }
        };

        crate::services::gateway::inbound::apply_inbound_quote(
            &mut message,
            inbound_quote.as_ref(),
            &self.messages.message_repo,
        );
        // Our own message is stored read by the repository
        // (`UnifiedMessageRepository::stored_is_read`), on this path and every other.

        Ok((message, authenticated_identity))
    }

    /// This account's post from this device (or an unknown device, as
    /// `prepare` treats it), not held here, that a user-deleted, uncertain
    /// send may have made. A post already held is ours to keep updating.
    fn cancelled_own_post(
        &self,
        proto_msg: &chat_v1::Message,
        channel_id: ChannelId,
    ) -> Result<bool, crate::error::StorageError> {
        let Ok(owner) = self.messages.settings_repo.current_user_id() else {
            return Ok(false);
        };
        let device = proto_msg.author_device_id;
        if proto_msg.author_id.parse::<UserId>().ok() != Some(owner)
            || (device != 0 && Some(device) != self.current_device_id())
        {
            return Ok(false);
        }
        if let Ok(id) = proto_msg.id.parse::<crate::domain::ids::MessageId>() {
            if self.messages.message_repo.get(&id)?.is_some() {
                return Ok(false);
            }
        }
        super::channel_send::suppresses_own_post(
            &self.messages.database,
            owner,
            channel_id,
            proto_msg.timestamp,
        )
    }

    fn current_device_id(&self) -> Option<u32> {
        self.messages
            .settings_repo
            .get::<u32>(crate::storage::repositories::settings_repo::keys::CURRENT_DEVICE_ID)
            .ok()
            .flatten()
    }

    /// Upsert, preserving attachments and reactions already stored locally.
    fn persist(
        &self,
        message: &mut Message,
        channel_id: ChannelId,
        authenticated_identity: bool,
    ) -> bool {
        Self::persist_message(&self.messages, message, channel_id, authenticated_identity)
    }

    pub(crate) fn persist_message(
        messages: &MessageService,
        message: &mut Message,
        channel_id: ChannelId,
        authenticated_identity: bool,
    ) -> bool {
        // Channel controls are peer sealed-sender messages. Server projections
        // cannot replace local reaction truth during backfill or live re-ingest.
        message.reaction_counts.clear();
        message.current_user_reactions.clear();
        match messages.get_message_local(&message.id) {
            Ok(Some(existing)) => {
                if existing.conversation_id != channel_id
                    || existing.conversation_type != message.conversation_type
                    || existing.sender_id != message.sender_id
                    || existing.thread_id != message.thread_id
                {
                    tracing::warn!("Channel envelope identity differs from retained row");
                    return false;
                }
                if message.sent_at != existing.sent_at && !authenticated_identity {
                    tracing::warn!(
                        message_id = %message.id, incoming = message.sent_at, retained = existing.sent_at,
                        "Retained channel clock differs without authenticated message UUID"
                    );
                    return false;
                }
                if authenticated_identity && message.sender_id.is_some() {
                    if let Err(error) = messages
                        .message_repo
                        .bind_authenticated_channel_clock(message)
                    {
                        tracing::warn!(%error, "Channel clock binding rejected");
                        return false;
                    }
                }
                let existing = match messages.get_message_local(&message.id) {
                    Ok(Some(row)) => row,
                    _ => return false,
                };
                // SQL update deliberately retains the original row's identity.
                message.sent_at = existing.sent_at;
                message.created_at = existing.created_at;
                if message.attachments.is_empty() {
                    message.attachments = existing.attachments.clone();
                }
                message.reaction_counts = existing.reaction_counts.clone();
                message.current_user_reactions = existing.current_user_reactions.clone();
                // Read state is local: the server copy always converts as
                // unread, so every catch-up re-fetch of a message the user had
                // already read flipped it back to unread.
                message.is_read |= existing.is_read;
                // Likewise a local soft-delete: the server copy never says deleted.
                message.is_deleted |= existing.is_deleted;
                // Content changes only through the edit path (author check,
                // edit-time ordering, 48h window: `apply_channel_edit`). A
                // re-ingested original — backfill, resync — must not undo an
                // applied edit or bypass those rules.
                if existing.is_edited || existing.as_text() != Some("[Encrypted message]") {
                    message.content = existing.content.clone();
                }
                message.is_edited = existing.is_edited;
                message.edited_at = existing.edited_at;
                if let Err(e) = messages.update_message(message) {
                    tracing::warn!(%channel_id, message_id = %message.id, error = %e,
                        "Failed to update channel message");
                    return false;
                }
            }
            Ok(None) => {
                if let Err(e) = messages.store_message_local(message) {
                    tracing::warn!(%channel_id, message_id = %message.id, error = %e,
                        "Failed to insert channel message");
                    return false;
                }
            }
            Err(e) => {
                tracing::warn!(%channel_id, message_id = %message.id, error = %e,
                    "Failed to read existing message before upsert");
                return false;
            }
        }
        if let Ok(Some(stored)) = messages.get_message_local(&message.id) {
            *message = stored;
        }
        true
    }
}

/// A new edit has fresh ciphertext; only duplicate/older versions reuse the
/// consumed sender key. This comparison also avoids replaying an old edit.
pub(crate) fn is_newer_edit(proto: &chat_v1::Message, existing: &Message) -> bool {
    proto.edited
        && proto.edited_at > 0
        && proto.edited_at
            > existing
                .edited_at
                .and_then(|time| time.timestamp_nanos_opt())
                .and_then(|ns| u64::try_from(ns).ok())
                .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ChatterClient;
    use crate::domain::message::{ConversationType, MessageContent};

    #[test]
    fn a_new_edit_is_not_a_duplicate_of_the_original() {
        let mut original = Message::new_text(
            ChannelId::new(),
            ConversationType::Channel,
            None,
            "original".into(),
            1_000_000,
        );
        let edit = chat_v1::Message {
            edited: true,
            edited_at: 2_000_000,
            ..Default::default()
        };
        assert!(is_newer_edit(&edit, &original));
        original.edited_at = Some(chrono::DateTime::from_timestamp_nanos(2_000_000));
        assert!(!is_newer_edit(&edit, &original));
        let older = chat_v1::Message {
            edited_at: 1_000_000,
            ..edit.clone()
        };
        assert!(!is_newer_edit(&older, &original));
        assert!(!is_newer_edit(&chat_v1::Message::default(), &original));
    }

    /// The catch-up sync re-fetches every message newer than its last run,
    /// including ones that already arrived live and were read. Re-ingesting
    /// one must not flip it back to unread.
    #[tokio::test]
    async fn reingesting_a_read_message_keeps_it_read() {
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        let inbound = ChannelInbound::new(client.messages().clone(), client.attachments().clone());

        let channel_id = ChannelId::new();
        let proto = chat_v1::Message {
            id: crate::domain::ids::MessageId::new().to_string(),
            channel_id: channel_id.to_string(),
            author_id: UserId::new().to_string(),
            timestamp: 1_000,
            content: Some(chat_v1::message::Content {
                r#type: Some(chat_v1::message::content::Type::Text("hi".into())),
            }),
            ..Default::default()
        };
        let unread = || {
            client
                .messages()
                .message_repo
                .get_unread_count(&channel_id, &ConversationType::Channel)
                .unwrap()
        };

        assert_eq!(
            inbound.ingest(&proto, channel_id, false).await,
            Ingested::Stored
        );
        assert_eq!(
            unread(),
            1,
            "positive control: a received message starts unread"
        );

        client
            .mark_conversation_read(&channel_id, &ConversationType::Channel)
            .unwrap();
        assert_eq!(
            inbound.ingest(&proto, channel_id, false).await,
            Ingested::Stored
        );
        assert_eq!(unread(), 0);
    }

    /// A backfill re-ingests the ORIGINAL; it must not undo an edit already
    /// applied through the edit path.
    #[tokio::test]
    async fn reingesting_an_original_keeps_its_applied_edit() {
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        let inbound = ChannelInbound::new(client.messages().clone(), client.attachments().clone());

        let channel_id = ChannelId::new();
        let id = crate::domain::ids::MessageId::new();
        let proto = chat_v1::Message {
            id: id.to_string(),
            channel_id: channel_id.to_string(),
            author_id: UserId::new().to_string(),
            timestamp: 1_000,
            content: Some(chat_v1::message::Content {
                r#type: Some(chat_v1::message::content::Type::Text("original".into())),
            }),
            ..Default::default()
        };
        assert_eq!(
            inbound.ingest(&proto, channel_id, false).await,
            Ingested::Stored
        );

        let mut edited = client
            .messages()
            .get_message_local(&id)
            .unwrap()
            .expect("stored");
        edited.content = crate::domain::message::MessageContent::Text("edited".into());
        edited.is_edited = true;
        client.messages().update_message(&edited).unwrap();

        assert_eq!(
            inbound.ingest(&proto, channel_id, false).await,
            Ingested::Stored
        );
        let stored = client
            .messages()
            .get_message_local(&id)
            .unwrap()
            .expect("stored");
        assert!(
            matches!(stored.content, crate::domain::message::MessageContent::Text(ref t) if t == "edited")
        );
        assert!(stored.is_edited);
    }

    /// Our own message from another device reaches storage by paths that skip
    /// `ingest` (the SKDM retry, a DM or SyncSent copy); whichever path writes
    /// it, it must not count as unread.
    #[tokio::test]
    async fn own_message_is_stored_read_on_any_path() {
        use crate::services::message::conversion::proto_to_domain_message_with_sender;
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        let me = UserId::new();
        client
            .messages()
            .settings_repo
            .set_string(
                crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
                &me.to_string(),
            )
            .unwrap();

        let channel_id = ChannelId::new();
        let store = |sender: UserId| {
            let proto = chat_v1::Message {
                id: crate::domain::ids::MessageId::new().to_string(),
                channel_id: channel_id.to_string(),
                author_id: sender.to_string(),
                timestamp: 1_000,
                content: Some(chat_v1::message::Content {
                    r#type: Some(chat_v1::message::content::Type::Text("hi".into())),
                }),
                ..Default::default()
            };
            let m = proto_to_domain_message_with_sender(
                &proto,
                Some(sender),
                ConversationType::Channel,
            )
            .unwrap();
            assert!(!m.is_read);
            client.messages().store_message_local(&m).unwrap();
        };
        let unread = || {
            client
                .messages()
                .message_repo
                .get_unread_count(&channel_id, &ConversationType::Channel)
                .unwrap()
        };

        store(UserId::new());
        assert_eq!(
            unread(),
            1,
            "positive control: someone else's message is unread"
        );
        store(me);
        assert_eq!(unread(), 1, "our own message counted as unread");
    }

    /// Delete-for-me (and a peer's delete-for-everyone) removes the local row,
    /// but the server still holds the message, so the next catch-up re-fetch
    /// hands it back. It must stay deleted.
    #[tokio::test]
    async fn reingesting_a_deleted_message_keeps_it_deleted() {
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        let inbound = ChannelInbound::new(client.messages().clone(), client.attachments().clone());

        let channel_id = ChannelId::new();
        let message_id = crate::domain::ids::MessageId::new();
        let proto = chat_v1::Message {
            id: message_id.to_string(),
            channel_id: channel_id.to_string(),
            author_id: UserId::new().to_string(),
            timestamp: 1_000,
            content: Some(chat_v1::message::Content {
                r#type: Some(chat_v1::message::content::Type::Text("hi".into())),
            }),
            ..Default::default()
        };

        assert_eq!(
            inbound.ingest(&proto, channel_id, false).await,
            Ingested::Stored
        );
        assert!(
            client
                .messages()
                .get_message_local(&message_id)
                .unwrap()
                .is_some(),
            "positive control: the message was stored"
        );

        client.messages().delete_message_local(&message_id).unwrap();
        let mut stale_malformed = proto.clone();
        stale_malformed.author_id = "bad-author".into();
        stale_malformed.parent_id = "bad-parent".into();
        stale_malformed.content = None;
        inbound
            .ingest_resynced(&stale_malformed, channel_id)
            .await
            .unwrap();
        inbound.ingest(&proto, channel_id, false).await;
        assert!(
            client
                .messages()
                .get_message_local(&message_id)
                .unwrap()
                .is_none(),
            "a re-fetched copy resurrected a deleted message"
        );
    }

    /// A server `message_updated` edit goes through the same edit logic as a
    /// sender-key one: author only, ordered by the edit's time, within 48h,
    /// held until its message arrives, and a re-delivery changes nothing.
    #[tokio::test]
    async fn server_channel_edits_follow_the_edit_rules() {
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        // The resync path reads the current user (own-message handling).
        let settings = &client.messages().settings_repo;
        settings
            .set(
                crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
                &UserId::new().to_string(),
            )
            .unwrap();
        settings
            .set(
                crate::storage::repositories::settings_repo::keys::CURRENT_DEVICE_ID,
                &1u32,
            )
            .unwrap();
        let inbound = ChannelInbound::new(client.messages().clone(), client.attachments().clone());
        let mut events = client.messages().event_bus.subscribe_domain();
        let mut edits = || {
            let mut n = 0;
            while let Ok(e) = events.try_recv() {
                n += usize::from(matches!(e, DomainEvent::MessageEdited { .. }));
            }
            n
        };
        let window = MessageService::EDIT_MAX_AGE_NS;
        let (channel_id, author, stranger) = (ChannelId::new(), UserId::new(), UserId::new());
        let sent = 1_700_000_000_000_000_000u64;
        let message = |id: &crate::domain::ids::MessageId,
                       by: UserId,
                       body: &str,
                       edited_at: u64| chat_v1::Message {
            id: id.to_string(),
            channel_id: channel_id.to_string(),
            author_id: by.to_string(),
            timestamp: sent,
            edited: edited_at > 0,
            edited_at,
            content: Some(chat_v1::message::Content {
                r#type: Some(chat_v1::message::content::Type::Text(body.into())),
            }),
            ..Default::default()
        };
        let id = crate::domain::ids::MessageId::new();
        let body = || {
            client
                .messages()
                .get_message_local(&id)
                .unwrap()
                .map(|m| m.content)
        };
        let text = |s: &str| Some(crate::domain::message::MessageContent::Text(s.into()));

        // Early: held, then applied when the message arrives.
        assert_eq!(
            inbound
                .ingest_edit(&message(&id, author, "early", sent + 10), channel_id)
                .await,
            Ingested::Dropped
        );
        assert_eq!(body(), None, "an edit alone does not create the message");
        assert_eq!(
            inbound
                .ingest(&message(&id, author, "original", 0), channel_id, false)
                .await,
            Ingested::Stored
        );
        assert_eq!(body(), text("early"));
        edits();

        assert_eq!(
            inbound
                .ingest_edit(&message(&id, author, "newer", sent + 30), channel_id)
                .await,
            Ingested::Stored
        );
        assert_eq!(edits(), 1);
        // Older than the edit shown, a re-delivery of it, another author's,
        // and one past 48h: all ignored, nothing announced.
        inbound
            .ingest_edit(&message(&id, author, "older", sent + 20), channel_id)
            .await;
        inbound
            .ingest_edit(&message(&id, author, "newer", sent + 30), channel_id)
            .await;
        inbound
            .ingest_edit(&message(&id, stranger, "hijack", sent + 40), channel_id)
            .await;
        inbound
            .ingest_edit(&message(&id, author, "late", sent + window + 1), channel_id)
            .await;
        assert_eq!(edits(), 0);
        let stored = client.messages().get_message_local(&id).unwrap().unwrap();
        assert_eq!(Some(stored.content), text("newer"));
        assert_eq!(
            stored.edited_at,
            Some(chrono::DateTime::from_timestamp_nanos((sent + 30) as i64))
        );

        // A resync re-fetch of the applied edit: no event, no edited_at churn.
        inbound
            .ingest_resynced(&message(&id, author, "newer", sent + 30), channel_id)
            .await
            .unwrap();
        assert_eq!(edits(), 0);
        assert_eq!(
            client
                .messages()
                .get_message_local(&id)
                .unwrap()
                .unwrap()
                .edited_at,
            stored.edited_at
        );
        // Resync must not turn a mismatched cached target or malformed
        // authoritative reaction snapshot into successful application.
        assert!(matches!(
            inbound
                .ingest_resynced(&message(&id, author, "newer", sent + 30), ChannelId::new())
                .await,
            Err(MessageError::InvalidContent(_))
        ));
        let mut bad_reactions = message(&id, author, "newer", sent + 30);
        bad_reactions.reactions.push(chat_v1::ReactionUpdate {
            target_author_aci: stranger.to_string(),
            target_sent_timestamp: sent,
            reactor_id: stranger.to_string(),
            emoji: "👍".into(),
            ..Default::default()
        });
        assert!(matches!(
            inbound.ingest_resynced(&bad_reactions, channel_id).await,
            Err(MessageError::InvalidContent(_))
        ));
        assert_eq!(edits(), 0);
        // A cached row still validates its envelope before clearing reactions.
        client
            .messages()
            .record_reaction(
                &crate::domain::reaction::ReactionScope::new(channel_id, ConversationType::Channel),
                &author,
                sent,
                &stranger,
                "👍",
                false,
            )
            .unwrap();
        let replay = message(&id, author, "newer", sent + 30);
        let mut malformed = Vec::new();
        let mut bad = replay.clone();
        bad.author_id = "bad-author".into();
        malformed.push(bad);
        let mut bad = replay.clone();
        bad.author_id = stranger.to_string();
        malformed.push(bad);
        let mut bad = replay.clone();
        bad.parent_id = "bad-parent".into();
        malformed.push(bad);
        let mut bad = replay.clone();
        bad.parent_id = crate::domain::ids::MessageId::new().to_string();
        malformed.push(bad);
        let mut bad = replay.clone();
        bad.timestamp += 1;
        malformed.push(bad);
        let mut bad = replay.clone();
        bad.channel_id = ChannelId::new().to_string();
        malformed.push(bad);
        let mut bad = replay.clone();
        bad.content = None;
        malformed.push(bad);
        let mut bad = replay.clone();
        bad.content = Some(chat_v1::message::Content { r#type: None });
        malformed.push(bad);
        let mut bad = replay.clone();
        bad.author_device_id = 1;
        bad.content = Some(chat_v1::message::Content {
            r#type: Some(chat_v1::message::content::Type::Encrypted(
                chat_v1::EncryptedData {
                    ciphertext: vec![0xff],
                    ..Default::default()
                },
            )),
        });
        malformed.push(bad);
        // A valid first reaction must not mutate the ledger before a later invalid entry.
        let mut bad = replay.clone();
        bad.reactions.push(chat_v1::ReactionUpdate {
            target_author_aci: author.to_string(),
            target_sent_timestamp: sent,
            reactor_id: stranger.to_string(),
            emoji: "❤".into(),
            ..Default::default()
        });
        bad.reactions.push(chat_v1::ReactionUpdate {
            target_author_aci: author.to_string(),
            target_sent_timestamp: sent,
            reactor_id: "bad-reactor".into(),
            emoji: "👍".into(),
            ..Default::default()
        });
        malformed.push(bad);
        for bad in malformed {
            assert!(matches!(
                inbound.ingest_resynced(&bad, channel_id).await,
                Err(MessageError::InvalidContent(_))
            ));
            let ledger = client
                .messages()
                .message_repo
                .get_reactions(
                    &crate::domain::reaction::ReactionScope::new(
                        channel_id,
                        ConversationType::Channel,
                    ),
                    &author,
                    sent,
                )
                .unwrap();
            assert_eq!(ledger.len(), 1);
            assert_eq!(ledger[0].emoji.value, "👍");
            assert_eq!(body(), text("newer"));
        }
        // Cached ciphertext is inspected structurally, never decrypted again.
        // The transport timestamp need not equal the stored DataMessage clock.
        let mut encrypted_replay = replay.clone();
        encrypted_replay.author_device_id = 1;
        encrypted_replay.timestamp = sent + 100;
        encrypted_replay.content = Some(chat_v1::message::Content {
            r#type: Some(chat_v1::message::content::Type::Encrypted(
                chat_v1::EncryptedData {
                    ciphertext: GroupEncrypted {
                        channel_id: channel_id.to_string(),
                        ciphertext: vec![1, 2, 3],
                        timestamp: sent,
                    }
                    .encode_to_vec(),
                    ..Default::default()
                },
            )),
        });
        for group in [
            GroupEncrypted::default(),
            GroupEncrypted {
                channel_id: ChannelId::new().to_string(),
                ciphertext: vec![1],
                timestamp: sent,
            },
            GroupEncrypted {
                channel_id: channel_id.to_string(),
                ciphertext: Vec::new(),
                timestamp: sent,
            },
        ] {
            let mut bad = encrypted_replay.clone();
            bad.content = Some(chat_v1::message::Content {
                r#type: Some(chat_v1::message::content::Type::Encrypted(
                    chat_v1::EncryptedData {
                        ciphertext: group.encode_to_vec(),
                        ..Default::default()
                    },
                )),
            });
            assert!(matches!(
                inbound.ingest_resynced(&bad, channel_id).await,
                Err(MessageError::InvalidContent(_))
            ));
            assert_eq!(
                client
                    .messages()
                    .message_repo
                    .get_reactions(
                        &crate::domain::reaction::ReactionScope::new(
                            channel_id,
                            ConversationType::Channel
                        ),
                        &author,
                        sent
                    )
                    .unwrap()
                    .len(),
                1
            );
        }
        encrypted_replay.author_id = author.to_string().to_uppercase();
        inbound
            .ingest_resynced(&encrypted_replay, channel_id)
            .await
            .unwrap();
        // Replies use the same parent UUID in the server envelope and local row.
        let reply_id = crate::domain::ids::MessageId::new();
        let mut reply = message(&reply_id, author, "reply", 0);
        reply.parent_id = id.to_string();
        reply.timestamp = sent + 3;
        assert_eq!(
            inbound.ingest(&reply, channel_id, false).await,
            Ingested::Stored
        );
        reply.content = encrypted_replay.content.clone();
        reply.author_device_id = 1;
        reply.timestamp = sent + 103;
        inbound.ingest_resynced(&reply, channel_id).await.unwrap();
        assert_eq!(
            client
                .messages()
                .get_message_local(&reply_id)
                .unwrap()
                .unwrap()
                .thread_id,
            Some(id)
        );
        assert_eq!(body(), text("newer"));
        assert!(
            client
                .messages()
                .message_repo
                .get_reactions(
                    &crate::domain::reaction::ReactionScope::new(
                        channel_id,
                        ConversationType::Channel
                    ),
                    &author,
                    sent
                )
                .unwrap()
                .len()
                == 1
        );
        // Anonymous cached rows cannot bypass malformed reaction validation.
        let anonymous_id = crate::domain::ids::MessageId::new();
        let mut anonymous = message(&anonymous_id, author, "anonymous", 0);
        anonymous.author_id.clear();
        anonymous.timestamp = sent + 2;
        assert_eq!(
            inbound.ingest(&anonymous, channel_id, false).await,
            Ingested::Stored
        );
        inbound
            .ingest_resynced(&anonymous, channel_id)
            .await
            .unwrap();
        anonymous.reactions.push(chat_v1::ReactionUpdate {
            target_author_aci: "bad-target".into(),
            target_sent_timestamp: sent + 2,
            reactor_id: "bad-reactor".into(),
            emoji: "👍".into(),
            ..Default::default()
        });
        assert!(matches!(
            inbound.ingest_resynced(&anonymous, channel_id).await,
            Err(MessageError::InvalidContent(_))
        ));
        anonymous.reactions[0].reactor_id = stranger.to_string();
        assert!(matches!(
            client
                .messages()
                .reconcile_channel_reactions(&anonymous, channel_id),
            Err(MessageError::InvalidContent(_))
        ));
        // Same channel UUID/author/clock is insufficient authority for a
        // channel snapshot to replace a foreign conversation-kind ledger.
        for kind in [ConversationType::ServerDm, ConversationType::AnonDm] {
            let mut foreign = Message::new_text(
                channel_id,
                kind.clone(),
                Some(author),
                "foreign history".into(),
                sent,
            );
            let scope = crate::domain::reaction::ReactionScope::of(&foreign);
            client.messages().store_message_local(&foreign).unwrap();
            client
                .messages()
                .record_reaction(&scope, &author, sent, &stranger, "👍", false)
                .unwrap();
            let plain = message(&foreign.id, author, "foreign history", 0);
            let encrypted = chat_v1::Message {
                author_device_id: 1,
                content: encrypted_replay.content.clone(),
                ..plain.clone()
            };
            for envelope in [&plain, &encrypted] {
                assert!(matches!(
                    inbound.ingest_resynced(envelope, channel_id).await,
                    Err(MessageError::InvalidContent(_))
                ));
                assert!(matches!(
                    client
                        .messages()
                        .reconcile_channel_reactions(envelope, channel_id),
                    Err(MessageError::InvalidContent(_))
                ));
            }
            foreign = client
                .messages()
                .get_message_local(&foreign.id)
                .unwrap()
                .unwrap();
            assert_eq!(
                foreign.content,
                MessageContent::Text("foreign history".into())
            );
            assert_eq!(foreign.reaction_counts[0].count, 1);
        }
        let legacy = Message::new_text(
            channel_id,
            ConversationType::AnonGroup,
            Some(author),
            "legacy encrypted history".into(),
            sent,
        );
        client.messages().store_message_local(&legacy).unwrap();
        let plain = message(&legacy.id, author, "legacy encrypted history", 0);
        assert!(matches!(
            inbound.ingest_resynced(&plain, channel_id).await,
            Err(MessageError::InvalidContent(_))
        ));
        let encrypted = chat_v1::Message {
            author_device_id: 1,
            content: encrypted_replay.content.clone(),
            ..plain
        };
        inbound
            .ingest_resynced(&encrypted, channel_id)
            .await
            .unwrap();
        assert_eq!(
            client
                .messages()
                .get_message_local(&legacy.id)
                .unwrap()
                .unwrap()
                .conversation_type,
            ConversationType::AnonGroup
        );
        // Valid replay still succeeds after invalid rows were rejected.
        inbound.ingest_resynced(&replay, channel_id).await.unwrap();
        assert_eq!(edits(), 0);
        // A resync of a message never seen stores it, edit included.
        let unseen = crate::domain::ids::MessageId::new();
        inbound
            .ingest_resynced(
                &chat_v1::Message {
                    timestamp: sent + 1,
                    ..message(&unseen, author, "edited", sent + 5)
                },
                channel_id,
            )
            .await
            .unwrap();
        let unseen = client
            .messages()
            .get_message_local(&unseen)
            .unwrap()
            .expect("stored");
        assert_eq!(Some(unseen.content), text("edited"));
    }
}

#[cfg(test)]
mod replay_failure_tests {
    use super::*;

    #[tokio::test]
    async fn failed_sqlite_write_is_not_acknowledged_by_live_or_resync_ingestion() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::system::Directories::with_base_dir(dir.path().to_path_buf())
            .unwrap()
            .database_path();
        std::fs::write(
            db.parent()
                .unwrap()
                .join(crate::system::keyring::NO_KEYRING_MARKER),
            b"",
        )
        .unwrap();
        let client = crate::client::ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .unwrap();
        let inbound = ChannelInbound::new(client.messages().clone(), client.attachments().clone());
        let channel = ChannelId::new();
        let message = chat_v1::Message {
            id: crate::domain::MessageId::new().to_string(),
            channel_id: channel.to_string(),
            author_id: crate::domain::UserId::new().to_string(),
            timestamp: 1000,
            content: Some(chat_v1::message::Content {
                r#type: Some(chat_v1::message::content::Type::Text("retry me".into())),
            }),
            ..Default::default()
        };
        inbound
            .messages
            .database
            .connection()
            .unwrap()
            .execute_batch("PRAGMA query_only=ON")
            .unwrap();
        assert_eq!(
            inbound.ingest(&message, channel, false).await,
            Ingested::RetryableFailure
        );
        assert!(inbound.ingest_resynced(&message, channel).await.is_err());
        inbound
            .messages
            .database
            .connection()
            .unwrap()
            .execute_batch("PRAGMA query_only=OFF")
            .unwrap();
        assert_eq!(
            inbound.ingest(&message, channel, false).await,
            Ingested::Stored
        );
        assert!(
            inbound
                .messages
                .get_message_local(&message.id.parse().unwrap())
                .unwrap()
                .is_some()
        );
    }
}

#[cfg(test)]
mod resync_acknowledgement_tests {
    use super::*;

    #[test]
    fn resync_distinguishes_pending_application_from_rejected_content() {
        assert!(matches!(
            ensure_resync_ingested(Ingested::DeferredPendingSenderKey),
            Err(MessageError::ResyncPending)
        ));
        assert!(matches!(
            ensure_resync_ingested(Ingested::RetryableFailure),
            Err(MessageError::ResyncPending)
        ));
        assert!(matches!(
            ensure_resync_ingested(Ingested::Dropped),
            Err(MessageError::InvalidContent(_))
        ));
        assert!(ensure_resync_ingested(Ingested::Stored).is_ok());
    }
    #[tokio::test]
    async fn peer_channel_reaction_survives_empty_or_partial_server_resync_and_legacy_ambiguity_remains_hidden()
     {
        use crate::client::ChatterClient;
        use crate::domain::channel::{
            Channel, ChannelType, MemberRole, Membership, NotificationPreference,
        };
        use crate::domain::message::ConversationType;
        use crate::domain::reaction::ReactionScope;
        static MOCK: std::sync::Once = std::sync::Once::new();
        MOCK.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder())
        });
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .unwrap();
        let own = UserId::new();
        let peer = UserId::new();
        let author = UserId::new();
        let id = ChannelId::new();
        let service = client.messages();
        service
            .settings_repo
            .set("auth.current_user_id", &own.to_string())
            .unwrap();
        service
            .settings_repo
            .set("auth.current_device_id", &1u32)
            .unwrap();
        for user in [own, peer] {
            service.user_repo.ensure_user_exists(&user).unwrap();
        }
        service
            .channel_repo
            .insert(&Channel::new(
                id,
                "resync reaction fixture".into(),
                ChannelType::Public,
                own,
            ))
            .unwrap();
        service
            .channel_repo
            .insert_membership(&Membership {
                channel_id: id,
                user_id: peer,
                role: MemberRole::Member,
                joined_at: chrono::Utc::now(),
                last_read_message_id: None,
                is_muted: false,
                muted_until: None,
                notification_preference: NotificationPreference::default(),
            })
            .unwrap();
        let scope = ReactionScope::new(id, ConversationType::Channel);
        let message = crate::domain::message::Message::new_text(
            id,
            ConversationType::Channel,
            Some(author),
            "peer authority".into(),
            100,
        );
        service.store_message_local(&message).unwrap();
        assert!(
            service
                .record_inbound_reaction(&scope, &peer, &author, 100, "👍", false, 10)
                .unwrap()
        );
        assert!(
            service
                .record_inbound_reaction(&scope, &own, &author, 100, "👍", false, 11)
                .unwrap()
        );
        let inbound = ChannelInbound::new(service.clone(), client.attachments().clone());
        let mut envelope = chat_v1::Message {
            id: message.id.to_string(),
            channel_id: id.to_string(),
            author_id: author.to_string(),
            timestamp: 100,
            content: Some(chat_v1::message::Content {
                r#type: Some(chat_v1::message::content::Type::Text(
                    "peer authority".into(),
                )),
            }),
            ..Default::default()
        };
        inbound.ingest_resynced(&envelope, id).await.unwrap();
        assert_eq!(
            service
                .message_repo
                .get_reaction_counts(&scope, &author, 100)
                .unwrap()[0]
                .count,
            2
        );
        assert!(
            service
                .user_has_reacted(&scope, &author, 100, "👍")
                .unwrap()
        );
        // A partial server projection has no authority to erase our peer vote.
        envelope.reactions.push(chat_v1::ReactionUpdate {
            target_author_aci: author.to_string(),
            target_sent_timestamp: 100,
            reactor_id: peer.to_string(),
            emoji: "👍".into(),
            ..Default::default()
        });
        inbound.ingest_resynced(&envelope, id).await.unwrap();
        assert_eq!(
            service
                .message_repo
                .get_reaction_counts(&scope, &author, 100)
                .unwrap()[0]
                .count,
            2
        );
        assert_eq!(inbound.ingest(&envelope, id, false).await, Ingested::Stored);
        assert!(
            service
                .user_has_reacted(&scope, &author, 100, "👍")
                .unwrap(),
            "live/backfill projection preserves own highlight too"
        );
        service
            .database
            .connection()
            .unwrap()
            .execute(
                "INSERT INTO reaction_scope_unresolved VALUES(?1,'channel',?2,100)",
                rusqlite::params![id.to_string(), author.to_string()],
            )
            .unwrap();
        envelope.reactions.clear();
        inbound.ingest_resynced(&envelope, id).await.unwrap();
        assert!(
            service
                .message_repo
                .ensure_reaction_scope_resolved(&scope, &author, 100)
                .is_err(),
            "an empty server projection cannot invent complete legacy authority"
        );
        assert!(
            service
                .get_message_local(&message.id)
                .unwrap()
                .unwrap()
                .reaction_counts
                .is_empty(),
            "quarantined old projection stays masked"
        );
    }
}
