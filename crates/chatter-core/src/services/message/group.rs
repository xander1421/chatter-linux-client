//! Group message operations.
//!
//! Sender key distribution, group mailbox announcements, and N-way fanout delivery.

use crate::domain::ids::{ChannelId, UserId};
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::message_client::MessageClient;
use crate::storage::repositories::settings_repo::keys;
use crate::util::time::TimestampNanos;

use super::{MessageError, MessageService};

impl MessageService {
    /// Distribute a sender key distribution message to channel members.
    ///
    /// Implements NDSS '21 pairwise distribution:
    /// - Each member receives SKDM + our mailbox announcement
    /// - Encrypted pairwise via Double Ratchet (Sealed Sender envelope)
    /// - Deposited in the member's device inboxes UAK-authorized, like a 1:1 DM.
    ///   Channel members already know each other, so this spends no anonymous
    ///   mailbox token (OPEN-ITEMS §120).
    pub async fn distribute_sender_key(
        &self,
        auth_client: &mut AuthClient,
        message_client: &mut MessageClient,
        channel_id: &ChannelId,
        skdm: &crate::proto::content_v1::SenderKeyDistribution,
    ) -> Result<(), MessageError> {
        use crate::proto::content_v1::{self, content};
        use prost::Message;

        // Get our user ID + current device id (so we don't ship SKDM to
        // ourselves on this same device — we already have our own sender key).
        let our_user_id = self.settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .map_err(MessageError::Storage)?
            .ok_or(MessageError::NotLoggedIn)?;
        let our_device_id = self.settings_repo
            .get::<u32>(keys::CURRENT_DEVICE_ID)
            .map_err(MessageError::Storage)?
            .ok_or(MessageError::NotLoggedIn)?;

        // Fan SKDM to ALL members including our own user's OTHER linked
        // devices. Own device on this client is the only one excluded —
        // multi-device sync requires every other device that might receive
        // this channel's messages to hold our sender key.
        let members = self.channel_repo.list_members(channel_id).unwrap_or_default();
        let fanout_user_ids: Vec<String> = members
            .iter()
            .map(|m| m.user_id.to_string())
            .collect();

        if fanout_user_ids.is_empty() {
            tracing::debug!(
                channel_id = %channel_id,
                "Skipping sender key distribution (no channel members)"
            );
            return Ok(());
        }

        // Get our mailbox public key for this channel (if available)
        let ephemeral_reply_to: Option<Vec<u8>> = self.group_mailbox_manager
            .load_group_state(channel_id)
            .ok()
            .flatten()
            .and_then(|state| state.our_mailbox)
            .map(|mailbox| mailbox.public_key.to_vec());
        let has_mailbox = ephemeral_reply_to.is_some();

        // Build the Content proto with SKDM + our mailbox announcement
        let timestamp = TimestampNanos::now().as_u64();

        let mut skdm_content = content_v1::Content {
            ephemeral_reply_to,
            timestamp,
            padding: vec![],
            content: Some(content::Content::SenderKey(skdm.clone())),
        };
        Self::pad_content_to_bucket(&mut skdm_content);

        let plaintext = skdm_content.encode_to_vec();

        // Distribute to each member's devices pairwise via Sealed Sender.
        let mut success_count = 0;
        let mut error_count = 0;

        for member_id in &fanout_user_ids {
            let is_self = member_id == &our_user_id;

            // Sender keys are per (user, device), so every device gets its own
            // ciphertext. Our own current device already holds the sender key.
            let outcome = self
                .deposit_sealed_to_all_devices(
                    auth_client,
                    message_client,
                    member_id,
                    &plaintext,
                    is_self.then_some(our_device_id),
                )
                .await;
            success_count += outcome.delivered;
            if outcome.delivered == 0 && !is_self {
                error_count += 1;
            }
        }

        tracing::info!(
            channel_id = %channel_id,
            total_members = fanout_user_ids.len(),
            success = success_count,
            errors = error_count,
            has_mailbox = has_mailbox,
            "Distributed sender key via pairwise sealed sender"
        );

        // Consider it success if at least one device received the SKDM (or
        // the only "member" is ourselves on this one device and no fanout is needed).
        let non_self_members = fanout_user_ids.iter().filter(|u| *u != &our_user_id).count();
        if success_count == 0 && non_self_members > 0 {
            return Err(MessageError::EncryptionFailed(
                "Failed to distribute SKDM to any member".into()
            ));
        }

        Ok(())
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
        // device, malicious peer) can't leak memory. Messages older than TTL
        // are dropped, and the whole map is capped at MAX_PENDING entries —
        // oldest-first eviction. The recipient loses only messages whose key
        // genuinely never came; anything whose SKDM lands within the window
        // still self-heals.
        const TTL: std::time::Duration = std::time::Duration::from_secs(300);
        const MAX_PENDING: usize = 512;
        let now = std::time::Instant::now();

        let mut guard = self.pending_group_decrypts.lock();

        // Expire stale entries first.
        for msgs in guard.values_mut() {
            msgs.retain(|(parked_at, _)| now.duration_since(*parked_at) < TTL);
        }
        guard.retain(|_, msgs| !msgs.is_empty());

        guard
            .entry((channel_id, sender_id, device_id))
            .or_default()
            .push((now, proto_msg));

        // Enforce the global cap by evicting the globally-oldest parked message
        // until back within budget.
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
                        v.remove(0);
                        total -= 1;
                    }
                }
                None => break,
            }
        }
        guard.retain(|_, msgs| !msgs.is_empty());
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

        let pending: Vec<chat_v1::Message> = {
            let mut guard = self.pending_group_decrypts.lock();
            guard
                .remove(&(channel_id, sender_id, device_id))
                .unwrap_or_default()
                .into_iter()
                .map(|(_parked_at, msg)| msg)
                .collect()
        };

        if pending.is_empty() {
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
            match self.signal_service.decrypt_from_group(&sender_id, device_id, &group_encrypted).await {
                Ok(plaintext) => {
                    use prost::Message as ProstMessage;
                    // The sender-key plaintext is a `content_v1::Content`
                    // wrapping a `DataMessage` (see message/core.rs send path
                    // and the streaming decrypt in client/channels.rs) — NOT a
                    // `chat_v1::message::Content`. Decoding it as the wire
                    // Content silently yielded r#type=None and dropped every
                    // healed message; mirror the stream path exactly instead.
                    let dm = match crate::proto::content_v1::Content::decode(plaintext.as_slice())
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

                    // Persist incoming attachment pointers so the receiver can
                    // download + decrypt (parity with the stream path).
                    if !dm.attachments.is_empty() {
                        let attachment_service = crate::services::attachment::AttachmentService::new_local(
                            self.attachment_repo.clone(),
                        );
                        attachment_service.create_from_pointers(&dm.attachments);
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
                    patched.content = Some(chat_v1::message::Content {
                        r#type: Some(new_type),
                    });
                    // Group decrypt retry path — channel_id is the
                    // current retry target. Derive ConversationType so
                    // identified groups stay tagged Channel.
                    let conv_type = crate::services::message::conversion::conversation_type_for_channel(
                        &self.channel_repo,
                        &channel_id,
                    );
                    match proto_to_domain_message_with_sender(&patched, Some(sender_id), conv_type) {
                        Ok(message) => {
                            // Persist so the healed message survives a channel
                            // switch, then surface it (parity with stream path).
                            match self.get_message_local(&message.id) {
                                Ok(Some(_)) => { let _ = self.update_message(&message); }
                                _ => { let _ = self.store_message_local(&message); }
                            }
                            let _ = self.event_bus.domain_sender().send(
                                DomainEvent::MessageReceived { message },
                            );
                        }
                        Err(e) => tracing::warn!(error = %e, "Retry: conversion failed"),
                    }
                }
                Err(e) => tracing::warn!(
                    sender_id = %sender_id,
                    device_id,
                    error = %e,
                    "Retry decrypt still failed after SKDM"
                ),
            }
        }
    }

    /// Ask every other current member for their SKDM.
    ///
    /// Called right after joining an encrypted channel so we can start
    /// decrypting messages the existing members send — without this step
    /// their first message after our join would fail the no-sender-key
    /// path and sit in the pending-decrypt queue until a manual retry.
    /// Individual request failures are logged but do not abort the loop;
    /// missing peers will heal on first decrypt miss.
    pub async fn request_skdms_from_channel_members(
        &self,
        channel_id: &ChannelId,
    ) -> Result<(), MessageError> {
        let current_user = self.settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .map_err(MessageError::Storage)?
            .ok_or(MessageError::NotLoggedIn)?;
        let members = self.channel_repo.list_members(channel_id).unwrap_or_default();
        for member in &members {
            if member.user_id.to_string() == current_user {
                continue;
            }
            if let Err(e) = self.send_sender_key_request(channel_id, &member.user_id).await {
                tracing::warn!(
                    channel_id = %channel_id,
                    member_id = %member.user_id,
                    error = %e,
                    "Failed to request SKDM from existing member"
                );
            }
        }
        Ok(())
    }

    /// Distribute sender key to channel members (simplified API - no client params).
    pub async fn distribute_skdm(
        &self,
        channel_id: &ChannelId,
        skdm: &crate::proto::content_v1::SenderKeyDistribution,
    ) -> Result<(), MessageError> {
        let mut auth_client = self.auth_client();
        let mut message_client = self.message_client();

        self.distribute_sender_key(&mut auth_client, &mut message_client, channel_id, skdm)
            .await
    }

    /// Send SKDM to a single member (for when a new member joins).
    ///
    /// This is called when we receive a MemberAdded event - we send our
    /// existing SKDM to the new member so they can decrypt our messages.
    pub async fn send_skdm_to_member(
        &self,
        channel_id: &ChannelId,
        member_id: &UserId,
        skdm: &crate::proto::content_v1::SenderKeyDistribution,
    ) -> Result<(), MessageError> {
        use crate::proto::content_v1::{self, content};
        use prost::Message;

        let mut auth_client = self.auth_client();

        // Get our mailbox public key for this channel (if available)
        let ephemeral_reply_to: Option<Vec<u8>> = self.group_mailbox_manager
            .load_group_state(channel_id)
            .ok()
            .flatten()
            .and_then(|state| state.our_mailbox)
            .map(|mailbox| mailbox.public_key.to_vec());

        // Build the Content proto with SKDM + our mailbox announcement
        let timestamp = TimestampNanos::now().as_u64();

        let mut skdm_content = content_v1::Content {
            ephemeral_reply_to,
            timestamp,
            padding: vec![],
            content: Some(content::Content::SenderKey(skdm.clone())),
        };
        Self::pad_content_to_bucket(&mut skdm_content);

        let plaintext = skdm_content.encode_to_vec();
        let member_id_str = member_id.to_string();

        // Per-device sealed deposit, UAK-authorized: sender keys are keyed by
        // (user, device), so each device needs its own ciphertext.
        let mut message_client = self.message_client();
        let outcome = self
            .deposit_sealed_to_all_devices(&mut auth_client, &mut message_client, &member_id_str, &plaintext, None)
            .await;

        if outcome.delivered == 0 {
            return Err(MessageError::EncryptionFailed(
                "SKDM delivery to new member failed on every device".into()
            ));
        }

        tracing::info!(
            channel_id = %channel_id,
            member_id = %member_id,
            "Sent SKDM to new channel member via sealed sender"
        );

        Ok(())
    }

    /// Create a sender key for a channel (generates or retrieves existing SKDM).
    ///
    /// This creates or retrieves our sender key for the specified channel.
    /// Used when responding to sender key requests.
    pub async fn create_sender_key_for_channel(
        &self,
        channel_id: &ChannelId,
    ) -> Result<crate::proto::content_v1::SenderKeyDistribution, MessageError> {
        self.signal_service
            .create_sender_key(channel_id)
            .await
            .map_err(|e| MessageError::EncryptionFailed(format!("Failed to create sender key: {}", e)))
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
        use crate::proto::content_v1::{self, content, SenderKeyRequest};
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
            .deposit_sealed_to_all_devices(&mut auth_client, &mut message_client, &target_id_str, &plaintext, None)
            .await;

        if outcome.delivered == 0 {
            return Err(MessageError::EncryptionFailed(
                "SenderKeyRequest failed to reach any target device".into()
            ));
        }

        tracing::info!(
            channel_id = %channel_id,
            target_user_id = %target_user_id,
            "Sent sender key request via sealed sender (multi-device fanout)"
        );

        Ok(())
    }

    /// Pad a Content proto to the nearest standard bucket size for traffic analysis protection.
    ///
    /// The server validates that sealed sender ciphertexts match one of these plaintext bucket
    /// sizes: 256, 512, 1024, 4096 bytes. Without padding, message sizes leak event type info.
    fn pad_content_to_bucket(content: &mut crate::proto::content_v1::Content) {
        use prost::Message;
        const BUCKETS: [usize; 4] = [256, 512, 1024, 4096];

        // Get current size without padding
        content.padding = vec![];
        let base_size = content.encoded_len();

        // Find smallest bucket that fits
        let target = BUCKETS.iter().copied()
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
