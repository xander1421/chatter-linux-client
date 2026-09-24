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
use super::MessageService;

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
        // Own messages are skipped only on the LIVE path, where they are
        // already on screen from the optimistic send. On a backfill they must
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

        if !self.persist(&mut message, channel_id) {
            return Ingested::Dropped;
        }

        let _ = self.event_bus().domain_sender().send(DomainEvent::MessageEdited {
            message_id,
            new_content,
            channel_id,
            conversation_type,
            target_sent_timestamp,
        });

        Ingested::Stored
    }

    /// Decrypt + convert, without touching storage.
    ///
    /// `skip_own` drops messages this user authored — right for the live
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
        let sender_id: Option<UserId> = if proto_msg.author_id.is_empty() {
            None
        } else {
            proto_msg
                .author_id
                .parse::<uuid::Uuid>()
                .ok()
                .map(UserId::from)
        };

        if skip_own {
            // Already on screen from the optimistic send, and we cannot
            // decrypt our own sender-key ciphertext anyway.
            if let (Some(sid), Ok(current)) = (
                sender_id.as_ref(),
                self.messages.settings_repo.current_user_id(),
            ) {
                if *sid == current {
                    return Err(Ingested::OwnMessage);
                }
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
                    tracing::warn!(%channel_id, %sid, error = %es, "Failed to decrypt channel message");
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

        Ok(message)
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
