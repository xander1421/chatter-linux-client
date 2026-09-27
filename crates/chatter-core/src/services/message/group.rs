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
        // Our user ID (the deposit leaves this device out of our own fan-out —
        // it already holds our sender key).
        let our_user_id = self.settings_repo
            .get_string(keys::CURRENT_USER_ID)
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

        let has_mailbox = self.our_group_mailbox_pk(channel_id).is_some();

        // Distribute to each member's devices pairwise via Sealed Sender.
        let mut success_count = 0;
        let mut error_count = 0;

        for member_id in &fanout_user_ids {
            let is_self = member_id == &our_user_id;
            let plaintext = self.skdm_plaintext(channel_id, skdm, member_id);

            // Sender keys are per (user, device), so every device gets its own
            // ciphertext. Our own current device already holds the sender key.
            let outcome = self
                .deposit_sealed_to_all_devices(
                    auth_client,
                    message_client,
                    member_id,
                    &plaintext,
                )
                .await;
            success_count += outcome.delivered;
            if outcome.delivered == 0 && !is_self {
                error_count += 1;
            }
            if outcome.delivered > 0 {
                if is_self {
                    self.record_own_devices_hold_sender_key(channel_id, &our_user_id).await;
                } else {
                    self.record_sender_key_delivered(channel_id, member_id);
                }
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

    /// Our mailbox public key for this channel, if we have one (NDSS '21
    /// announcement that rides every SKDM).
    fn our_group_mailbox_pk(&self, channel_id: &ChannelId) -> Option<Vec<u8>> {
        self.group_mailbox_manager
            .load_group_state(channel_id)
            .ok()
            .flatten()
            .and_then(|state| state.our_mailbox)
            .map(|mailbox| mailbox.public_key.to_vec())
    }

    /// The padded, encoded SKDM Content for one recipient: the SKDM, our
    /// mailbox announcement, and our profile key (see [`Self::skdm_profile_key`]).
    fn skdm_plaintext(
        &self,
        channel_id: &ChannelId,
        skdm: &crate::proto::content_v1::SenderKeyDistribution,
        recipient: &str,
    ) -> Vec<u8> {
        use crate::proto::content_v1::{self, content};
        use prost::Message;

        let mut skdm = skdm.clone();
        skdm.profile_key = self.skdm_profile_key(channel_id, recipient);
        let mut skdm_content = content_v1::Content {
            ephemeral_reply_to: self.our_group_mailbox_pk(channel_id),
            timestamp: TimestampNanos::now().as_u64(),
            padding: vec![],
            content: Some(content::Content::SenderKey(skdm)),
        };
        Self::pad_content_to_bucket(&mut skdm_content);
        skdm_content.encode_to_vec()
    }

    /// Our profile key for a channel member's SKDM, so they can decrypt our
    /// name and avatar without being our friend — Signal shares every group
    /// member's profile key with the group. The SKDM is pairwise-encrypted,
    /// so it reaches only this member; `None` for anyone this device does not
    /// hold as a member of `channel_id` — an SKDM also answers a
    /// `SenderKeyRequest`, which anyone can send — and for whoever the
    /// visibility setting keeps it from (ourselves, a member we blocked, a
    /// non-friend under "my contacts", everyone under "nobody"). Fails
    /// closed: a lookup error shares nothing.
    fn skdm_profile_key(&self, channel_id: &ChannelId, recipient: &str) -> Option<Vec<u8>> {
        let recipient: UserId = recipient.parse().ok()?;
        if !matches!(self.channel_repo.get_membership(channel_id, &recipient), Ok(Some(_))) {
            return None;
        }
        crate::services::profile_visibility::shareable_profile_key(&self.settings_repo, &self.user_repo, &recipient)
            .map(|k| k.to_vec())
    }

    fn record_sender_key_delivered(&self, channel_id: &ChannelId, member_id: &str) {
        if let Err(e) = self.signal_service.mark_sender_key_delivered(channel_id, member_id) {
            tracing::warn!(%channel_id, member_id, error = %e, "Failed to record SKDM delivery");
        }
    }

    /// Our own other devices are tracked per DEVICE (`"{user}.{device}"`), not
    /// per user like other members: a device linked after our key went out
    /// must get it too, or it cannot read what this device sends to the
    /// channel (Signal-Desktop fans the SKDM to the sender's own devices).
    async fn record_own_devices_hold_sender_key(&self, channel_id: &ChannelId, our_user_id: &str) {
        for device_id in self.own_device_ids().await.unwrap_or_default() {
            self.record_sender_key_delivered(channel_id, &format!("{our_user_id}.{device_id}"));
        }
    }

    /// Our live device ids, from the `own_devices` cache (refetched when
    /// over a minute old). `None` when the list cannot be fetched.
    async fn own_device_ids(&self) -> Option<std::collections::HashSet<u32>> {
        const MAX_AGE: u64 = 60 * 1_000_000_000;
        let mut cache = self.own_devices.lock().await;
        let now = TimestampNanos::now().as_u64();
        if let Some((fetched_at, ids)) = cache.as_ref() {
            if now.saturating_sub(*fetched_at) <= MAX_AGE {
                return Some(ids.clone());
            }
        }
        match self.fetch_own_device_ids().await {
            Ok(ids) => {
                *cache = Some((now, ids.clone()));
                Some(ids)
            }
            Err(e) => {
                tracing::warn!(error = %e, "Could not list our devices");
                None
            }
        }
    }

    /// Our devices that can still send and receive: the ones with a prekey
    /// bundle, the same set every own-user fan-out encrypts to. Not
    /// `ListDevices` — an identity republish drops every bundle but leaves the
    /// old `user_devices` rows (server `republish_identity.go`), so a dead
    /// install stays listed there forever and its channel messages were parked
    /// and re-requested from it on every launch. The wildcard fetch pops no
    /// one-time prekeys.
    async fn fetch_own_device_ids(&self) -> Result<std::collections::HashSet<u32>, crate::error::NetworkError> {
        use crate::services::current_user::CurrentUserProvider;
        let me = self.settings_repo.current_user_id()
            .map_err(|e| crate::error::NetworkError::InvalidRequest(e.to_string()))?;
        let resp = self.auth_client().get_pre_key_bundles_all_devices(&me.to_string()).await?;
        Ok(bundle_device_ids(resp))
    }

    /// Hand our CURRENT sender key to every channel member that does not hold
    /// it yet. MUST run before the message is encrypted: the SKDM carries the
    /// chain's current iteration, so one built after `group_encrypt` would
    /// start one past the message it was meant to unlock (Signal-Desktop
    /// `sendToGroup` sends to `newToMemberDevices` first for the same reason).
    ///
    /// A member we cannot reach is logged and left for the next send — failing
    /// the whole send would let one unreachable member mute the channel. That
    /// member reads this message only if its SKDM lands before the next one.
    ///
    /// ponytail: an unreachable member costs a prekey fetch per send; add a
    /// per-member backoff if that shows up against the fetch limiter.
    pub(crate) async fn distribute_sender_key_to_new_members(
        &self,
        channel_id: &ChannelId,
    ) -> Result<(), MessageError> {
        // No key yet: `encrypt_for_group` creates one and the caller fans it
        // to everyone via `distribute_sender_key`. Mid-rotation: the old key
        // must not go anywhere, and `encrypt_for_group` refuses the send.
        let crypto = |e: crate::error::ServiceError| MessageError::EncryptionFailed(e.to_string());
        if self.signal_service.is_channel_send_blocked(channel_id).map_err(crypto)?
            || !self.signal_service.has_sender_key(channel_id).await.map_err(crypto)?
        {
            return Ok(());
        }
        let our_user_id = self.settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .map_err(MessageError::Storage)?
            .ok_or(MessageError::NotLoggedIn)?;
        let our_device_id = self.settings_repo
            .get::<u32>(keys::CURRENT_DEVICE_ID)
            .map_err(MessageError::Storage)?
            .ok_or(MessageError::NotLoggedIn)?;
        let holders = self.signal_service.sender_key_recipients(channel_id).map_err(crypto)?;
        let members: Vec<UserId> = self
            .channel_repo
            .list_members(channel_id)
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.user_id)
            .collect();
        let missing: Vec<UserId> = members
            .iter()
            .copied()
            .filter(|u| {
                let u = u.to_string();
                u != our_user_id && !holders.contains(&u)
            })
            .collect();
        // Our own other devices (e.g. one linked since our key went out).
        let own_missing = members.iter().any(|u| u.to_string() == our_user_id)
            && self.own_device_ids().await.is_some_and(|ids| {
                ids.iter()
                    .any(|d| *d != our_device_id && !holders.contains(&format!("{our_user_id}.{d}")))
            });
        if missing.is_empty() && !own_missing {
            return Ok(());
        }

        let skdm = self.create_sender_key_for_channel(channel_id).await?;
        if own_missing {
            if let Ok(our_id) = our_user_id.parse::<UserId>() {
                if let Err(e) = self.send_skdm_to_member(channel_id, &our_id, &skdm).await {
                    tracing::warn!(%channel_id, error = %e,
                        "Could not hand our sender key to our own other devices before sending");
                }
            }
        }
        for member_id in &missing {
            if let Err(e) = self.send_skdm_to_member(channel_id, member_id, &skdm).await {
                tracing::warn!(
                    %channel_id, %member_id, error = %e,
                    "Could not hand our sender key to a member before sending; \
                     they cannot read this message unless it lands first"
                );
            }
        }
        Ok(())
    }

    /// Whether to send a `SenderKeyRequest` for `(channel, sender, device)` now.
    ///
    /// One outstanding request per key, with exponential backoff: a history
    /// sync that parks twenty of one sender's messages used to fire twenty
    /// requests (and twenty prekey fetches, which tripped the server's per-target
    /// fetch limiter), and the sender answered every one. The gate is lifted
    /// when that key's SKDM arrives (`retry_pending_group_decrypts`).
    pub fn should_request_sender_key(&self, channel_id: ChannelId, sender_id: UserId, device_id: u32) -> bool {
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
        if parked.iter().any(|(_, m)| m.id == proto_msg.id && m.content == proto_msg.content) {
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
                svc.resend_parked(channel_id, sender_id, device_id, proto_msg).await;
            });
        }

        let svc = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(PENDING_GROUP_DECRYPT_TTL).await;
            svc.expire_pending_group_decrypt(channel_id, sender_id, device_id, &message_id).await;
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
            let Some(parked) = guard.get_mut(&key) else { return };
            let (expired, kept): (Vec<_>, Vec<_>) =
                std::mem::take(parked).into_iter().partition(|(_, m)| m.id == message_id);
            *parked = kept;
            if parked.is_empty() {
                guard.remove(&key);
            }
            expired
        };
        let Some((_, proto_msg)) = expired.into_iter().next() else { return };
        tracing::info!(%channel_id, %sender_id, device_id, %message_id,
            "Sender key never arrived for a parked message; requesting a resend");
        self.resend_parked(channel_id, sender_id, device_id, proto_msg).await;
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

        let Some(Type::Encrypted(enc)) = proto_msg.content.as_ref().and_then(|c| c.r#type.as_ref()) else {
            return;
        };
        let Ok(group_encrypted) = GroupEncrypted::decode(enc.ciphertext.as_slice()) else { return };
        self.request_group_resend(channel_id, sender_id, device_id, &proto_msg, &group_encrypted.ciphertext)
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

        // The key we asked for arrived: the next miss may ask again at once.
        // Device 0 is the inbox path's "device unknown" entry.
        {
            let mut gate = self.sender_key_requests.lock();
            gate.remove(&(channel_id, sender_id, device_id));
            gate.remove(&(channel_id, sender_id, 0));
        }

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
        if !self.may_hold_sender_key(channel_id, member_id) {
            return Err(MessageError::EncryptionFailed(format!(
                "{member_id} is not a current member of {channel_id}; refusing to hand it our sender key"
            )));
        }
        let mut auth_client = self.auth_client();

        let member_id_str = member_id.to_string();
        let plaintext = self.skdm_plaintext(channel_id, skdm, &member_id_str);
        // To ourselves (our other devices asking, or a newly linked one) the
        // deposit leaves out this device, which already holds the key.
        let is_self = self.settings_repo.get_string(keys::CURRENT_USER_ID).ok().flatten().as_deref()
            == Some(member_id_str.as_str());

        // Per-device sealed deposit, UAK-authorized: sender keys are keyed by
        // (user, device), so each device needs its own ciphertext.
        let mut message_client = self.message_client();
        let outcome = self
            .deposit_sealed_to_all_devices(&mut auth_client, &mut message_client, &member_id_str, &plaintext)
            .await;

        if outcome.delivered == 0 {
            return Err(MessageError::EncryptionFailed(
                "SKDM delivery to new member failed on every device".into()
            ));
        }
        if is_self {
            self.record_own_devices_hold_sender_key(channel_id, &member_id_str).await;
        } else {
            self.record_sender_key_delivered(channel_id, &member_id_str);
        }

        tracing::info!(
            channel_id = %channel_id,
            member_id = %member_id,
            "Sent SKDM to new channel member via sealed sender"
        );

        Ok(())
    }

    /// Whether `user` may be handed our sender key for `channel_id`: only a
    /// CURRENT member per our local membership, the same rows every SKDM
    /// fan-out iterates (a kick/ban deletes the row and marks the key for
    /// rotation). Signal distributes sender keys to current group members
    /// only. Fails closed: a lookup error hands out nothing.
    fn may_hold_sender_key(&self, channel_id: &ChannelId, user: &UserId) -> bool {
        matches!(self.channel_repo.get_membership(channel_id, user), Ok(Some(_)))
    }

    /// Answer a `SenderKeyRequest` from `requester` — the authenticated sender
    /// of the request, never a field inside it. Anyone who knows a channel id
    /// can ask, so a requester that is not a current member is dropped before
    /// any key is created. `skdm_bytes` is the SKDM the gateway already built,
    /// or empty to fetch ours here. `Ok(false)` = dropped.
    pub async fn answer_sender_key_request(
        &self,
        channel_id: &ChannelId,
        requester: &UserId,
        skdm_bytes: &[u8],
    ) -> Result<bool, MessageError> {
        if !self.may_hold_sender_key(channel_id, requester) {
            tracing::debug!(%channel_id, %requester, "Dropping SenderKeyRequest from a non-member");
            return Ok(false);
        }
        let skdm = if skdm_bytes.is_empty() {
            self.create_sender_key_for_channel(channel_id).await?
        } else {
            use prost::Message as _;
            crate::proto::content_v1::SenderKeyDistribution::decode(skdm_bytes)
                .map_err(|e| MessageError::InvalidContent(format!("SKDM decode: {e}")))?
        };
        self.send_skdm_to_member(channel_id, requester, &skdm).await?;
        Ok(true)
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
            .deposit_sealed_to_all_devices(&mut auth_client, &mut message_client, &target_id_str, &plaintext)
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

    /// RECEIVER half of Signal's sender-key retry: ask the device that sent a
    /// group message we cannot decrypt to resend it pairwise.
    ///
    /// The SKDM a sender hands us starts at its chain's CURRENT iteration, so a
    /// message it sent before we held the key ("old counter") is unreadable
    /// here for good; only the sender can give us the content again
    /// (Signal-Desktop `onRetryRequest`). One request per (message, device).
    pub async fn request_group_resend(
        &self,
        channel_id: ChannelId,
        sender_id: UserId,
        device_id: u32,
        proto_msg: &crate::proto::chat_v1::Message,
        ciphertext: &[u8],
    ) {
        use crate::services::current_user::CurrentUserProvider;

        // Our own sender-key ciphertext from this very device is undecryptable
        // by design, not lost.
        let us = (
            self.settings_repo.current_user_id().ok(),
            self.settings_repo.get::<u32>(keys::CURRENT_DEVICE_ID).ok().flatten(),
        );
        if us == (Some(sender_id), Some(device_id)) {
            return;
        }
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
                .get_by_conversation_sender_timestamp(&channel_id, &sender_id, proto_msg.timestamp)
                .ok()
                .flatten()
                .is_some();
        if held {
            return;
        }
        // A retired device will never answer, and the server refuses to queue
        // anything for it.
        if self.is_retired_device(sender_id, device_id, proto_msg.timestamp).await {
            tracing::info!(%channel_id, %sender_id, device_id, message_id = %proto_msg.id,
                "Group message from a retired device is unrecoverable; dropped");
            return;
        }
        // Recorded before the request goes out, so the answer cannot race it.
        self.solicited_resends
            .insert((channel_id, sender_id, proto_msg.timestamp / 1_000_000));
        {
            let mut asked = self.group_retry_requests.lock();
            // ponytail: cleared wholesale at 4096 entries; a forgotten entry
            // costs at most one repeat request, which the sender answers again.
            if asked.len() >= 4096 {
                asked.clear();
            }
            if !asked.insert((proto_msg.id.clone(), device_id)) {
                return;
            }
        }

        let dem = match crate::services::signal::SignalService::group_decryption_error(
            ciphertext,
            proto_msg.timestamp,
            device_id,
        ) {
            Ok(dem) => dem,
            Err(e) => {
                tracing::warn!(%channel_id, %sender_id, device_id, error = %e,
                    "Could not build DecryptionErrorMessage for a group message");
                return;
            }
        };
        match self.send_decryption_error_report(&sender_id, device_id, dem).await {
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
            }
            Err(e) => tracing::warn!(
                %channel_id, %sender_id, device_id, message_id = %proto_msg.id, error = %e,
                "Failed to send DecryptionErrorMessage for a group message"
            ),
        }
    }

    /// Whether `(user, device)` is a device the server no longer lists, for a
    /// group message the server stamped `sent_at` (epoch nanos). Nobody holds
    /// such a device's sender key any more, so a message from it is
    /// unrecoverable: asking for its SKDM or a resend only reaches a device
    /// that does not exist.
    ///
    /// Our OWN devices are checked against the ones holding a prekey bundle
    /// (`fetch_own_device_ids`). A peer device counts as retired only once the server has
    /// refused a send to it, for [`PENDING_GROUP_DECRYPT_TTL`] — the server
    /// reuses a removed device's id, so the verdict must not stick.
    /// Fails open (not retired) when the list cannot be fetched.
    pub(crate) async fn is_retired_device(&self, user_id: UserId, device_id: u32, sent_at: u64) -> bool {
        use crate::services::current_user::CurrentUserProvider;

        if self.settings_repo.current_user_id().ok() != Some(user_id) {
            let mut retired = self.retired_peer_devices.lock();
            retired.retain(|_, at| at.elapsed() < PENDING_GROUP_DECRYPT_TTL);
            return retired.contains_key(&(user_id, device_id));
        }
        // Held across the fetch: a history sync full of one retired device's
        // messages costs one ListDevices, not one per message.
        let mut cache = self.own_devices.lock().await;
        let now = TimestampNanos::now().as_u64();
        if let Some((fetched_at, ids)) = cache.as_ref() {
            if ids.contains(&device_id) {
                return false;
            }
            if !own_device_list_stale(*fetched_at, now, sent_at) {
                return true;
            }
        }
        match self.fetch_own_device_ids().await {
            Ok(ids) => {
                let retired = !ids.contains(&device_id);
                *cache = Some((now, ids));
                retired
            }
            Err(e) => {
                tracing::warn!(error = %e, device_id, "Could not list our devices; treating the device as current");
                false
            }
        }
    }

    /// SENDER half of Signal's sender-key retry: `peer_id`'s device could not
    /// decrypt the group message we sent at `timestamp_ms` (server time, epoch
    /// millis). Resend its content pairwise to that one device, as a
    /// `DataMessage` stamped with the message's server timestamp so the peer
    /// files it under the same (sender, sent_at) as the copy it could not read.
    ///
    /// Bounded like Signal-Desktop's send log: only our own messages, only for
    /// 24h, and only to a device of a CURRENT member of that channel — a
    /// removed member must not get content back by asking for it.
    pub async fn resend_on_decryption_error(
        &self,
        peer_id: &UserId,
        peer_device_id: u32,
        timestamp_ms: u64,
    ) {
        match self.resend_group_message(peer_id, peer_device_id, timestamp_ms).await {
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
        let Some(plaintext) = self.group_resend_content(peer_id, timestamp_ms)? else {
            return Ok(false);
        };
        let sealed_body = self
            .signal_service
            .encrypt_message(&mut self.auth_client(), &peer_id.to_string(), peer_device_id, &plaintext)
            .await
            .map_err(|e| match e {
                // Kept typed: a NotFound here is the retired-device verdict.
                crate::error::ServiceError::Network(e) => MessageError::Network(e),
                e => MessageError::EncryptionFailed(e.to_string()),
            })?;
        self.deposit_sealed_to_device(peer_id, peer_device_id, sealed_body).await?;
        Ok(true)
    }

    /// The content `peer_id` may have back for our group message sent at
    /// `timestamp_ms`, or `None` when the retention and membership bounds of
    /// [`Self::resend_on_decryption_error`] say no.
    fn group_resend_content(
        &self,
        peer_id: &UserId,
        timestamp_ms: u64,
    ) -> Result<Option<Vec<u8>>, MessageError> {
        use crate::domain::message::ConversationType;
        use crate::services::current_user::CurrentUserProvider;
        const RETENTION_NS: u64 = 24 * 60 * 60 * 1_000_000_000;

        let us = self.settings_repo.current_user_id()?;
        // ponytail: scans our newest 1000 messages; add a (sender, sent_at
        // range) repo query if one user sends more than that within 24h.
        let Some(message) = self.message_repo.list_by_sender(&us, 1000)?.into_iter().find(|m| {
            m.sent_at / 1_000_000 == timestamp_ms
                && m.conversation_type == ConversationType::Channel
                && !m.is_deleted
        }) else {
            return Ok(None);
        };
        if TimestampNanos::now().as_u64().saturating_sub(message.sent_at) > RETENTION_NS {
            return Ok(None);
        }
        let is_member = self
            .channel_repo
            .list_members(&message.conversation_id)
            .unwrap_or_default()
            .iter()
            .any(|m| m.user_id == *peer_id);
        if !is_member {
            return Ok(None);
        }
        Ok(self.rebuild_group_content(&message))
    }

    /// The pairwise `Content` for one of our stored channel messages: the
    /// fields the original sender-key `DataMessage` carried (`core.rs`
    /// `send_message`), rebuilt from the local row. `None` for content that
    /// is not a user message.
    fn rebuild_group_content(&self, message: &crate::domain::message::Message) -> Option<Vec<u8>> {
        use crate::domain::message::MessageContent;
        use crate::proto::content_v1::{content, Content, DataMessage};
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

        let dm = DataMessage {
            body,
            attachments,
            channel_id: message.conversation_id.to_string(),
            // The SERVER timestamp, not the original client clock: it is the
            // sent_at every member files this message (and its reactions,
            // edits, deletes) under.
            timestamp: message.sent_at,
            quote: self.build_wire_quote(quote.as_ref()),
            thread_id: message.thread_id.map(|id| id.to_string()).unwrap_or_default(),
            mentioned_integrations: message.mentioned_integrations.iter().map(|id| id.to_string()).collect(),
            targets_ai_bot: message.targets_ai_bot,
            ephemeral: ttl.is_some(),
            ephemeral_ttl_seconds: ttl.unwrap_or(0),
            // Our row's id is the server's (`send_message` stores the
            // server response), i.e. the id every other member holds.
            message_id: message.id.to_string(),
            ..Default::default()
        };
        let mut content = Content {
            timestamp: dm.timestamp,
            content: Some(content::Content::DataMessage(dm)),
            ..Default::default()
        };
        Self::pad_content_to_bucket(&mut content);
        Some(content.encode_to_vec())
    }

    /// Deposit one sealed envelope in one device inbox, UAK-authorized when we
    /// hold the peer's profile key (the same choice as
    /// `send_decryption_error_report`).
    pub(super) async fn deposit_sealed_to_device(
        &self,
        peer_id: &UserId,
        device_id: u32,
        sealed_body: Vec<u8>,
    ) -> Result<(), MessageError> {
        let sealed_message = crate::proto::chat_v1::SealedSenderMessage {
            sealed_body,
            server_key_id: self.signal_service.get_server_key_id(),
            timestamp: TimestampNanos::now().as_u64(),
        };
        let peer = peer_id.to_string();
        let uak = self.sealed_access_key(&peer, false);
        let identified = match uak {
            Some(uak) => match self
                .grpc_pool
                .anonymous_message_client()
                .send_sealed_message_with_uak(&peer, device_id, sealed_message.clone(), &uak)
                .await
            {
                Ok(_) => false,
                // Our stored key is stale: the peer rotated it
                // (`profile_visibility`), so its UAK moved with it.
                Err(e) if e.is_auth_error() => {
                    tracing::warn!(%peer_id, reason = "UAK from our stored profile key rejected", "SEALED SENDER DEGRADED: group resend delivered identified");
                    true
                }
                Err(e) => return Err(e.into()),
            },
            None => {
                tracing::warn!(
                    %peer_id,
                    reason = "no stored profile key to derive the peer's UAK",
                    "SEALED SENDER DEGRADED: group resend delivered identified \
                     (Basic auth) — the server WILL record us as the sender"
                );
                true
            }
        };
        if identified {
            self.message_client()
                .send_sealed_message(&peer, device_id, sealed_message)
                .await?;
        }
        Ok(())
    }

    /// Pad a Content proto to the nearest standard bucket size for traffic analysis protection.
    ///
    /// The server validates that sealed sender ciphertexts match one of these plaintext bucket
    /// sizes: 256, 512, 1024, 4096 bytes. Without padding, message sizes leak event type info.
    pub(super) fn pad_content_to_bucket(content: &mut crate::proto::content_v1::Content) {
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

/// Device ids in a prekey-bundle response (multi-device list or the
/// single-bundle fallback).
fn bundle_device_ids(resp: crate::proto::chat_v1::GetPreKeyBundleResponse) -> std::collections::HashSet<u32> {
    use crate::proto::chat_v1::get_pre_key_bundle_response::Result;
    match resp.result {
        Some(Result::Devices(list)) => list.bundles.iter().map(|b| b.device_id).collect(),
        Some(Result::Bundle(b)) => [b.device_id].into(),
        None => Default::default(),
    }
}

/// How long a group message parked for a missing sender key waits for its
/// SKDM before we ask the sender to resend it instead.
const PENDING_GROUP_DECRYPT_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// Whether a cached own-device list that lacks a device must be refetched
/// before calling that device retired: it is over a minute old, or the message
/// is newer than the list (a device linked since). 30s of slack for the gap
/// between the server's clock, which stamps `sent_at`, and ours.
fn own_device_list_stale(fetched_at: u64, now: u64, sent_at: u64) -> bool {
    const MAX_AGE: u64 = 60 * 1_000_000_000;
    const CLOCK_SLACK: u64 = 30 * 1_000_000_000;
    now.saturating_sub(fetched_at) > MAX_AGE || sent_at.saturating_add(CLOCK_SLACK) > fetched_at
}

/// Per sender: start of the current one-minute window, and resend requests
/// sent in it for parked messages the cap evicted.
pub(crate) type EvictionResendGate = std::collections::HashMap<UserId, (std::time::Instant, u32)>;

/// At most 10 eviction resend requests per sender per minute.
pub(crate) fn eviction_resend_due(gate: &mut EvictionResendGate, sender: UserId, now: std::time::Instant) -> bool {
    const WINDOW: std::time::Duration = std::time::Duration::from_secs(60);
    const PER_WINDOW: u32 = 10;
    // ponytail: never pruned; one entry per sender that ever overflowed the cap.
    let entry = gate.entry(sender).or_insert((now, 0));
    if now.duration_since(entry.0) >= WINDOW {
        *entry = (now, 0);
    }
    if entry.1 >= PER_WINDOW {
        return false;
    }
    entry.1 += 1;
    true
}

/// Backoff state per `(channel, sender, device)`: when the next request may go
/// out, and how many have gone out without an answer.
pub(crate) type SenderKeyRequestGate =
    std::collections::HashMap<(ChannelId, UserId, u32), (std::time::Instant, u32)>;

/// 30s, 60s, 120s ... capped at 15 min between unanswered requests.
fn sender_key_request_due(
    gate: &mut SenderKeyRequestGate,
    key: (ChannelId, UserId, u32),
    now: std::time::Instant,
) -> bool {
    const BASE: std::time::Duration = std::time::Duration::from_secs(30);
    const MAX: std::time::Duration = std::time::Duration::from_secs(15 * 60);
    let entry = gate.entry(key).or_insert((now, 0));
    if now < entry.0 {
        return false;
    }
    let delay = BASE.saturating_mul(1u32 << entry.1.min(10)).min(MAX);
    *entry = (now + delay, entry.1 + 1);
    true
}

#[cfg(test)]
mod tests {
    /// Own devices are the ones with a bundle; a response without any is none.
    #[test]
    fn own_devices_are_the_bundle_holders() {
        use crate::proto::chat_v1::{get_pre_key_bundle_response::Result, GetPreKeyBundleResponse, SignalPreKeyBundle};
        let b = |device_id| SignalPreKeyBundle { device_id, ..Default::default() };
        let many = GetPreKeyBundleResponse {
            result: Some(Result::Devices(crate::proto::chat_v1::PreKeyBundleDeviceList {
                bundles: vec![b(2), b(6), b(7), b(8)],
                ..Default::default()
            })),
            ..Default::default()
        };
        assert_eq!(super::bundle_device_ids(many), [2, 6, 7, 8].into());
        let one = GetPreKeyBundleResponse { result: Some(Result::Bundle(b(6))), ..Default::default() };
        assert_eq!(super::bundle_device_ids(one), [6].into());
        assert!(super::bundle_device_ids(GetPreKeyBundleResponse::default()).is_empty());
    }

    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn sender_key_request_is_sent_once_then_backs_off() {
        let mut gate = SenderKeyRequestGate::new();
        let key = (ChannelId::new(), UserId::new(), 6);
        let t0 = Instant::now();

        // A burst of misses from one history sync: one request.
        assert!(sender_key_request_due(&mut gate, key, t0));
        for _ in 0..20 {
            assert!(!sender_key_request_due(&mut gate, key, t0));
        }
        assert!(!sender_key_request_due(&mut gate, key, t0 + Duration::from_secs(29)));
        // Unanswered: ask again after 30s, then the wait doubles.
        assert!(sender_key_request_due(&mut gate, key, t0 + Duration::from_secs(30)));
        assert!(!sender_key_request_due(&mut gate, key, t0 + Duration::from_secs(89)));
        assert!(sender_key_request_due(&mut gate, key, t0 + Duration::from_secs(90)));

        // Another device of the same sender is gated on its own.
        assert!(sender_key_request_due(&mut gate, (key.0, key.1, 1), t0));

        // SKDM arrived (entry removed): the next miss asks immediately.
        gate.remove(&key);
        assert!(sender_key_request_due(&mut gate, key, t0 + Duration::from_secs(91)));
    }

    /// Alice and Bob, each a client with its own store, both members of one
    /// channel. The TempDirs must outlive the clients.
    struct TwoMembers {
        alice: std::sync::Arc<crate::client::ChatterClient>,
        bob: std::sync::Arc<crate::client::ChatterClient>,
        alice_id: UserId,
        bob_id: UserId,
        channel_id: ChannelId,
        _dirs: (tempfile::TempDir, tempfile::TempDir),
    }

    fn two_members() -> TwoMembers {
        use crate::client::ChatterClient;
        use crate::domain::channel::{Channel, ChannelType, MemberRole, Membership, NotificationPreference};

        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
        let client = |dir: &tempfile::TempDir, user: UserId| {
            let c = ChatterClient::builder()
                .server_url("http://localhost:1")
                .data_dir(dir.path().to_str().unwrap())
                .build()
                .expect("build client");
            let s = &c.messages().settings_repo;
            s.set(keys::CURRENT_USER_ID, &user.to_string()).unwrap();
            s.set(keys::CURRENT_DEVICE_ID, &1u32).unwrap();
            c
        };
        let (alice_id, bob_id) = (UserId::new(), UserId::new());
        let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (alice, bob) = (client(&da, alice_id), client(&db, bob_id));
        let channel_id = ChannelId::new();
        for c in [&alice, &bob] {
            let repo = &c.messages().channel_repo;
            repo.insert(&Channel::new(channel_id, "g".into(), ChannelType::Private, alice_id)).unwrap();
            for user_id in [alice_id, bob_id] {
                repo.insert_membership(&Membership {
                    channel_id,
                    user_id,
                    role: MemberRole::Member,
                    joined_at: chrono::Utc::now(),
                    last_read_message_id: None,
                    is_muted: false,
                    muted_until: None,
                    notification_preference: NotificationPreference::default(),
                })
                .unwrap();
            }
        }

        TwoMembers { alice, bob, alice_id, bob_id, channel_id, _dirs: (da, db) }
    }

    /// Signal hands sender keys to current group members only: a request from
    /// a non-member is dropped before any key is created; a member's is
    /// answered (its delivery fails here only because there is no server).
    #[tokio::test]
    async fn sender_key_request_is_answered_for_members_only() {
        let t = two_members();
        let alice = t.alice.messages();
        let outsider = UserId::new();

        assert!(matches!(alice.answer_sender_key_request(&t.channel_id, &outsider, &[]).await, Ok(false)));
        assert!(!t.alice.signal().has_sender_key(&t.channel_id).await.unwrap(),
            "a non-member's request must not even create our key");
        let skdm = t.alice.signal().create_sender_key(&t.channel_id).await.unwrap();
        assert!(alice.send_skdm_to_member(&t.channel_id, &outsider, &skdm).await.is_err());

        let answered = alice.answer_sender_key_request(&t.channel_id, &t.bob_id, &[]).await;
        assert!(matches!(answered, Err(MessageError::EncryptionFailed(ref e)) if !e.contains("not a current member")),
            "a member's request goes to delivery: {answered:?}");

        // Removed (kick/ban deletes the row): no longer answered.
        alice.channel_repo.delete_membership(&t.channel_id, &t.bob_id).unwrap();
        assert!(matches!(alice.answer_sender_key_request(&t.channel_id, &t.bob_id, &[]).await, Ok(false)));
    }

    /// Alice's group message as the server hands it to Bob.
    fn server_copy(
        t: &TwoMembers,
        id: &crate::domain::ids::MessageId,
        ts: u64,
        ge: &crate::proto::content_v1::GroupEncrypted,
    ) -> crate::proto::chat_v1::Message {
        use crate::proto::chat_v1;
        use prost::Message as _;
        chat_v1::Message {
            id: id.to_string(),
            channel_id: t.channel_id.to_string(),
            author_id: t.alice_id.to_string(),
            author_device_id: 1,
            timestamp: ts,
            content: Some(chat_v1::message::Content {
                r#type: Some(chat_v1::message::content::Type::Encrypted(chat_v1::EncryptedData {
                    ciphertext: ge.encode_to_vec(),
                    ..Default::default()
                })),
            }),
            ..Default::default()
        }
    }

    /// Signal's sender-key retry, end to end between two real clients short of
    /// the network hop: Bob cannot read Alice's message sent before her SKDM,
    /// asks with a DecryptionErrorMessage, and Alice's pairwise resend lands in
    /// Bob's channel under the message's server id and (sender, sent_at).
    #[tokio::test]
    async fn pre_skdm_group_message_is_recovered_by_dem_and_resend() {
        use crate::domain::ids::MessageId;
        use crate::domain::message::{ConversationType, Message, MessageContent};
        use crate::proto::content_v1;
        use crate::services::message::channel_inbound::{ChannelInbound, Ingested};
        use prost::Message as _;

        let t = two_members();
        let (alice, bob) = (&t.alice, &t.bob);
        let (alice_id, bob_id, channel_id) = (t.alice_id, t.bob_id, t.channel_id);

        // Alice sends before Bob holds her key; the server stamps it T.
        let plaintext = content_v1::Content {
            content: Some(content_v1::content::Content::DataMessage(content_v1::DataMessage {
                body: "sent before the SKDM".into(),
                channel_id: channel_id.to_string(),
                ..Default::default()
            })),
            ..Default::default()
        }
        .encode_to_vec();
        let (ge, _) = alice.signal().encrypt_for_group(&channel_id, &plaintext).await.unwrap();
        let server_ts = TimestampNanos::now().as_u64();
        let message_id = MessageId::new();
        alice
            .messages()
            .message_repo
            .insert_outgoing(&Message {
                id: message_id,
                ..Message::new_text(
                    channel_id,
                    ConversationType::Channel,
                    Some(alice_id),
                    "sent before the SKDM".into(),
                    server_ts,
                )
            })
            .unwrap();

        // Bob gets Alice's SKDM only now: it starts past the message.
        let skdm = alice.signal().create_sender_key(&channel_id).await.unwrap();
        bob.signal().process_sender_key(&alice_id, 1, &skdm).await.unwrap();
        let server_copy = server_copy(&t, &message_id, server_ts, &ge);
        let inbound = ChannelInbound::new(bob.messages().clone(), bob.attachments().clone());
        assert_eq!(inbound.ingest(&server_copy, channel_id, false).await, Ingested::Dropped,
            "positive control: the pre-SKDM message is unreadable on its own");

        // Bob's DEM names a sender-key message: no ratchet key, so Alice's
        // gateway resends instead of archiving their 1:1 session.
        let dem_bytes = crate::services::signal::SignalService::group_decryption_error(&ge.ciphertext, server_ts, 1).unwrap();
        let dem = libsignal_protocol::DecryptionErrorMessage::try_from(dem_bytes.as_slice()).unwrap();
        assert!(dem.ratchet_key().is_none());
        assert_eq!(dem.device_id(), 1);
        let ts_ms = dem.timestamp().epoch_millis();

        // Alice's resend bounds: a non-member gets nothing back.
        assert!(alice.messages().group_resend_content(&UserId::new(), ts_ms).unwrap().is_none());
        let resent = alice.messages().group_resend_content(&bob_id, ts_ms).unwrap()
            .expect("Alice finds the message she sent at that timestamp");

        // Bob ingests it through the pairwise sealed DataMessage path.
        let Some(content_v1::content::Content::DataMessage(dm)) =
            content_v1::Content::decode(resent.as_slice()).unwrap().content
        else {
            panic!("resend is not a DataMessage");
        };
        let m = bob.messages();
        let gateway = crate::services::gateway::MessageGateway::new(
            (*m.signal_service).clone(),
            m.message_repo.clone(),
            m.channel_repo.clone(),
            m.user_repo.clone(),
            m.settings_repo.clone(),
            m.solicited_resends.clone(),
        );
        // Bob's ingest above asked Alice for this message.
        for _ in 0..10 {
            if !m.solicited_resends.is_empty() { break; }
            tokio::task::yield_now().await;
        }
        assert!(m.solicited_resends.contains(&(channel_id, alice_id, server_ts / 1_000_000)));
        gateway.store_data_message(dm, Some(alice_id)).await.unwrap();
        let stored = m
            .message_repo
            .get_by_conversation_sender_timestamp(&channel_id, &alice_id, server_ts)
            .unwrap()
            .expect("the recovered message is filed under the server timestamp");
        assert_eq!(stored.content, MessageContent::Text("sent before the SKDM".into()));
        assert_eq!(stored.id, message_id,
            "the recovered message keeps the server id the other members hold");

        // A later re-fetch of the server copy neither duplicates it nor asks
        // again (the first ingest above already recorded its one request).
        assert_eq!(inbound.ingest(&server_copy, channel_id, false).await, Ingested::Dropped);
        m.group_retry_requests.lock().clear();
        m.request_group_resend(channel_id, alice_id, 1, &server_copy, &ge.ciphertext).await;
        assert!(m.group_retry_requests.lock().is_empty());
    }

    /// F72: a channel member's SKDM carries their profile key to the other
    /// members — and only to them: not to a non-member who asked, not to a
    /// member they blocked — and the receiver keeps it without a friendship.
    #[tokio::test]
    async fn skdm_shares_profile_key_with_channel_members_only() {
        use crate::domain::user::{Friendship, FriendshipStatus};
        use crate::proto::content_v1;
        use prost::Message as _;

        let t = two_members();
        let (alice, bob) = (&t.alice, &t.bob);
        alice.messages().settings_repo.set_bytes(keys::PROFILE_KEY, &[9; 32]).unwrap();
        let skdm = alice.signal().create_sender_key(&t.channel_id).await.unwrap();
        let carried = |to: &UserId| {
            let bytes = alice.messages().skdm_plaintext(&t.channel_id, &skdm, &to.to_string());
            match content_v1::Content::decode(bytes.as_slice()).unwrap().content {
                Some(content_v1::content::Content::SenderKey(s)) => s.profile_key,
                _ => panic!("not an SKDM"),
            }
        };

        let key = carried(&t.bob_id).expect("a co-member gets Alice's profile key");
        assert_eq!(key, vec![9; 32]);
        assert!(carried(&UserId::new()).is_none(), "a non-member who asked gets none");
        assert!(carried(&t.alice_id).is_none(), "our own devices already hold it");

        // "My contacts": a co-member who is not Alice's friend gets none.
        use crate::services::profile_visibility::ProfileVisibility;
        ProfileVisibility::Contacts.store(&alice.messages().settings_repo).unwrap();
        assert!(carried(&t.bob_id).is_none(), "a non-friend co-member under my contacts gets none");
        ProfileVisibility::Everyone.store(&alice.messages().settings_repo).unwrap();

        // Bob is not Alice's friend: the key still lands, once.
        let m = bob.messages();
        let gateway = crate::services::gateway::MessageGateway::new(
            (*m.signal_service).clone(),
            m.message_repo.clone(),
            m.channel_repo.clone(),
            m.user_repo.clone(),
            m.settings_repo.clone(),
            m.solicited_resends.clone(),
        );
        assert!(gateway.store_skdm_profile_key(&t.alice_id, Some(&key)));
        assert!(!gateway.store_skdm_profile_key(&t.alice_id, Some(&key)), "same key is no change");
        assert_eq!(m.user_repo.get_friend_profile_key(&t.bob_id, &t.alice_id).unwrap(), Some([9; 32]));

        // Once Alice blocks Bob, her SKDM to him carries no key.
        let am = alice.messages();
        am.user_repo.ensure_user_exists(&t.alice_id).unwrap();
        am.user_repo.ensure_user_exists(&t.bob_id).unwrap();
        am.user_repo
            .insert_friendship(&Friendship {
                requester_id: t.alice_id,
                addressee_id: t.bob_id,
                status: FriendshipStatus::Blocked,
                created_at: chrono::Utc::now(),
            })
            .unwrap();
        assert!(carried(&t.bob_id).is_none(), "a blocked member gets none");
    }

    /// A message parked for a sender key that never comes is not dropped
    /// silently when its wait runs out: Bob asks Alice to resend it, once.
    #[tokio::test]
    async fn parked_group_message_requests_resend_when_its_sender_key_never_arrives() {
        use crate::domain::ids::MessageId;
        use crate::services::message::channel_inbound::{ChannelInbound, Ingested};

        let t = two_members();
        let (ge, _) = t.alice.signal().encrypt_for_group(&t.channel_id, b"lost key").await.unwrap();
        let message_id = MessageId::new();
        let copy = server_copy(&t, &message_id, TimestampNanos::now().as_u64(), &ge);
        let m = t.bob.messages();
        let inbound = ChannelInbound::new(m.clone(), t.bob.attachments().clone());
        assert_eq!(inbound.ingest(&copy, t.channel_id, false).await, Ingested::DeferredPendingSenderKey);
        assert!(m.group_retry_requests.lock().is_empty(), "no resend request while the SKDM may still come");

        m.expire_pending_group_decrypt(t.channel_id, t.alice_id, 1, &copy.id).await;
        assert!(m.pending_group_decrypts.lock().is_empty());
        assert!(m.group_retry_requests.lock().contains(&(copy.id.clone(), 1)),
            "the expired message is handed to request_group_resend");

        // Nothing left parked: a later expiry asks nothing.
        m.group_retry_requests.lock().clear();
        m.expire_pending_group_decrypt(t.channel_id, t.alice_id, 1, &copy.id).await;
        assert!(m.group_retry_requests.lock().is_empty());
    }

    #[test]
    fn own_device_list_is_refetched_only_when_it_may_be_out_of_date() {
        let s = 1_000_000_000u64;
        let fetched = 10_000 * s;
        // A message from long before the list: the list is authoritative.
        assert!(!own_device_list_stale(fetched, fetched + 10 * s, fetched - 3600 * s));
        // A message newer than the list (or within clock slack): maybe a new device.
        assert!(own_device_list_stale(fetched, fetched + 10 * s, fetched + 5 * s));
        assert!(own_device_list_stale(fetched, fetched + 10 * s, fetched - 10 * s));
        // A list over a minute old is refetched regardless.
        assert!(own_device_list_stale(fetched, fetched + 61 * s, fetched - 3600 * s));
    }

    #[test]
    fn eviction_resends_are_capped_per_sender_per_minute() {
        let mut gate = EvictionResendGate::new();
        let (a, b) = (UserId::new(), UserId::new());
        let t0 = Instant::now();
        for _ in 0..10 {
            assert!(eviction_resend_due(&mut gate, a, t0));
        }
        assert!(!eviction_resend_due(&mut gate, a, t0 + Duration::from_secs(59)));
        assert!(eviction_resend_due(&mut gate, b, t0), "another sender has its own budget");
        assert!(eviction_resend_due(&mut gate, a, t0 + Duration::from_secs(60)), "a new minute, a new budget");
    }

    /// F41: a channel message sent from ANOTHER of our devices reaches this
    /// one live and must be decrypted and stored (read — it is ours), not
    /// skipped as "already on screen", which is true only for this device's
    /// own sends.
    #[tokio::test]
    async fn live_message_from_our_other_device_is_stored_as_read() {
        use crate::domain::ids::MessageId;
        use crate::domain::message::{ConversationType, MessageContent};
        use crate::proto::content_v1;
        use crate::services::message::channel_inbound::{ChannelInbound, Ingested};
        use prost::Message as _;

        let t = two_members();
        // Bob's store is Alice's second device (7); Alice's client is device 1.
        let m = t.bob.messages();
        m.settings_repo.set(keys::CURRENT_USER_ID, &t.alice_id.to_string()).unwrap();
        m.settings_repo.set(keys::CURRENT_DEVICE_ID, &7u32).unwrap();
        let skdm = t.alice.signal().create_sender_key(&t.channel_id).await.unwrap();
        t.bob.signal().process_sender_key(&t.alice_id, 1, &skdm).await.unwrap();

        let body = content_v1::Content {
            content: Some(content_v1::content::Content::DataMessage(content_v1::DataMessage {
                body: "from my tablet".into(),
                ..Default::default()
            })),
            ..Default::default()
        }
        .encode_to_vec();
        let (ge, _) = t.alice.signal().encrypt_for_group(&t.channel_id, &body).await.unwrap();
        let id = MessageId::new();
        let copy = server_copy(&t, &id, TimestampNanos::now().as_u64(), &ge);
        let inbound = ChannelInbound::new(m.clone(), t.bob.attachments().clone());

        assert_eq!(inbound.ingest(&copy, t.channel_id, true).await, Ingested::Stored);
        let stored = m.get_message_local(&id).unwrap().expect("stored");
        assert!(matches!(stored.content, MessageContent::Text(ref b) if b == "from my tablet"));
        assert_eq!(
            m.message_repo.get_unread_count(&t.channel_id, &ConversationType::Channel).unwrap(),
            0,
            "our own message is never unread"
        );

        // Positive control: the same message on the device that sent it is skipped.
        m.settings_repo.set(keys::CURRENT_DEVICE_ID, &1u32).unwrap();
        let copy = server_copy(&t, &MessageId::new(), TimestampNanos::now().as_u64(), &ge);
        assert_eq!(inbound.ingest(&copy, t.channel_id, true).await, Ingested::OwnMessage);
    }

    /// Our own previous install (device 1, no longer listed by the server)
    /// sent channel messages before it was retired. Nobody holds its sender
    /// key: the message is dropped, not parked, and nothing is asked of a
    /// device that does not exist.
    #[tokio::test]
    async fn message_from_our_own_retired_device_is_dropped_not_parked() {
        use crate::domain::ids::MessageId;
        use crate::services::message::channel_inbound::{ChannelInbound, Ingested};

        let t = two_members();
        // Bob's store is Alice's new install: same user, device 6.
        let m = t.bob.messages();
        m.settings_repo.set(keys::CURRENT_USER_ID, &t.alice_id.to_string()).unwrap();
        m.settings_repo.set(keys::CURRENT_DEVICE_ID, &6u32).unwrap();
        let now = TimestampNanos::now().as_u64();
        *m.own_devices.lock().await = Some((now, [6].into()));

        let (ge, _) = t.alice.signal().encrypt_for_group(&t.channel_id, b"old install").await.unwrap();
        let copy = server_copy(&t, &MessageId::new(), now - 3600 * 1_000_000_000, &ge);
        let inbound = ChannelInbound::new(m.clone(), t.bob.attachments().clone());
        assert_eq!(inbound.ingest(&copy, t.channel_id, false).await, Ingested::Dropped);
        assert!(m.pending_group_decrypts.lock().is_empty());
        m.request_group_resend(t.channel_id, t.alice_id, 1, &copy, &ge.ciphertext).await;
        assert!(m.group_retry_requests.lock().is_empty(), "no DEM to a retired device");
        assert!(m.solicited_resends.is_empty());

        // Positive control: the same message from a device the server lists
        // waits for its sender key as usual.
        *m.own_devices.lock().await = Some((now, [1, 6].into()));
        assert_eq!(inbound.ingest(&copy, t.channel_id, false).await, Ingested::DeferredPendingSenderKey);
    }

    /// A pairwise resend may take the server id it carries only when this
    /// device asked that sender for the message at that sent_at, once.
    #[tokio::test]
    async fn resend_keeps_its_carried_id_only_when_solicited() {
        use crate::domain::ids::MessageId;
        use crate::proto::content_v1::DataMessage;

        let t = two_members();
        let m = t.bob.messages();
        let gateway = crate::services::gateway::MessageGateway::new(
            (*m.signal_service).clone(),
            m.message_repo.clone(),
            m.channel_repo.clone(),
            m.user_repo.clone(),
            m.settings_repo.clone(),
            m.solicited_resends.clone(),
        );
        let resend = |id: MessageId, ts: u64| DataMessage {
            body: "resent".into(),
            channel_id: t.channel_id.to_string(),
            timestamp: ts,
            message_id: id.to_string(),
            ..Default::default()
        };
        let stored_id = |ts: u64| {
            m.message_repo
                .get_by_conversation_sender_timestamp(&t.channel_id, &t.alice_id, ts)
                .unwrap()
                .expect("stored")
                .id
        };

        // Unsolicited: another member's id cannot be squatted.
        let foreign = MessageId::new();
        let ts1 = TimestampNanos::now().as_u64();
        gateway.store_data_message(resend(foreign, ts1), Some(t.alice_id)).await.unwrap();
        assert_ne!(stored_id(ts1), foreign);
        assert!(m.get_message_local(&foreign).unwrap().is_none());

        // Solicited from ANOTHER sender: still not honoured for Alice.
        let ts2 = ts1 + 7_000_000;
        m.solicited_resends.insert((t.channel_id, t.bob_id, ts2 / 1_000_000));
        let id2 = MessageId::new();
        gateway.store_data_message(resend(id2, ts2), Some(t.alice_id)).await.unwrap();
        assert_ne!(stored_id(ts2), id2);

        // Solicited from Alice at that sent_at: keeps the server id, and the
        // request is consumed.
        let ts3 = ts1 + 14_000_000;
        m.solicited_resends.insert((t.channel_id, t.alice_id, ts3 / 1_000_000));
        let id3 = MessageId::new();
        gateway.store_data_message(resend(id3, ts3), Some(t.alice_id)).await.unwrap();
        assert_eq!(stored_id(ts3), id3);
        assert!(!m.solicited_resends.contains(&(t.channel_id, t.alice_id, ts3 / 1_000_000)));
    }

    /// Past the parked cap the oldest message is evicted — and its sender is
    /// asked to resend it rather than it vanishing.
    #[tokio::test]
    async fn parked_message_evicted_by_the_cap_requests_a_resend() {
        use crate::domain::ids::MessageId;

        let t = two_members();
        let m = t.bob.messages();
        let (ge, _) = t.alice.signal().encrypt_for_group(&t.channel_id, b"flood").await.unwrap();
        let ts = TimestampNanos::now().as_u64();
        let copies: Vec<_> = (0..513).map(|i| server_copy(&t, &MessageId::new(), ts + i, &ge)).collect();
        for c in &copies {
            m.defer_pending_group_decrypt(t.channel_id, t.alice_id, 1, c.clone());
        }
        assert_eq!(m.pending_group_decrypts.lock().values().map(Vec::len).sum::<usize>(), 512);
        let oldest = (copies[0].id.clone(), 1);
        for _ in 0..10 {
            if m.group_retry_requests.lock().contains(&oldest) { break; }
            tokio::task::yield_now().await;
        }
        assert!(m.group_retry_requests.lock().contains(&oldest),
            "the evicted message is handed to request_group_resend");
    }
}
