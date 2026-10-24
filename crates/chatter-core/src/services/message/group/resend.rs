use super::*;

impl MessageService {
    /// Sender-asserted timestamp (epoch nanos) of a group message, read from
    /// the `GroupEncrypted` envelope.
    ///
    /// That proto is the `EncryptedData` ciphertext the server stores and
    /// returns whole. Its `timestamp` sits beside the sender-key ciphertext,
    /// so a recipient that cannot decrypt still has it. The outer
    /// `Message.timestamp` is the server's receive time and is not a name
    /// for the message. `None` when the envelope does not carry one.
    pub(super) fn group_envelope_timestamp_ns(
        proto_msg: &crate::proto::chat_v1::Message,
    ) -> Option<u64> {
        use crate::proto::chat_v1::message::content::Type;
        use crate::proto::content_v1::GroupEncrypted;
        use prost::Message as _;

        let Some(Type::Encrypted(enc)) = proto_msg.content.as_ref().and_then(|c| c.r#type.as_ref())
        else {
            return None;
        };
        let ge = GroupEncrypted::decode(enc.ciphertext.as_slice()).ok()?;
        (ge.timestamp != 0).then_some(ge.timestamp)
    }

    /// RECEIVER half of Signal's sender-key retry: ask the device that sent a
    /// group message we cannot decrypt to resend it pairwise.
    ///
    /// The SKDM a sender hands us starts at its chain's CURRENT iteration, so a
    /// message it sent before we held the key ("old counter") is unreadable
    /// here for good; only the sender can give us the content again
    /// (Signal-Desktop `onRetryRequest`). One request per (message, device).
    /// The DecryptionErrorMessage names the original by the sender-asserted
    /// envelope timestamp ([`Self::group_envelope_timestamp_ns`]), the same
    /// clock the sender stored as `sent_at`.
    pub async fn request_group_resend(
        &self,
        channel_id: ChannelId,
        sender_id: UserId,
        device_id: u32,
        proto_msg: &crate::proto::chat_v1::Message,
        ciphertext: &[u8],
    ) {

        // Our own sender-key ciphertext from this very device is undecryptable
        // by design, not lost.
        let us = (
            self.settings_repo.current_user_id().ok(),
            self.settings_repo
                .get::<u32>(keys::CURRENT_DEVICE_ID)
                .ok()
                .flatten(),
        );
        if us == (Some(sender_id), Some(device_id)) {
            return;
        }
        // The clock the sender put on the envelope. The server's
        // Message.timestamp is receive time and does not match the sender's
        // sent_at, so a DecryptionErrorMessage named with it is dropped.
        let Some(named_ns) = Self::group_envelope_timestamp_ns(proto_msg) else {
            tracing::warn!(%channel_id, %sender_id, device_id, message_id = %proto_msg.id,
                "Group message envelope has no sender timestamp; not asking for a resend by server receive time");
            return;
        };
        // Already held — decrypted before (then "old counter" is a genuine
        // duplicate), or recovered by an earlier resend (stored under the
        // server id it carries, and the same (sender, sent_at)).
        let held = proto_msg
            .id
            .parse::<crate::domain::ids::MessageId>()
            .ok()
            .and_then(|id| self.message_repo.get(&id).ok().flatten())
            .is_some()
            || self
                .message_repo
                .get_by_conversation_sender_timestamp(&channel_id, &sender_id, named_ns)
                .ok()
                .flatten()
                .is_some();
        if held {
            return;
        }
        // A retired device will never answer, and the server refuses to queue
        // anything for it. The server receive time is what this check compares
        // to our device list.
        if self
            .is_retired_device(sender_id, device_id, proto_msg.timestamp)
            .await
        {
            tracing::info!(%channel_id, %sender_id, device_id, message_id = %proto_msg.id,
                "Group message from a retired device is unrecoverable; dropped");
            self.record_channel_delivery_issue(channel_id, sender_id, proto_msg);
            return;
        }
        // Recorded before the request goes out, so the answer cannot race it.
        // Keyed by the sender clock the resend's DataMessage will carry.
        self.solicited_resends
            .insert((channel_id, sender_id, named_ns / 1_000_000));
        {
            let mut asked = self.group_retry_requests.lock();
            // cleared wholesale at 4096 entries; a forgotten entry
            // costs at most one repeat request, which the sender answers again.
            if asked.len() >= 4096 {
                asked.clear();
            }
            if !asked.insert((proto_msg.id.clone(), device_id)) {
                return;
            }
        }

        let dem = match crate::services::signal::SignalService::group_decryption_error(
            ciphertext, named_ns, device_id,
        ) {
            Ok(dem) => dem,
            Err(e) => {
                tracing::warn!(%channel_id, %sender_id, device_id, error = %e,
                    "Could not build DecryptionErrorMessage for a group message");
                return;
            }
        };
        match self
            .send_decryption_error_report(&sender_id, device_id, dem)
            .await
        {
            Ok(()) => tracing::info!(
                %channel_id, %sender_id, device_id, message_id = %proto_msg.id,
                "Asked the sender to resend a group message we cannot decrypt"
            ),
            Err(MessageError::Network(e)) if e.is_unknown_destination_device() => {
                tracing::info!(%channel_id, %sender_id, device_id,
                    "Sender device is retired; dropping its parked group messages");
                self.retired_peer_devices
                    .lock()
                    .insert((sender_id, device_id), std::time::Instant::now());
                self.pending_group_decrypts
                    .lock()
                    .retain(|(_, s, d), _| (*s, *d) != (sender_id, device_id));
                self.record_channel_delivery_issue(channel_id, sender_id, proto_msg);
            }
            Err(e) => tracing::warn!(
                %channel_id, %sender_id, device_id, message_id = %proto_msg.id, error = %e,
                "Failed to send DecryptionErrorMessage for a group message"
            ),
        }
    }

    /// SENDER half of Signal's sender-key retry: `peer_id`'s device could not
    /// decrypt the group message we sent at `timestamp_ms` (the sender-asserted
    /// envelope clock, epoch millis — not the server receive time). Resend its
    /// content pairwise to that one device, as a `DataMessage` stamped with
    /// that same `sent_at` so the peer files it under the same (sender, sent_at)
    /// as the copy it could not read.
    ///
    /// Bounded like Signal-Desktop's send log: only our own messages, only for
    /// 24h, and only to a device that was a recipient of that send and whose
    /// user is still a member. A device or member added later gets nothing,
    /// and a member who left gets nothing.
    pub async fn resend_on_decryption_error(
        &self,
        peer_id: &UserId,
        peer_device_id: u32,
        timestamp_ms: u64,
    ) {
        match self
            .resend_group_message(peer_id, peer_device_id, timestamp_ms)
            .await
        {
            Ok(true) => tracing::info!(%peer_id, peer_device_id, timestamp_ms,
                "Resent a group message pairwise after the member's DecryptionErrorMessage"),
            Ok(false) => tracing::info!(%peer_id, peer_device_id, timestamp_ms,
                "Group resend request matched nothing we may resend"),
            // The device is gone: the server refused the send, or — with no
            // session, so the single-device bundle fetch came first — holds no
            // bundle for it (NotFound, signal_prekeys.go). Same verdict as the
            // receiver half above.
            Err(MessageError::Network(e))
                if e.is_unknown_destination_device()
                    || matches!(e, crate::error::NetworkError::NotFound(_)) =>
            {
                tracing::info!(%peer_id, peer_device_id, timestamp_ms,
                    "Group resend: device retired on server");
                self.retired_peer_devices
                    .lock()
                    .insert((*peer_id, peer_device_id), std::time::Instant::now());
            }
            Err(e) => tracing::warn!(%peer_id, peer_device_id, timestamp_ms, error = %e,
                "Failed to resend a group message"),
        }
    }

    async fn resend_group_message(
        &self,
        peer_id: &UserId,
        peer_device_id: u32,
        timestamp_ms: u64,
    ) -> Result<bool, MessageError> {
        let Some(plaintext) = self.group_resend_content(peer_id, peer_device_id, timestamp_ms)?
        else {
            return Ok(false);
        };
        let sealed_body = self
            .signal_service
            .encrypt_message(
                &mut self.auth_client(),
                &peer_id.to_string(),
                peer_device_id,
                &plaintext,
            )
            .await
            .map_err(|e| match e {
                // Kept typed: a NotFound here is the retired-device verdict.
                crate::error::ServiceError::Network(e) => MessageError::Network(e),
                e => MessageError::EncryptionFailed(e.to_string()),
            })?;
        self.deposit_sealed_to_device(peer_id, peer_device_id, sealed_body)
            .await?;
        Ok(true)
    }

    /// The content `peer_id`'s `peer_device_id` may have back for our group
    /// message sent at `timestamp_ms`, or `None` when the retention and
    /// original-recipient bounds of [`Self::resend_on_decryption_error`] say no.
    pub(super) fn group_resend_content(
        &self,
        peer_id: &UserId,
        peer_device_id: u32,
        timestamp_ms: u64,
    ) -> Result<Option<Vec<u8>>, MessageError> {
        use crate::domain::ids::DeviceId;
        use crate::domain::message::ConversationType;
        const RETENTION_NS: u64 = 24 * 60 * 60 * 1_000_000_000;

        let us = self.settings_repo.current_user_id()?;
        // scans our newest 1000 messages; add a (sender, sent_at
        // range) repo query if one user sends more than that within 24h.
        let Some(message) = self
            .message_repo
            .list_by_sender(&us, 1000)?
            .into_iter()
            .find(|m| {
                m.sent_at / 1_000_000 == timestamp_ms
                    && m.conversation_type == ConversationType::Channel
                    && !m.is_deleted
            })
        else {
            return Ok(None);
        };
        if TimestampNanos::now()
            .as_u64()
            .saturating_sub(message.sent_at)
            > RETENTION_NS
        {
            return Ok(None);
        }
        let receipts =
            crate::storage::repositories::message_receipt_repo::MessageReceiptRepository::new(
                self.database.clone(),
            );
        let original = receipts.targets_for_send(
            message.conversation_id,
            message.conversation_type.clone(),
            us,
            message.sent_at,
        )?;
        // Still a member = in the verified distribution set; nothing while
        // the state is failed or forked.
        let Ok(verified) = self.group_gate().for_distribution(&message.conversation_id) else {
            return Ok(None);
        };
        let current = verified_user_ids(&verified);
        let device = DeviceId(peer_device_id);
        // Our own other devices are not receipt targets, yet they read our
        // channel messages through the same sender key. Their own snapshot
        // bounds them like any member: only a device that existed at send time.
        let allowed = if *peer_id == us {
            receipts
                .own_devices_for_send(
                    message.conversation_id,
                    message.conversation_type.clone(),
                    us,
                    message.sent_at,
                )?
                .contains(&device)
        } else {
            resend_targets(&original, &current)
                .into_iter()
                .any(|(user, id)| user == *peer_id && id == device)
        };
        if !allowed {
            return Ok(None);
        }
        Ok(self.rebuild_group_content(&message, &verified))
    }

    /// User-initiated retry of a stored private channel message: the same
    /// pairwise `DataMessage` a DecryptionErrorMessage gets, to every original
    /// recipient device whose user is still a member. Nothing is posted to the
    /// channel, and the receipt snapshot is not rewritten.
    pub(in crate::services::message) async fn resend_stored_channel_message(
        &self,
        message: &crate::domain::message::Message,
    ) -> Result<usize, MessageError> {

        let us = self.settings_repo.current_user_id()?;
        let original =
            crate::storage::repositories::message_receipt_repo::MessageReceiptRepository::new(
                self.database.clone(),
            )
            .targets_for_send(
                message.conversation_id,
                message.conversation_type.clone(),
                us,
                message.sent_at,
            )?;
        let verified = self
            .group_gate()
            .for_distribution(&message.conversation_id)?;
        let targets = resend_targets(&original, &verified_user_ids(&verified));
        if targets.is_empty() {
            return Err(MessageError::InvalidContent(
                "no original recipient of this message is still in the channel".into(),
            ));
        }
        let plaintext = self
            .rebuild_group_content(message, &verified)
            .ok_or_else(|| MessageError::InvalidContent("message cannot be resent".into()))?;
        crate::services::gateway::submission::mark_submission_started()?;
        self.deliver_group_resend_to(&plaintext, &targets).await
    }

    /// Encrypt `plaintext` for each `(user, device)` and deposit that one
    /// device's inbox. A device that fails does not stop the others. `Err`
    /// only when none were reached.
    async fn deliver_group_resend_to(
        &self,
        plaintext: &[u8],
        targets: &[(UserId, crate::domain::ids::DeviceId)],
    ) -> Result<usize, MessageError> {
        let mut delivered = 0usize;
        let mut error = None;
        for (user, device) in targets {
            let sealed = match self
                .signal_service
                .encrypt_message(
                    &mut self.auth_client(),
                    &user.to_string(),
                    device.0,
                    plaintext,
                )
                .await
            {
                Ok(sealed) => sealed,
                Err(e) => {
                    super::super::server_dm::record_deposit_error(
                        &mut error,
                        MessageError::from_signal_send(e, "group resend"),
                    );
                    continue;
                }
            };
            match self.deposit_sealed_to_device(user, device.0, sealed).await {
                Ok(()) => delivered += 1,
                Err(e) => super::super::server_dm::record_deposit_error(&mut error, e),
            }
        }
        if delivered == 0 {
            return Err(error.unwrap_or_else(|| {
                MessageError::InvalidContent("resend reached no device".into())
            }));
        }
        Ok(delivered)
    }

    /// The pairwise `Content` for one of our stored channel messages: the
    /// fields the original sender-key `DataMessage` carried (`core.rs`
    /// `send_message`), rebuilt from the local row. `None` for content that
    /// is not a user message.
    fn rebuild_group_content(
        &self,
        message: &crate::domain::message::Message,
        verified: &VerifiedChannel,
    ) -> Option<Vec<u8>> {
        use crate::domain::message::MessageContent;
        use crate::proto::content_v1::{Content, DataMessage, content};
        use prost::Message as _;

        let body = match &message.content {
            MessageContent::Text(t) => t.clone(),
            MessageContent::Mixed(m) => m.text.clone().unwrap_or_default(),
            _ => return None,
        };
        let attachments = message
            .attachments
            .iter()
            .filter_map(|id| self.attachment_repo.get(id).ok().flatten())
            .filter_map(|a| a.to_pointer())
            .collect();
        let quote = message
            .reply_to
            .and_then(|id| self.message_repo.get(&id).ok().flatten())
            .and_then(|orig| {
                Some(crate::services::gateway::QuoteIntent {
                    target_author_id: orig.sender_id?,
                    target_timestamp: orig.sent_at,
                    target_preview: message.reply_to_text.clone(),
                })
            });
        let ttl = self
            .ephemeral_service
            .get_state(&message.id)
            .ok()
            .flatten()
            .map(|e| e.ttl.as_secs() as u32);

        let mut dm = DataMessage {
            body,
            attachments,
            channel_id: message.conversation_id.to_string(),
            // The sender-asserted clock (DataMessage and GroupEncrypted
            // timestamp), not the server receive time. Members file this
            // message, and this resend, under that sent_at.
            timestamp: message.sent_at,
            quote: self.build_wire_quote(quote.as_ref()),
            thread_id: message
                .thread_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
            mentioned_integrations: message
                .mentioned_integrations
                .iter()
                .map(|id| id.to_string())
                .collect(),
            targets_ai_bot: message.targets_ai_bot,
            ephemeral: ttl.is_some(),
            ephemeral_ttl_seconds: ttl.unwrap_or(0),
            // Our row's id is the server's (`send_message` stores the
            // server response), i.e. the id every other member holds.
            message_id: message.id.to_string(),
            ..Default::default()
        };
        stamp_group_data_message(&mut dm, verified);
        dm.profile_key = self.group_profile_key(verified);
        let mut content = Content {
            timestamp: dm.timestamp,
            content: Some(content::Content::DataMessage(dm)),
            ..Default::default()
        };
        Self::pad_content_to_bucket(&mut content);
        Some(content.encode_to_vec())
    }

    /// Deposit one sealed envelope in one device inbox
    /// (`SealedDelivery::NamedDevices`), UAK-authorized when we hold the
    /// peer's profile key (the same choice as `send_decryption_error_report`).
    /// A retired device comes back as `MismatchedDevices` with it `extra`
    /// (`NetworkError::is_unknown_destination_device`).
    pub(in crate::services::message) async fn deposit_sealed_to_device(
        &self,
        peer_id: &UserId,
        device_id: u32,
        sealed_body: Vec<u8>,
    ) -> Result<(), MessageError> {
        let message = crate::proto::chat_v1::SealedDeviceMessage {
            device_id,
            registration_id: 0,
            sealed_message: Some(crate::proto::chat_v1::SealedSenderMessage {
                sealed_body,
                server_key_id: self.signal_service.get_server_key_id(),
                timestamp: TimestampNanos::now_anonymized().as_u64(),
            }),
        };
        let peer = peer_id.to_string();
        let uak = self.sealed_access_key(&peer, false);
        if uak.is_none() {
            tracing::warn!(
                %peer_id,
                reason = "no stored profile key to derive the peer's UAK",
                "SEALED SENDER DEGRADED: group resend delivered identified \
                 (Basic auth) — the server WILL record us as the sender"
            );
        }
        let (_, uak_rejected) = self
            .send_sealed_batch(
                &mut self.message_client(),
                &peer,
                vec![message],
                crate::proto::chat_v1::SealedDelivery::NamedDevices,
                uak.as_ref(),
                &uuid::Uuid::new_v4().to_string(),
            )
            .await?;
        if uak_rejected {
            tracing::warn!(%peer_id, reason = "UAK from our stored profile key rejected", "SEALED SENDER DEGRADED: group resend delivered identified");
        }
        Ok(())
    }

    /// Pad a Content proto to the nearest standard bucket size for traffic analysis protection.
    ///
    /// The server validates that sealed sender ciphertexts match one of these plaintext bucket
    /// sizes: 256, 512, 1024, 4096 bytes. Without padding, message sizes leak event type info.
    pub(crate) fn pad_content_to_bucket(content: &mut crate::proto::content_v1::Content) {
        use prost::Message;
        const BUCKETS: [usize; 4] = [256, 512, 1024, 4096];

        // Get current size without padding
        content.padding = vec![];
        let base_size = content.encoded_len();

        // Find smallest bucket that fits
        let target = BUCKETS
            .iter()
            .copied()
            .find(|&b| b >= base_size)
            .unwrap_or(4096);

        if base_size >= target {
            return;
        }

        // Start with an overestimate, then trim to exact target.
        // Protobuf overhead for the padding field: 1 (tag) + varint(len) + len.
        let estimate = target.saturating_sub(base_size);
        content.padding = vec![0u8; estimate];

        while content.encoded_len() > target && !content.padding.is_empty() {
            content.padding.pop();
        }
        // If we undershot by 1 due to varint boundary, add back
        while content.encoded_len() < target {
            content.padding.push(0);
        }
    }
}
