use super::*;

impl MessageService {
    /// Whether to send a `SenderKeyRequest` for `(channel, sender, device)` now.
    ///
    /// One outstanding request per key, with exponential backoff: a history
    /// sync that parks twenty of one sender's messages used to fire twenty
    /// requests (and twenty prekey fetches, which tripped the server's per-target
    /// fetch limiter), and the sender answered every one. The gate is lifted
    /// when that key's SKDM arrives (`retry_pending_group_decrypts`).
    pub fn should_request_sender_key(
        &self,
        channel_id: ChannelId,
        sender_id: UserId,
        device_id: u32,
    ) -> bool {
        sender_key_request_due(
            &mut self.sender_key_requests.lock(),
            (channel_id, sender_id, device_id),
            std::time::Instant::now(),
        )
    }

    /// Queue a group-encrypted proto whose sender key isn't stored yet.
    ///
    /// Call site: inbox stream decrypt fails with "no sender key" → enqueue
    /// here, then fire a `SenderKeyRequest`. The matching
    /// `SenderKeyReceived` event on the bus should trigger
    /// [`retry_pending_group_decrypts`].
    pub fn defer_pending_group_decrypt(
        &self,
        channel_id: ChannelId,
        sender_id: UserId,
        device_id: u32,
        proto_msg: crate::proto::chat_v1::Message,
    ) {
        // Bound the buffer so an SKDM that never arrives (kicked sender, lost
        // device, malicious peer) can't leak memory. Each message waits
        // PENDING_GROUP_DECRYPT_TTL for its SKDM, then is handed to
        // `expire_pending_group_decrypt` (which asks the sender to resend it);
        // the whole map is capped at MAX_PENDING entries — oldest-first
        // eviction.
        const MAX_PENDING: usize = 512;
        let now = std::time::Instant::now();
        let message_id = proto_msg.id.clone();

        let mut guard = self.pending_group_decrypts.lock();

        // A history sync can hand us the same message more than once. Park it
        // once: a second copy would decrypt as libsignal's DuplicatedMessage
        // after the first consumed the message key.
        let parked = guard.entry((channel_id, sender_id, device_id)).or_default();
        if parked
            .iter()
            .any(|(_, m)| m.id == proto_msg.id && m.content == proto_msg.content)
        {
            return;
        }
        parked.push((now, proto_msg));

        // Enforce the global cap by evicting the globally-oldest parked message
        // until back within budget.
        let mut evicted = Vec::new();
        let mut total: usize = guard.values().map(|v| v.len()).sum();
        while total > MAX_PENDING {
            let oldest_key = guard
                .iter()
                .filter(|(_, v)| !v.is_empty())
                .min_by_key(|(_, v)| v[0].0)
                .map(|(k, _)| *k);
            match oldest_key {
                Some(k) => {
                    if let Some(v) = guard.get_mut(&k) {
                        evicted.push((k, v.remove(0).1));
                        total -= 1;
                    }
                }
                None => break,
            }
        }
        guard.retain(|_, msgs| !msgs.is_empty());
        drop(guard);

        // An evicted message is not lost silently: its sender is asked to
        // resend it, exactly as when its wait runs out. Channel messages are
        // resendable (the sender keeps them 24h), which is where Signal-Desktop
        // sends a DecryptionErrorMessage rather than recording a delivery
        // issue (`handleRetry.ts`: `ContentHint.RESENDABLE`). Rate-limited per
        // sender so one flooding member cannot turn the cap into a DEM storm.
        let now = std::time::Instant::now();
        for ((channel_id, sender_id, device_id), proto_msg) in evicted {
            if !eviction_resend_due(&mut self.eviction_resends.lock(), sender_id, now) {
                tracing::warn!(%channel_id, %sender_id, device_id, message_id = %proto_msg.id,
                    "Parked group message evicted over the cap and the sender's resend budget; dropped");
                continue;
            }
            let svc = self.clone();
            tokio::spawn(async move {
                svc.resend_parked(channel_id, sender_id, device_id, proto_msg)
                    .await;
            });
        }

        let svc = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(PENDING_GROUP_DECRYPT_TTL).await;
            svc.expire_pending_group_decrypt(channel_id, sender_id, device_id, &message_id)
                .await;
        });
    }

    /// A parked message whose SKDM did not arrive within
    /// [`PENDING_GROUP_DECRYPT_TTL`]: stop waiting and ask its sender to resend
    /// it pairwise, as for a message the SKDM could not open. Dropping it here
    /// silently lost it for good when the SKDM came late. A no-op when the
    /// SKDM already drained it (`retry_pending_group_decrypts`).
    pub(crate) async fn expire_pending_group_decrypt(
        &self,
        channel_id: ChannelId,
        sender_id: UserId,
        device_id: u32,
        message_id: &str,
    ) {
        let expired = {
            let mut guard = self.pending_group_decrypts.lock();
            let key = (channel_id, sender_id, device_id);
            let Some(parked) = guard.get_mut(&key) else {
                return;
            };
            let (expired, kept): (Vec<_>, Vec<_>) = std::mem::take(parked)
                .into_iter()
                .partition(|(_, m)| m.id == message_id);
            *parked = kept;
            if parked.is_empty() {
                guard.remove(&key);
            }
            expired
        };
        let Some((_, proto_msg)) = expired.into_iter().next() else {
            return;
        };
        tracing::info!(%channel_id, %sender_id, device_id, %message_id,
            "Sender key never arrived for a parked message; requesting a resend");
        self.resend_parked(channel_id, sender_id, device_id, proto_msg)
            .await;
    }

    /// Hand a parked (still sender-key encrypted) message that left the queue
    /// undecrypted to [`Self::request_group_resend`].
    async fn resend_parked(
        &self,
        channel_id: ChannelId,
        sender_id: UserId,
        device_id: u32,
        proto_msg: crate::proto::chat_v1::Message,
    ) {
        use crate::proto::chat_v1::message::content::Type;
        use crate::proto::content_v1::GroupEncrypted;
        use prost::Message as _;

        let Some(Type::Encrypted(enc)) = proto_msg.content.as_ref().and_then(|c| c.r#type.as_ref())
        else {
            return;
        };
        let Ok(group_encrypted) = GroupEncrypted::decode(enc.ciphertext.as_slice()) else {
            return;
        };
        self.request_group_resend(
            channel_id,
            sender_id,
            device_id,
            &proto_msg,
            &group_encrypted.ciphertext,
        )
        .await;
    }

    /// Drain deferred group messages for `(channel, sender, device)` and
    /// re-run decrypt using the now-stored sender key. Successes emit
    /// `DomainEvent::MessageReceived`; permanent failures are dropped.
    pub async fn retry_pending_group_decrypts(
        &self,
        channel_id: ChannelId,
        sender_id: UserId,
        device_id: u32,
    ) {
        use crate::events::DomainEvent;
        use crate::proto::chat_v1;
        use crate::proto::content_v1::GroupEncrypted;
        use crate::services::message::conversion::proto_to_domain_message_with_sender;

        // An SKDM reply is not proof that it supplied a missing historical key.
        // Keep request backoff until a parked message is durably persisted.

        let pending: Vec<chat_v1::Message> = {
            let mut guard = self.pending_group_decrypts.lock();
            guard
                .remove(&(channel_id, sender_id, device_id))
                .unwrap_or_default()
                .into_iter()
                .map(|(_parked_at, msg)| msg)
                .collect()
        };

        // The SKDM's stamp may have shown a peer ahead of us.
        self.sync_channel_state_if_behind(&channel_id);

        if pending.is_empty() {
            return;
        }
        // Admitted when parked; the verified state may have dropped the sender since.
        if !self.group_gate().admits(&channel_id, &sender_id) {
            tracing::warn!(%channel_id, %sender_id, count = pending.len(),
                "Dropping parked group messages: sender is no longer a verified member");
            return;
        }

        tracing::info!(
            channel_id = %channel_id,
            sender_id = %sender_id,
            device_id,
            count = pending.len(),
            "Retrying deferred group-encrypted messages after SKDM arrival"
        );

        for proto_msg in pending {
            let Some(chat_v1::message::content::Type::Encrypted(enc)) =
                proto_msg.content.as_ref().and_then(|c| c.r#type.as_ref())
            else {
                continue;
            };
            use prost::Message as _;
            let group_encrypted = match GroupEncrypted::decode(enc.ciphertext.as_slice()) {
                Ok(ge) => ge,
                Err(e) => {
                    tracing::warn!(error = %e, "Retry: failed to decode GroupEncrypted");
                    continue;
                }
            };
            match self
                .signal_service
                .decrypt_from_group(&sender_id, device_id, &group_encrypted)
                .await
            {
                Ok(plaintext) => {
                    // The sender-key plaintext is a `content_v1::Content`
                    // wrapping a `DataMessage` (see message/core.rs send path
                    // and the streaming decrypt in client/channels.rs) — NOT a
                    // `chat_v1::message::Content`. Decoding it as the wire
                    // Content silently yielded r#type=None and dropped every
                    // healed message; mirror the stream path exactly instead.
                    let dm =
                        match crate::network::validation::protovalidate::decode_inbound_content(
                            plaintext.as_slice(),
                            "channel_retry",
                        )
                        .ok()
                        .and_then(|c| c.content)
                        {
                            Some(crate::proto::content_v1::content::Content::DataMessage(dm)) => dm,
                            _ => {
                                tracing::warn!(
                                    sender_id = %sender_id,
                                    device_id,
                                    "Retry: decrypted plaintext is not a valid DataMessage; dropping"
                                );
                                continue;
                            }
                        };
                    if dm.channel_id.parse::<ChannelId>().ok() != Some(channel_id)
                        || dm.timestamp == 0
                        || proto_msg.channel_id.parse::<ChannelId>().ok() != Some(channel_id)
                        || proto_msg.author_id.parse::<UserId>().ok() != Some(sender_id)
                        || proto_msg.author_device_id != device_id
                        || (!dm.message_id.is_empty() && dm.message_id != proto_msg.id)
                        || ((!dm.message_id.is_empty() || !dm.thread_id.is_empty())
                            && dm.thread_id != proto_msg.parent_id)
                    {
                        tracing::warn!("Retry: authenticated channel identity mismatch");
                        continue;
                    }
                    self.check_group_stamp(&channel_id, &dm);

                    // Persist incoming attachment pointers so the receiver can
                    // download + decrypt (parity with the stream path).
                    if !dm.attachments.is_empty() {
                        let attachment_service =
                            crate::services::attachment::AttachmentService::new_local(
                                self.attachment_repo.clone(),
                            );
                        if let Err(error) = attachment_service.create_from_pointers(&dm.attachments)
                        {
                            tracing::warn!(%error, "Failed to store attachment records");
                        }
                    }

                    // Rebuild the wire proto with the decrypted text +
                    // attachment ids so `proto_to_domain_message_with_sender`
                    // picks them up (Text or Mixed), exactly as the stream path.
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
                    // Target identity comes from the authenticated DataMessage,
                    // never from the later server transport envelope.
                    patched.timestamp = dm.timestamp;
                    patched.content = Some(chat_v1::message::Content {
                        r#type: Some(new_type),
                    });
                    // Group decrypt retry path — channel_id is the
                    // current retry target. Derive ConversationType so
                    // identified groups stay tagged Channel.
                    let conv_type =
                        crate::services::message::conversion::conversation_type_for_channel(
                            &self.channel_repo,
                            &channel_id,
                        );
                    match proto_to_domain_message_with_sender(&patched, Some(sender_id), conv_type)
                    {
                        Ok(mut message) => {
                            crate::services::gateway::inbound::apply_inbound_quote(
                                &mut message,
                                dm.quote.as_ref(),
                                &self.message_repo,
                            );
                            let Some(persisted) =
                                super::super::channel_inbound::ChannelInbound::persist_message(
                                    self,
                                    &mut message,
                                    channel_id,
                                )
                            else {
                                continue;
                            };
                            {
                                let mut gate = self.sender_key_requests.lock();
                                gate.remove(&(channel_id, sender_id, device_id));
                                gate.remove(&(channel_id, sender_id, 0));
                            }
                            if persisted == super::super::channel_inbound::Persisted::New {
                                let _ = self
                                    .event_bus
                                    .domain_sender()
                                    .send(DomainEvent::MessageReceived { message });
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "Retry: conversion failed"),
                    }
                }
                // libsignal's DuplicatedMessage: the message's iteration is
                // below the chain we hold. The SKDM we got starts at the
                // sender's CURRENT iteration (libsignal
                // `create_sender_key_distribution_message`), so a message they
                // sent before distributing to us has no key on this device and
                // never will: only the sender can give us the content again.
                // Any other failure after the SKDM is equally terminal here.
                Err(e) => {
                    tracing::info!(
                        sender_id = %sender_id,
                        device_id,
                        message_id = %proto_msg.id,
                        error = %e,
                        "Parked message still undecryptable after SKDM; requesting a resend"
                    );
                    self.request_group_resend(
                        channel_id,
                        sender_id,
                        device_id,
                        &proto_msg,
                        &group_encrypted.ciphertext,
                    )
                    .await;
                }
            }
        }
    }

    /// Send a sender key request to a channel member.
    ///
    /// Fanned to every device the target has, not just the primary. Sender
    /// keys are per (user, device); whichever device currently holds the
    /// sending sender key responds with its own SKDM. Without this fanout, a
    /// request would land only in device 1's inbox and dormant multi-device
    /// users whose device 1 is offline would never reply.
    pub async fn send_sender_key_request(
        &self,
        channel_id: &ChannelId,
        target_user_id: &UserId,
    ) -> Result<(), MessageError> {
        use crate::proto::content_v1::{self, SenderKeyRequest, content};
        use prost::Message;

        let mut auth_client = self.auth_client();

        // Build the SenderKeyRequest proto
        let timestamp = TimestampNanos::now().as_u64();
        let request = SenderKeyRequest {
            channel_id: channel_id.to_string(),
            target_user_id: target_user_id.to_string(),
            timestamp,
        };

        // Wrap in Content proto
        let mut content = content_v1::Content {
            content: Some(content::Content::SenderKeyRequest(request.clone())),
            ephemeral_reply_to: None,
            timestamp,
            padding: vec![],
        };
        Self::pad_content_to_bucket(&mut content);

        let plaintext = content.encode_to_vec();
        let target_id_str = target_user_id.to_string();

        // Sealed per device, UAK-authorized (pairwise sender keys are per
        // (user, device); we need a distinct envelope for each).
        let mut message_client = self.message_client();
        let outcome = self
            .deposit_sealed_to_all_devices(
                &mut auth_client,
                &mut message_client,
                &target_id_str,
                &plaintext,
                Some(channel_id.as_bytes()),
            )
            .await;

        if outcome.delivered == 0 {
            return Err(MessageError::EncryptionFailed(
                "SenderKeyRequest failed to reach any target device".into(),
            ));
        }

        tracing::info!(
            channel_id = %channel_id,
            target_user_id = %target_user_id,
            "Sent sender key request via sealed sender (multi-device fanout)"
        );

        Ok(())
    }
}
