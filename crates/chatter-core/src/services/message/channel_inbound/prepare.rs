use super::*;

// Scope includes the complete envelope digest, so a new edit or repaired ciphertext
// is immediately eligible. Bounded volatile state only delays work: server recovery
// and message-sync checkpoints remain pending until durable handling succeeds.
pub(crate) type DecryptFailureGate =
    std::collections::HashMap<(ChannelId, UserId, u32, [u8; 32]), (std::time::Instant, u32)>;

/// Gated attempts at a message a held sender key cannot open before it is
/// given up as a delivery issue: with the gate's 30 s doubling backoff the
/// resend has had about 7.5 minutes to arrive.
pub(super) const RESEND_GIVE_UP_ATTEMPTS: u32 = 5;

/// Record one failed decrypt in the gate: the next attempt waits 30 s,
/// doubling per failure up to 15 minutes. Returns the failures so far.
pub(super) fn record_decrypt_failure(
    failures: &mut DecryptFailureGate,
    key: (ChannelId, UserId, u32, [u8; 32]),
    now: std::time::Instant,
) -> u32 {
    // Evict one oldest deadline, never clear every sender's gate.
    if failures.len() >= 512 && !failures.contains_key(&key) {
        if let Some(oldest) = failures.iter().min_by_key(|(_, v)| v.0).map(|(k, _)| *k) {
            failures.remove(&oldest);
        }
    }
    let entry = failures.entry(key).or_insert((now, 0));
    let delay = std::time::Duration::from_secs(30)
        .saturating_mul(1u32 << entry.1.min(10))
        .min(std::time::Duration::from_secs(15 * 60));
    *entry = (now + delay, entry.1.saturating_add(1));
    entry.1
}

impl ChannelInbound {
    /// Decrypt + convert, adopting authenticated profile keys on the way.
    ///
    /// `skip_own` drops messages THIS device authored — right for the live
    /// stream (already rendered optimistically), wrong for a backfill.
    ///
    /// `Err(outcome)` is the non-message result (own message, deferred on a
    /// missing sender key, or dropped).
    pub(super) async fn prepare(
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
                    return Err(Ingested::RetryableFailure);
                }
            }
        }

        // A deleted send whose canonical id never came back, recognised by
        // its exact request key: tombstoned and dropped.
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
        match Self::drop_blocked_sender(&self.messages, proto_msg, channel_id) {
            Ok(true) => return Err(Ingested::Dropped),
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(%channel_id, %error, "Failed to check blocked channel sender");
                return Err(Ingested::RetryableFailure);
            }
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
            match self
                .messages
                .channel_decrypt_failures
                .lock()
                .get(&failure_key)
            {
                // Already given up and marked. Retrying restarted the count,
                // so each re-fetch failed again and the channel's checkpoint
                // never advanced. The entry lives in memory only: after a
                // restart (or a gate eviction) the message gets another
                // bounded round of resend requests, and the marker dedups.
                Some((_, attempts)) if *attempts >= RESEND_GIVE_UP_ATTEMPTS => {
                    return Err(Ingested::DeliveryIssue);
                }
                Some((due, _)) if std::time::Instant::now() < *due => {
                    return Err(Ingested::RetryableFailure);
                }
                _ => {}
            }

            match self
                .messages
                .signal_service
                .decrypt_from_group(&sid, device_id, &group_encrypted)
                .await
            {
                Ok(plaintext) => {
                    let dm =
                        match crate::network::validation::protovalidate::decode_inbound_content(
                            plaintext.as_slice(),
                            "channel",
                        )
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
                    // The sender's verified (revision, state_hash): fork or
                    // staleness check against ours.
                    self.messages.check_group_stamp(&channel_id, &dm);

                    if current_user != Some(sid)
                        && let Some(key) = dm
                            .profile_key
                            .as_deref()
                            .and_then(|k| <[u8; 32]>::try_from(k).ok())
                    {
                        let Some(us) = current_user else {
                            return Err(Ingested::RetryableFailure);
                        };
                        match self
                            .messages
                            .user_repo
                            .set_friend_profile_key(&us, &sid, &key)
                        {
                            Ok(true) => {
                                if let Err(error) = self.users.get_user_profile(&sid).await {
                                    tracing::warn!(%sid, %error, "Could not refresh channel sender profile");
                                }
                            }
                            Ok(false) => {
                                // A prior refresh may have failed after the key
                                // was stored. Next contact retries, throttled
                                // only after a successful profile fetch.
                                if let Err(error) = self.users.refresh_profile_if_stale(&sid).await
                                {
                                    tracing::warn!(%sid, %error, "Could not refresh channel sender profile");
                                }
                            }
                            Err(error) => {
                                tracing::warn!(%sid, %error, "Could not store channel sender profile key");
                                return Err(Ingested::RetryableFailure);
                            }
                        }
                    }

                    inbound_quote = dm.quote.clone();

                    if !dm.attachments.is_empty()
                        && let Err(error) = self.attachments.create_from_pointers(&dm.attachments)
                    {
                        tracing::warn!(%error, "Failed to store attachment records");
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
                    let attempts = record_decrypt_failure(
                        &mut self.messages.channel_decrypt_failures.lock(),
                        failure_key,
                        std::time::Instant::now(),
                    );
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
                            // Nothing is asked of a retired device, so there is
                            // no repair attempt to bound: a re-fetch drops it
                            // again rather than reporting a retryable failure.
                            self.messages
                                .channel_decrypt_failures
                                .lock()
                                .remove(&failure_key);
                            tracing::debug!(%channel_id, %sid, device_id, message_id = %proto_msg.id,
                                "Channel message from a retired device is unrecoverable; dropped");
                            self.messages
                                .record_channel_delivery_issue(channel_id, sid, proto_msg);
                            return Err(Ingested::DeliveryIssue);
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
                    // Only the sender can give it back, and it answers only
                    // from a 24h send log. Holding the sync checkpoint until it
                    // does re-fetched the channel on every sync forever, so
                    // after a bounded number of gated attempts the gap is shown
                    // as a delivery issue and the message dropped; a resend
                    // that does arrive is still filed by its (sender, sent_at).
                    // The gate entry stays, at the give-up count, so later
                    // re-fetches return here without decrypting again.
                    if attempts >= RESEND_GIVE_UP_ATTEMPTS {
                        tracing::info!(%channel_id, %sid, device_id, message_id = %proto_msg.id,
                            "Group message still undecryptable after asking for a resend; recorded as a delivery issue");
                        self.messages
                            .record_channel_delivery_issue(channel_id, sid, proto_msg);
                        return Err(Ingested::DeliveryIssue);
                    }
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

        Ok(message)
    }

    /// The deposit of a send this account deleted before its canonical id
    /// came back, recognised exactly by its request key and tombstoned
    /// (`channel_send::settle_cancelled_deposit`).
    pub(super) fn cancelled_own_post(
        &self,
        proto_msg: &chat_v1::Message,
        channel_id: ChannelId,
    ) -> Result<bool, crate::error::StorageError> {
        if proto_msg.send_request_key.is_empty() {
            return Ok(false);
        }
        let Ok(owner) = self.messages.settings_repo.current_user_id() else {
            return Ok(false);
        };
        super::super::channel_send::settle_cancelled_deposit(
            &self.messages.database,
            owner,
            channel_id,
            proto_msg,
        )
    }

    fn current_device_id(&self) -> Option<u32> {
        self.messages
            .settings_repo
            .get::<u32>(crate::storage::repositories::settings_repo::keys::CURRENT_DEVICE_ID)
            .ok()
            .flatten()
    }
}
