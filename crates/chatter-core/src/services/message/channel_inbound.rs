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
}

/// Shared decrypt → persist → announce path for channel messages.
pub struct ChannelInbound {
    messages: Arc<MessageService>,
    attachments: Arc<AttachmentService>,
}

impl ChannelInbound {
    pub fn new(messages: Arc<MessageService>, attachments: Arc<AttachmentService>) -> Self {
        Self { messages, attachments }
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
        let mut message = match self.prepare(proto_msg, channel_id, emit).await {
            Ok(m) => m,
            Err(outcome) => return outcome,
        };

        if !self.persist(&mut message, channel_id) {
            return Ingested::Dropped;
        }

        if emit {
            let _ = self
                .event_bus()
                .domain_sender()
                .send(DomainEvent::MessageReceived { message });
        }

        Ingested::Stored
    }

    /// Same path, for a message the server says was EDITED. Persists the new
    /// body and announces `MessageEdited` so an open conversation patches the
    /// bubble in place instead of appending a duplicate.
    ///
    /// INFRA-4: nothing consumed `message_updated` before, so an edit made by
    /// another member never appeared on this client at all.
    pub async fn ingest_edit(
        &self,
        proto_msg: &chat_v1::Message,
        channel_id: ChannelId,
    ) -> Ingested {
        let mut message = match self.prepare(proto_msg, channel_id, true).await {
            Ok(m) => m,
            Err(outcome) => return outcome,
        };
        message.is_edited = true;

        let new_content = message.content.clone();
        let conversation_type = message.conversation_type.clone();
        let message_id = message.id;
        let target_sent_timestamp = message.sent_at;
        let author_id = message.sender_id;

        if !self.persist(&mut message, channel_id) {
            return Ingested::Dropped;
        }

        // An edit with no known author cannot be matched to a bubble.
        if let Some(author_id) = author_id {
            let _ = self.event_bus().domain_sender().send(DomainEvent::MessageEdited {
                message_id,
                new_content,
                channel_id,
                conversation_type,
                author_id,
                target_sent_timestamp,
            });
        }

        Ingested::Stored
    }

    /// One undeleted message from a resync page (`get_message_changes`).
    ///
    /// Errors only for a bad message id or a local storage failure. Whatever
    /// the message's own outcome — stored, parked for its sender key, unusable
    /// — the resync goes on to the next one: a parked message already has its
    /// SKDM request out (and, on expiry, a resend request), and an unusable one
    /// was handled where it failed. Stopping kept the stream cursor in place,
    /// so one undecryptable message made every reconnect resync again and
    /// stalled every channel behind it.
    pub async fn ingest_resynced(
        &self,
        proto_msg: &chat_v1::Message,
        channel_id: ChannelId,
    ) -> Result<(), MessageError> {
        let id = proto_msg
            .id
            .parse::<crate::domain::ids::MessageId>()
            .map_err(|_| MessageError::InvalidContent("invalid resync message id".into()))?;
        if self.messages.message_repo.is_tombstoned(&id)? {
            return Ok(());
        }
        let existing = self.messages.get_message_local(&id)?;
        if existing.as_ref().is_none_or(|local| is_newer_edit(proto_msg, local)) {
            if proto_msg.edited {
                self.ingest_edit(proto_msg, channel_id).await;
            } else {
                self.ingest(proto_msg, channel_id, false).await;
            }
        }
        match self.messages.reconcile_channel_reactions(proto_msg, channel_id) {
            // A malformed reaction is this message's problem, not the resync's.
            Err(MessageError::InvalidContent(e)) => {
                tracing::warn!(%channel_id, message_id = %id, error = %e,
                    "Resync: skipping reactions that do not match their message");
                Ok(())
            }
            other => other,
        }
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
    ) -> Result<Message, Ingested> {
        // The server keeps a message this user deleted for themselves (or a
        // peer deleted for everyone); every catch-up re-fetch hands it back.
        if let Ok(id) = proto_msg.id.parse::<crate::domain::ids::MessageId>() {
            match self.messages.message_repo.is_tombstoned(&id) {
                Ok(false) => {}
                Ok(true) => return Err(Ingested::Dropped),
                Err(e) => {
                    tracing::warn!(%channel_id, message_id = %id, error = %e,
                        "Failed to check delete tombstone; dropping");
                    return Err(Ingested::Dropped);
                }
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
                if let Ok(Some(existing)) = self.messages.get_message_local(&id) {
                    if !is_newer_edit(proto_msg, &existing) {
                        return Err(Ingested::Dropped);
                    }
                }
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
                    patched.content = Some(chat_v1::message::Content { r#type: Some(new_type) });
                    patched
                }
                Err(e) => {
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
                        if self.messages.is_retired_device(sid, device_id, proto_msg.timestamp).await {
                            tracing::debug!(%channel_id, %sid, device_id, message_id = %proto_msg.id,
                                "Channel message from a retired device is unrecoverable; dropped");
                            self.messages.record_channel_delivery_issue(channel_id, sid, proto_msg);
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
                        if self.messages.should_request_sender_key(channel_id, sid, device_id) {
                            let msg_svc = self.messages.clone();
                            tokio::spawn(async move {
                                if let Err(e) = msg_svc.send_sender_key_request(&channel_id, &sid).await {
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
                            .request_group_resend(channel_id, sid, device_id, &proto, &group_encrypted.ciphertext)
                            .await;
                    });
                    return Err(Ingested::Dropped);
                }
            }
        } else {
            proto_msg.clone()
        };

        let conv_type = conversation_type_for_channel(&self.messages.channel_repo, &channel_id);
        let mut message = match proto_to_domain_message_with_sender(&decrypted, sender_id, conv_type) {
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

        Ok(message)
    }

    fn current_device_id(&self) -> Option<u32> {
        self.messages
            .settings_repo
            .get::<u32>(crate::storage::repositories::settings_repo::keys::CURRENT_DEVICE_ID)
            .ok()
            .flatten()
    }

    /// Upsert, preserving attachments and reactions already stored locally.
    fn persist(&self, message: &mut Message, channel_id: ChannelId) -> bool {
        match self.messages.get_message_local(&message.id) {
            Ok(Some(existing)) => {
                if message.attachments.is_empty() {
                    message.attachments = existing.attachments;
                }
                if message.reaction_counts.is_empty() {
                    message.reaction_counts = existing.reaction_counts;
                }
                // Read state is local: the server copy always converts as
                // unread, so every catch-up re-fetch of a message the user had
                // already read flipped it back to unread.
                message.is_read |= existing.is_read;
                // Likewise a local soft-delete: the server copy never says deleted.
                message.is_deleted |= existing.is_deleted;
                if let Err(e) = self.messages.update_message(message) {
                    tracing::warn!(%channel_id, message_id = %message.id, error = %e,
                        "Failed to update channel message");
                    return false;
                }
            }
            Ok(None) => {
                if let Err(e) = self.messages.store_message_local(message) {
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
        true
    }
}

/// A new edit has fresh ciphertext; only duplicate/older versions reuse the
/// consumed sender key. This comparison also avoids replaying an old edit.
pub(crate) fn is_newer_edit(proto: &chat_v1::Message, existing: &Message) -> bool {
    proto.edited && proto.edited_at > 0 && proto.edited_at > existing.edited_at
        .and_then(|time| time.timestamp_nanos_opt())
        .and_then(|ns| u64::try_from(ns).ok()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ChatterClient;
    use crate::domain::message::ConversationType;

    #[test]
    fn a_new_edit_is_not_a_duplicate_of_the_original() {
        let mut original = Message::new_text(
            ChannelId::new(), ConversationType::Channel, None, "original".into(), 1_000_000,
        );
        let edit = chat_v1::Message { edited: true, edited_at: 2_000_000, ..Default::default() };
        assert!(is_newer_edit(&edit, &original));
        original.edited_at = Some(chrono::DateTime::from_timestamp_nanos(2_000_000));
        assert!(!is_newer_edit(&edit, &original));
        let older = chat_v1::Message { edited_at: 1_000_000, ..edit.clone() };
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
            .server_url("http://localhost:1")
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

        assert_eq!(inbound.ingest(&proto, channel_id, false).await, Ingested::Stored);
        assert_eq!(unread(), 1, "positive control: a received message starts unread");

        client.mark_conversation_read(&channel_id, &ConversationType::Channel).unwrap();
        assert_eq!(inbound.ingest(&proto, channel_id, false).await, Ingested::Stored);
        assert_eq!(unread(), 0);
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
            .server_url("http://localhost:1")
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
            let m = proto_to_domain_message_with_sender(&proto, Some(sender), ConversationType::Channel)
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
        assert_eq!(unread(), 1, "positive control: someone else's message is unread");
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
            .server_url("http://localhost:1")
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

        assert_eq!(inbound.ingest(&proto, channel_id, false).await, Ingested::Stored);
        assert!(client.messages().get_message_local(&message_id).unwrap().is_some(),
            "positive control: the message was stored");

        client.messages().delete_message_local(&message_id).unwrap();
        inbound.ingest(&proto, channel_id, false).await;
        assert!(client.messages().get_message_local(&message_id).unwrap().is_none(),
            "a re-fetched copy resurrected a deleted message");
    }
}
