//! Friend DMs — one conversation per friend, two transports.
//!
//! Chatter's DM model mirrors Signal's 1:1 messaging: there is no
//! server-side DM channel. The local conversation is keyed by the peer's
//! `UserId` (`ActiveConversation::ServerDm { peer_id }`, local storage key
//! `ChannelId::from(peer_id)`, `ConversationType::ServerDm`) whichever
//! transport carried a message, so switching modes never hides history.
//!
//! The transport is picked per send by Shield mode, in exactly one place —
//! [`MessageService::send_friend_dm_content`]:
//!
//! - **Normal**: sealed sender, streaming. A message is Signal-Protocol-
//!   encrypted once per recipient device and deposited directly into that
//!   device's inbox via `SendSealedMessage`, authorized with the recipient's
//!   Unidentified Access Key rather than our own credentials — so the server
//!   learns the recipient (and the timing) but never the sender. Instant.
//! - **Shield**: the NDSS'21 ephemeral-mailbox transport (`dm.rs`: two
//!   encryption layers, mailbox addresses, bootstrap handshake), over the
//!   onion pool, received by polling. Recipient and timing are hidden too —
//!   the statistical-disclosure signal sealed sender alone leaks (Martiny et
//!   al., NDSS'21 §III-IV).

use prost::Message as ProstMessage;

use crate::domain::ids::{AttachmentId, ChannelId, UserId};
use crate::domain::message::{ConversationType, Message, MessageContent, MixedContent};
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::message_client::MessageClient;
use crate::proto::chat_v1::{DestinationDevice, SealedSenderMessage};
use crate::proto::content_v1::AttachmentPointer;
use crate::services::current_user::CurrentUserProvider;

use super::{DmSendResult, MessageError, MessageService};

impl MessageService {
    /// Send a DataMessage (text + optional attachments) to a friend over
    /// the channel-free sealed-sender path, and persist it locally as
    /// `ConversationType::ServerDm` keyed by `ChannelId::from(peer_id)`.
    pub async fn send_server_dm(
        &self,
        peer_id: &UserId,
        text: &str,
        attachment_pointers: Vec<AttachmentPointer>,
        sent_at: u64,
        quote: Option<&crate::services::gateway::outgoing::QuoteIntent>,
    ) -> Result<DmSendResult, MessageError> {
        // F11: trim is used ONLY to decide emptiness (a whitespace-only body with
        // no attachments is still rejected — F10). A NON-empty body is sent with
        // its ORIGINAL bytes: the wire must carry exactly what the user typed.
        // Whitespace is stripped at DISPLAY time instead (chatter-ui converters/
        // message.rs trims before rendering), never on the wire.
        let trimmed = text.trim();
        if trimmed.is_empty() && attachment_pointers.is_empty() {
            return Err(MessageError::InvalidContent(
                "message must have text or attachments".to_string(),
            ));
        }

        tracing::info!(
            peer_id = %peer_id,
            text_len = text.len(),
            attachment_count = attachment_pointers.len(),
            is_reply = quote.is_some(),
            sent_at,
            "MessageService: send_server_dm called"
        );

        let attachment_ids: Vec<AttachmentId> = attachment_pointers
            .iter()
            .map(|p| AttachmentId::from_cdn_key(&p.cdn_key))
            .collect();

        let (content, reply_ctx, disappearing_ttl_secs) =
            self.server_dm_content(peer_id, text, attachment_pointers, sent_at, quote)?;

        // Shield, mailbox not established with this friend yet: the content is
        // queued and goes out when the bootstrap completes. Stored now like a
        // sent message so it survives switching conversations.
        //
        // A delivery failure is stored too, as a "failed" bubble the user can
        // retry (`retry_server_dm`), Signal's "Not sent, tap to retry" — not
        // handed back as an error that leaves the text in the composer.
        let (bootstrap_initiated, send_error) = match self.send_friend_dm_content(peer_id, content).await {
            Ok(()) => (false, None),
            Err(MessageError::BootstrapInitiated) => (true, None),
            Err(e) => {
                tracing::warn!(peer_id = %peer_id, error = %e, sent_at, "Friend DM not sent; stored as failed");
                (false, Some(e))
            }
        };

        let current_user_id = self.settings_repo.current_user_id()?;
        let conversation_id: ChannelId = (*peer_id).into();
        let mut message = Message::new_text(
            conversation_id,
            ConversationType::ServerDm,
            Some(current_user_id),
            text.to_string(),
            sent_at,
        );
        // Queued, not sent: the flush's outcome later makes it "sent" or
        // "failed" (`DmBootstrapCompleted` / `MessageSendFailed`).
        if bootstrap_initiated {
            message.delivery_status = "sending".into();
        } else if let Some(error) = &send_error {
            message.delivery_status = "failed".into();
            message.failure_reason = error.permanent_send_failure_reason().map(str::to_owned);
        }

        if !attachment_ids.is_empty() {
            message.attachments = attachment_ids.clone();
            // Emptiness decision still uses `trimmed` (a whitespace-only caption
            // is treated as "no caption"), but a real caption is stored with its
            // ORIGINAL bytes to match the wire (F11).
            message.content = if trimmed.is_empty() {
                MessageContent::Mixed(MixedContent::attachments_only(attachment_ids.clone()))
            } else {
                MessageContent::Mixed(MixedContent::new(text.to_string(), attachment_ids.clone()))
            };
        }

        // Persist reply metadata on the local echo so the sender's bubble keeps
        // its quote across reloads (mirrors the wire Quote we just sent).
        if let Some((q, parts)) = reply_ctx {
            message.reply_to = Some(parts.reply_to);
            message.reply_to_author_id = Some(q.target_author_id);
            message.reply_to_text = parts.reply_to_text;
            message.reply_to_thumbnail = parts.reply_to_thumbnail;
            message.reply_to_thumbnail_mime = parts.reply_to_thumbnail_mime;
        }

        if let Err(e) = self.message_repo.insert_outgoing(&message) {
            tracing::warn!(
                peer_id = %peer_id,
                error = %e,
                "Failed to store sent ServerDm locally"
            );
        } else if let Some(secs) = disappearing_ttl_secs {
            // SENDER local half for sealed DMs: arm our own copy for reaping.
            if let Err(e) = self.ephemeral_service.track_armed(message.id, secs) {
                tracing::warn!(
                    peer_id = %peer_id,
                    error = %e,
                    "failed to arm local disappearing timer for sent ServerDm"
                );
            }
        }

        if let Some(stored) = self.message_repo.get(&message.id)? {
            message.delivery_status = stored.delivery_status;
        }
        Ok(DmSendResult {
            message: Some(message),
            attachment_ids,
            gif_attachment_ids: vec![],
            sent_at,
            bootstrap_initiated,
            send_error,
        })
    }

    /// The wire `Content` of a friend DM with the given `sent_at` (its
    /// DataMessage timestamp, which is also the bubble's key), plus the reply
    /// parts and disappearing timer the local copy is stored with. Shared by
    /// the first send and by `retry_server_dm`, so a retry carries exactly what
    /// the first attempt would have.
    #[allow(clippy::type_complexity)]
    pub(super) fn server_dm_content<'q>(
        &self,
        peer_id: &UserId,
        text: &str,
        attachment_pointers: Vec<AttachmentPointer>,
        sent_at: u64,
        quote: Option<&'q crate::services::gateway::outgoing::QuoteIntent>,
    ) -> Result<
        (
            crate::proto::content_v1::Content,
            Option<(&'q crate::services::gateway::outgoing::QuoteIntent, super::dm::OutboundReplyParts)>,
            Option<u32>,
        ),
        MessageError,
    > {
        // Resolve the quoted message once via the shared assembler (same path
        // the mailbox DM uses). Carrying the quote on the wire is what lets the
        // receiver's `store_data_message` rebuild the reply bubble.
        let reply_ctx = quote.map(|q| {
            let parts = self.assemble_reply_parts(
                &q.target_author_id,
                q.target_timestamp,
                q.target_preview.as_deref().unwrap_or(""),
            );
            (q, parts)
        });

        // Attachment pointers are compact by construction — the thumbnail is a
        // separate CDN sidecar (thumbnail_cdn_key), not inline, and no field
        // scales with file size — so the sealed body always fits the device-inbox
        // padding budget (≤4096-byte buckets, 8192-byte hard cap) on every path.
        let content = match &reply_ctx {
            Some((q, parts)) => crate::services::build_reply_content(
                text,
                attachment_pointers,
                &q.target_author_id.to_string(),
                q.target_timestamp,
                // Core-computed preview (media label / truncated text), not the
                // raw UI preview which is empty for image/gif/video parents.
                parts.reply_to_text.as_deref().unwrap_or(""),
                parts.quote_attachments.clone(),
                sent_at,
            ),
            None => crate::services::build_data_message_content(text, attachment_pointers, sent_at),
        };

        // Signal-style profile-key exchange: piggyback OUR profile key on the
        // DataMessage so the friend can decrypt our encrypted display name +
        // avatar. Auto-enables profile sharing with an accepted friend on first
        // send. Without this the peer only ever sees the "User <id>" fallback.
        let content = self.inject_profile_key_if_sharing(peer_id, content)?;

        // Disappearing messages for SEALED DMs. The server can't schedule a
        // Temporal delete for a sealed message (it can't read the envelope to
        // learn the ttl), so sealed-DM disappearing is entirely local: read
        // this conversation's timer (`conv.disappearing.server_dm.<peer>`,
        // written by `set_disappearing_timer`) and ARM the E2E marker on the
        // DataMessage so the RECIPIENT's `store_data_message` tracks + reaps
        // their local copy — and track OUR OWN copy below. `0`/absent = off.
        // Every message states the timer with its version, so the peer
        // orders it against changes it has already seen.
        let timer = crate::services::disappearing::server_dm_state(&self.settings_repo, peer_id);
        let disappearing_ttl_secs: Option<u32> = Some(timer.seconds).filter(|s| *s > 0);
        let mut content = content;
        if let Some(crate::proto::content_v1::content::Content::DataMessage(dm)) = content.content.as_mut() {
            crate::services::disappearing::stamp_timer(dm, timer);
        }

        Ok((content, reply_ctx, disappearing_ttl_secs))
    }

    /// Send a failed friend DM again: same message id, same `sent_at` (so the
    /// recipient sees one message, deduplicated on its timestamp), content
    /// rebuilt from the stored copy. A stored failure label does not block
    /// the retry. Returns the new status, `"sent"` or `"sending"` (queued
    /// behind a Shield bootstrap); on failure the bubble stays `"failed"` and
    /// the error is returned.
    pub async fn retry_server_dm(
        &self,
        message_id: &crate::domain::ids::MessageId,
    ) -> Result<&'static str, MessageError> {
        let mut message = self
            .message_repo
            .get(message_id)?
            .ok_or(MessageError::MessageNotFound(*message_id))?;
        let me = self.settings_repo.current_user_id()?;
        if message.conversation_type != ConversationType::ServerDm
            || message.sender_id != Some(me)
            || message.delivery_status != "failed"
            || message.is_deleted
        {
            return Err(MessageError::InvalidContent("only a failed friend DM can be retried".into()));
        }
        let peer_id = UserId::from_uuid(*message.conversation_id.as_uuid());

        let pointers = self.pointers_for_resend(&message.attachments).await?;
        let quote = match (message.reply_to_author_id, message.reply_to) {
            (Some(author), Some(parent_id)) => self.message_repo.get(&parent_id)?.map(|parent| {
                crate::services::gateway::outgoing::QuoteIntent {
                    target_author_id: author,
                    target_timestamp: parent.sent_at,
                    target_preview: message.reply_to_text.clone(),
                }
            }),
            _ => None,
        };

        let (content, _, _) = self.server_dm_content(
            &peer_id,
            message.as_text().unwrap_or(""),
            pointers,
            message.sent_at,
            quote.as_ref(),
        )?;
        // The devices this message was encrypted for, not the peer's devices
        // now. A device linked after the send is not in that set. No fallback
        // when the set is empty: we do not know who the original was for.
        let devices = self.original_dm_devices(peer_id, message.sent_at)?;
        let status = match self.send_friend_dm_to_original_devices(&peer_id, content, &devices).await {
            Ok(()) => "sent",
            Err(MessageError::BootstrapInitiated) => "sending",
            Err(e) => {
                message.failure_reason = e.permanent_send_failure_reason().map(str::to_owned);
                self.message_repo.update(&message)?;
                return Err(e);
            }
        };
        // `set_friend_dm_delivery_status` writes the status only, so a previous
        // failure label would stay on the bubble. Update the same row.
        message.delivery_status = status.into();
        message.failure_reason = None;
        message.updated_at = chrono::Utc::now();
        self.message_repo.update(&message)?;
        Ok(status)
    }

    /// Send a Signal-style profile-key handshake to a friend: an empty-body,
    /// no-attachment DataMessage whose only job is to carry our profile key.
    ///
    /// Fired once on BOTH sides right after friend-accept (see
    /// `inbox_processor`'s `FriendAccepted` handler and the UI's accept-request
    /// wiring) so each peer's real display name + avatar appear immediately —
    /// mirroring how the anon-DM init/accept messages piggyback the same
    /// key+encrypted-profile pair, instead of waiting for a first real
    /// message to carry it (`send_server_dm`'s `inject_profile_key_if_sharing`).
    /// The gateway recognizes the empty body and stores the key without
    /// creating a visible message (`store_data_message`).
    ///
    /// Nothing else re-sends the key until a real message carries it, so one
    /// lost send left the friend showing "User <id>". A failed send is retried
    /// with backoff ([`PROFILE_KEY_RETRY`]) until the server accepts it, the
    /// peer is no longer a friend, or the attempts run out. Not persisted: an
    /// app restart mid-retry ends it.
    pub async fn send_profile_key_handshake(&self, peer_id: &UserId) -> Result<(), MessageError> {
        retry_while(
            PROFILE_KEY_RETRY,
            move || async move {
                let content = crate::services::build_data_message_content(
                    "",
                    vec![],
                    crate::util::time::TimestampNanos::now().as_u64(),
                );
                // Auto-enables profile sharing with an accepted friend and sets
                // `profile_key` on the DataMessage.
                let content = self.inject_profile_key_if_sharing(peer_id, content)?;
                match self.send_friend_dm_content(peer_id, content).await {
                    // Shield without a mailbox yet: queued for the bootstrap
                    // flush, which delivers it.
                    Ok(()) | Err(MessageError::BootstrapInitiated) => Ok(()),
                    Err(e) => Err(e),
                }
            },
            || self.is_accepted_friend(peer_id),
        )
        .await
    }

    /// Whether `peer_id` is an accepted friend, in either direction of the
    /// original request (`UserService::is_friend_local`).
    fn is_accepted_friend(&self, peer_id: &UserId) -> bool {
        let Ok(me) = self.settings_repo.current_user_id() else { return false };
        [(me, *peer_id), (*peer_id, me)].iter().any(|(a, b)| {
            matches!(
                self.user_repo.get_friendship(a, b),
                Ok(Some(f)) if f.status == crate::domain::user::FriendshipStatus::Accepted
            )
        })
    }

    /// Change the disappearing-messages timer of the DM with `peer_id`
    /// (`0` = off), Signal-style: adopt it locally at the next version, file the "You set
    /// disappearing messages to …" notice, and announce it to the friend with
    /// a timer-update `DataMessage` so their side adopts the same value.
    ///
    /// Returns the notice (for the UI to show), or `None` when the timer
    /// already had that value — nothing is written or sent then.
    ///
    /// The local change stands even if the announcement fails: the timer rides
    /// on every later message too, and the friend adopts it from the first one
    /// that arrives.
    pub async fn set_server_dm_disappearing_timer(
        &self,
        peer_id: &UserId,
        seconds: u32,
    ) -> Result<Option<Message>, MessageError> {
        let me = self.settings_repo.current_user_id()?;
        let sent_at = crate::util::time::TimestampNanos::now().as_u64();
        let Some((notice, timer)) = crate::services::disappearing::change_server_dm_timer(
            &self.settings_repo,
            &self.message_repo,
            peer_id,
            &me,
            seconds,
            sent_at,
        )?
        else {
            return Ok(None);
        };

        let content = crate::services::disappearing::timer_update_content(timer, sent_at);
        match self.send_friend_dm_content(peer_id, content).await {
            // Shield without an established mailbox: queued for the bootstrap
            // flush, which is delivery as far as this change is concerned.
            Ok(()) | Err(MessageError::BootstrapInitiated) => Ok(Some(notice)),
            Err(e) => Err(e),
        }
    }

    /// Deliver `content` (text, receipt, typing, reaction, edit, delete, …) to
    /// a friend over the transport the current mode allows. The ONLY place a
    /// friend DM's transport is decided.
    ///
    /// Shield: the ephemeral mailbox over onion. A single receipt or typing
    /// indicator over sealed-sender streaming re-creates the recipient+timing
    /// signal the mailbox exists to remove (NDSS'21 §III-IV), so there is no
    /// fallback — a mailbox failure is this send's failure.
    /// `Err(BootstrapInitiated)` means the mailbox with this friend is not
    /// established yet: the content was queued for the bootstrap flush.
    ///
    /// Normal: sealed sender, streaming, for instant delivery.
    pub async fn send_friend_dm_content(
        &self,
        peer_id: &UserId,
        content: crate::proto::content_v1::Content,
    ) -> Result<(), MessageError> {
        if self.shield_mode.load(std::sync::atomic::Ordering::Acquire) {
            self.send_dm(peer_id, content, &self.mailbox_manager).await
        } else {
            self.send_sealed_dm_content(peer_id, content).await
        }
    }

    /// Devices of `peer` recorded for the DM sent at `sent_at`.
    fn original_dm_devices(&self, peer: UserId, sent_at: u64) -> Result<Vec<u32>, MessageError> {
        let me = self.current_user_id()?;
        let devices = crate::storage::repositories::message_receipt_repo::MessageReceiptRepository::new(
            self.database.clone(),
        )
        .targets_for_send(ChannelId::from(peer), ConversationType::ServerDm, me, sent_at)?
        .into_iter()
        .filter(|(user, _)| *user == peer)
        .map(|(_, device)| device.0)
        .collect::<Vec<_>>();
        if devices.is_empty() {
            return Err(MessageError::InvalidContent(
                "no original device of this message can be retried".into(),
            ));
        }
        Ok(devices)
    }

    /// Retry delivery to `devices` only. Does not enumerate the peer's current
    /// devices, and does not copy the message to our own other devices: those
    /// were not recipients of the original send.
    async fn send_friend_dm_to_original_devices(
        &self,
        peer_id: &UserId,
        content: crate::proto::content_v1::Content,
        devices: &[u32],
    ) -> Result<(), MessageError> {
        if self.shield_mode.load(std::sync::atomic::Ordering::Acquire) {
            self.send_shield_dm_to_devices(peer_id, content, devices).await
        } else {
            self.send_sealed_dm_to_devices(peer_id, content, devices).await
        }
    }

    /// Sealed retry to a caller-chosen device list. Encrypts each device on
    /// its own session and deposits that ciphertext. Does not snapshot again
    /// and does not ask the server who the peer's devices are now.
    async fn send_sealed_dm_to_devices(
        &self,
        peer_id: &UserId,
        content: crate::proto::content_v1::Content,
        devices: &[u32],
    ) -> Result<(), MessageError> {
        if self.shield_mode.load(std::sync::atomic::Ordering::Acquire) {
            return Err(MessageError::DmError(
                "Shield mode: refusing the sealed-sender streaming transport for a DM".into(),
            ));
        }
        if devices.is_empty() {
            return Err(MessageError::InvalidContent(
                "no original device of this message can be retried".into(),
            ));
        }
        let plaintext = content.encode_to_vec();
        let peer = peer_id.to_string();
        let mut auth_client = self.auth_client();
        let mut ciphertexts = std::collections::HashMap::new();
        let mut error = None;
        for &device_id in devices {
            match self
                .signal_service
                .encrypt_message(&mut auth_client, &peer, device_id, &plaintext)
                .await
            {
                Ok(ciphertext) => {
                    ciphertexts.insert(device_id, ciphertext);
                }
                Err(e) => record_deposit_error(
                    &mut error,
                    MessageError::from_signal_send(e, "device encryption"),
                ),
            }
        }
        if ciphertexts.is_empty() {
            return Err(error.unwrap_or_else(|| {
                MessageError::DmError(format!("Failed to encrypt the retry for {peer_id}"))
            }));
        }
        let uak = self.sealed_access_key(&peer, false);
        let mut message_client = self.message_client();
        let outcome = self
            .deposit_sealed_ciphertexts(&mut message_client, &peer, ciphertexts, uak.as_ref())
            .await;
        if outcome.delivered == 0 {
            return Err(outcome.error.unwrap_or_else(|| {
                MessageError::DmError(format!(
                    "Failed to deliver sealed DM retry to any original device of {peer_id}"
                ))
            }));
        }
        self.record_dm_send(*peer_id, &content, &outcome.deposits);
        Ok(())
    }

    /// Send an arbitrary `Content` proto (reactions, typing, pin, edit,
    /// delete, receipts, or a DataMessage) to every device of `peer_id`,
    /// sealed-sender + UAK-authorized, with no server-side channel. Also
    /// copies it to our own other devices (Signal-style multi-device sync)
    /// when `own_device_copy` gives it one.
    ///
    /// If the recipient's UAK is unavailable this degrades to identified
    /// (Basic auth) delivery, which exposes us to the server as the sender.
    /// That case is loud in the logs and never silent — see
    /// `deposit_sealed_to_all_devices`.
    ///
    /// Callers that need local storage (e.g. `send_server_dm`) do it
    /// themselves after this returns — this method only handles delivery.
    async fn send_sealed_dm_content(
        &self,
        peer_id: &UserId,
        content: crate::proto::content_v1::Content,
    ) -> Result<(), MessageError> {
        // Backstop for `send_friend_dm_content`: this transport names the
        // recipient to the server, which Shield mode must never do.
        if self.shield_mode.load(std::sync::atomic::Ordering::Acquire) {
            return Err(MessageError::DmError(
                "Shield mode: refusing the sealed-sender streaming transport for a DM".into(),
            ));
        }
        let plaintext = content.encode_to_vec();

        let mut auth_client = self.auth_client();
        let mut message_client = self.message_client();

        let peer_id_str = peer_id.to_string();

        // Recipient: every device. A device list that went stale is
        // reconciled and the send retried inside (Signal's 409/410).
        let peer = self
            .deposit_sealed_to_all_devices(&mut auth_client, &mut message_client, &peer_id_str, &plaintext)
            .await;

        if peer.delivered == 0 {
            if let Some(error) = peer.error { return Err(error); }
            return Err(MessageError::DmError(format!(
                "Failed to deliver sealed DM to any device of {peer_id}"
            )));
        }

        // What a peer device's DecryptionErrorMessage will name if it cannot
        // decrypt this (`resend_dm_on_decryption_error`).
        self.record_dm_send(*peer_id, &content, &peer.deposits);

        // Self-sync: every one of our own devices except this one.
        // One policy for both DM transports (`sync_sent_to_own_devices`).
        let own_delivered = self.sync_sent_to_own_devices(&mut auth_client, peer_id, &content).await;

        // Surface degradation at the top of the send, not just at the deposit
        // site: this is the one place that knows the whole message failed to be
        // sealed, and it must not look like an ordinary successful send.
        if peer.degraded > 0 {
            tracing::warn!(
                peer_id = %peer_id,
                peer_devices_degraded = peer.degraded,
                "SEALED SENDER DEGRADED: sealed DM was delivered identified on some \
                 devices — the server recorded us as the sender for those"
            );
        }

        tracing::debug!(
            peer_id = %peer_id,
            peer_devices_delivered = peer.delivered,
            self_devices_delivered = own_delivered,
            "Sealed DM delivery complete"
        );

        Ok(())
    }

    /// Ship a `DecryptionErrorMessage` back to the peer device whose message
    /// we could not decrypt (receiver half of the 1:1 session self-heal).
    ///
    /// Unlike a normal DM this targets exactly ONE device — the one that sent
    /// the wedged ciphertext (only its session with us is broken) — and the
    /// DEM is sealed as session-LESS `PlaintextContent` (the ratchet is what's
    /// broken; see `SignalService::encrypt_decryption_error_report`).
    ///
    /// UAK authorization is derived locally from the peer's stored profile key
    /// (Signal's `ProfileKey.deriveAccessKey()`), so no prekey-bundle fetch —
    /// and no one-time-prekey burn — is needed. Without a profile key we
    /// degrade to identified (Basic auth) delivery, loudly, like
    /// `deposit_sealed_to_all_devices` does.
    ///
    /// At most [`DEM_RETRY_LIMIT`] reports per failed message; past that
    /// this returns `Ok` without sending.
    pub async fn send_decryption_error_report(
        &self,
        peer_id: &UserId,
        peer_device_id: u32,
        dem_serialized: Vec<u8>,
    ) -> Result<(), MessageError> {
        let original_ms = libsignal_protocol::DecryptionErrorMessage::try_from(dem_serialized.as_slice())
            .map_err(|e| MessageError::InvalidContent(format!("DecryptionErrorMessage: {e}")))?
            .timestamp()
            .epoch_millis();
        if !count_dem_retry(&mut DEM_RETRIES.lock(), (*peer_id, peer_device_id, original_ms)) {
            tracing::warn!(peer_id = %peer_id, peer_device_id, original_ms,
                "DecryptionErrorMessage retry limit reached for this message; not sending another");
            return Ok(());
        }
        let mut auth_client = self.auth_client();
        let sealed_body = self
            .signal_service
            .encrypt_decryption_error_report(
                &mut auth_client,
                &peer_id.to_string(),
                peer_device_id,
                dem_serialized,
            )
            .await
            .map_err(|e| {
                MessageError::DmError(format!("failed to seal DecryptionErrorMessage: {e}"))
            })?;

        let sealed_message = SealedSenderMessage {
            sealed_body,
            server_key_id: self.signal_service.get_server_key_id(),
            timestamp: crate::util::time::TimestampNanos::now_anonymized().as_u64(),
        };

        let peer_id_str = peer_id.to_string();
        let peer_uak = self.sealed_access_key(&peer_id_str, false);

        let identified = match peer_uak {
            Some(uak) => {
                let mut anon_client = self.grpc_pool.anonymous_message_client();
                match anon_client
                    .send_sealed_message_with_uak(&peer_id_str, peer_device_id, sealed_message.clone(), &uak)
                    .await
                {
                    Ok(_) => false,
                    // Our stored key is stale: the peer rotated it
                    // (`profile_visibility`), so its UAK moved with it.
                    Err(e) if e.is_auth_error() => {
                        tracing::warn!(peer_id = %peer_id, reason = "UAK from our stored profile key rejected", "SEALED SENDER DEGRADED: DecryptionErrorMessage delivered identified");
                        true
                    }
                    Err(e) => return Err(MessageError::Network(e)),
                }
            }
            // To our own user the deposit is identified by design (see
            // `deposit_sealed_to_session_devices`), not a degradation.
            None if self.current_user_id().is_ok_and(|me| me == *peer_id) => true,
            None => {
                tracing::warn!(
                    peer_id = %peer_id,
                    reason = "no stored profile key to derive the peer's UAK",
                    "SEALED SENDER DEGRADED: DecryptionErrorMessage delivered identified \
                     (Basic auth) — the server WILL record us as the sender"
                );
                true
            }
        };
        if identified {
            let mut message_client = self.message_client();
            message_client
                .send_sealed_message(&peer_id_str, peer_device_id, sealed_message)
                .await
                .map_err(MessageError::Network)?;
        }

        tracing::info!(
            peer_id = %peer_id,
            peer_device_id,
            "DecryptionErrorMessage report delivered — peer will archive its session and re-establish"
        );
        Ok(())
    }

    /// Encrypt `plaintext` for every device of `recipient_id` and deposit
    /// each ciphertext via `SendSealedMessage`, UAK-authorized. `skip_device_id`
    /// excludes a single device (used for self-sync to skip the sending device)
    /// before any session is built or message encrypted for it; if it was the
    /// only device, the outcome is `delivered: 0` with no error.
    ///
    /// Returns how many devices were delivered to, and how many of those were
    /// delivered in the DEGRADED identified-sender mode — see [`DepositOutcome`].
    /// Every per-device failure is logged — dropped devices are never silently
    /// capped or swallowed.
    ///
    /// Shared by 1:1 DMs and by channel-member traffic (SKDMs, sender-key
    /// requests, control fanout — see `group.rs` / `core.rs`): both go to
    /// recipients who already know who we are, so they spend no anonymous
    /// mailbox token (OPEN-ITEMS §120).
    ///
    /// Signal's device-list reconciliation: every request names the whole
    /// device set, the server answers a set that is not the recipient's
    /// registered devices with the difference and queues nothing, and we then
    /// fix our sessions (`SignalService::reconcile_devices`) and send again,
    /// once. A second mismatch is a failed send.
    pub(super) async fn deposit_sealed_to_all_devices(
        &self,
        auth_client: &mut AuthClient,
        message_client: &mut MessageClient,
        recipient_id: &str,
        plaintext: &[u8],
    ) -> DepositOutcome {
        let mismatch = match self
            .deposit_sealed_to_session_devices(auth_client, message_client, recipient_id, plaintext)
            .await
        {
            Ok(outcome) => return outcome,
            Err(mismatch) => mismatch,
        };
        tracing::info!(
            recipient = recipient_id,
            %mismatch,
            "Sealed send: recipient's devices changed — reconciling sessions and retrying once"
        );
        let crate::error::NetworkError::MismatchedDevices { missing, extra, stale } = &mismatch else {
            return DepositOutcome { error: Some(MessageError::Network(mismatch)), ..Default::default() };
        };
        if let Err(e) = self
            .signal_service
            .reconcile_devices(auth_client, recipient_id, missing, extra, stale)
            .await
        {
            let rate_limited = matches!(
                e,
                crate::error::service_error::ServiceError::Network(
                    crate::error::NetworkError::RateLimited
                )
            );
            tracing::warn!(
                recipient = recipient_id,
                error = %e,
                rate_limited,
                "Sealed send: could not reconcile the recipient's devices"
            );
            return DepositOutcome { error: Some(MessageError::from_signal_send(e, "device reconciliation")), ..Default::default() };
        }
        match self
            .deposit_sealed_to_session_devices(auth_client, message_client, recipient_id, plaintext)
            .await
        {
            Ok(outcome) => outcome,
            Err(mismatch) => {
                tracing::warn!(
                    recipient = recipient_id,
                    %mismatch,
                    "Sealed send: devices still mismatched after reconciling — not sent"
                );
                self.signal_service.forget_device_list(recipient_id);
                DepositOutcome { error: Some(MessageError::Network(mismatch)), ..Default::default() }
            }
        }
    }

    /// One attempt of [`Self::deposit_sealed_to_all_devices`]. `Err` is the
    /// server's `NetworkError::MismatchedDevices`.
    async fn deposit_sealed_to_session_devices(
        &self,
        auth_client: &mut AuthClient,
        message_client: &mut MessageClient,
        recipient_id: &str,
        plaintext: &[u8],
    ) -> Result<DepositOutcome, crate::error::NetworkError> {
        let (sealed, unrestricted) = match self
            .signal_service
            .encrypt_message_all_devices_with_access_policy(auth_client, recipient_id, plaintext, true)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                let rate_limited = matches!(
                    e,
                    crate::error::service_error::ServiceError::Network(
                        crate::error::NetworkError::RateLimited
                    )
                );
                tracing::warn!(
                    recipient = recipient_id,
                    error = %e,
                    rate_limited,
                    "Sealed DM: failed to encrypt for recipient's devices"
                );
                // No request went out, so the server had nothing to answer:
                // the next send re-fetches the whole device list.
                if !rate_limited {
                    self.signal_service.forget_device_list(recipient_id);
                }
                return Ok(DepositOutcome { error: Some(MessageError::from_signal_send(e, "device encryption")), ..Default::default() });
            }
        };
        if let (Ok(peer), Ok(content)) = (
            recipient_id.parse::<UserId>(), crate::proto::content_v1::Content::decode(plaintext),
        ) {
            self.snapshot_dm_content_targets(peer, &content, &sealed.keys().copied().collect::<Vec<_>>())
                .map_err(|error| crate::error::NetworkError::ServerError(format!("persist receipt targets: {error}")))?;
        }
        let attempted = !sealed.is_empty();
        // Each device under the registration id of the session that
        // encrypted to it — never 0, which would skip the stale check.
        let registration_ids: std::collections::HashMap<u32, u32> =
            sealed.iter().map(|(&device_id, s)| (device_id, s.registration_id)).collect();
        let device_set = registration_ids
            .iter()
            .map(|(&device_id, &registration_id)| DestinationDevice { device_id, registration_id })
            .collect();
        let ciphertexts = sealed.into_iter().map(|(device_id, s)| (device_id, s.ciphertext)).collect();

        // To our own user (self-sync, SKDM, SenderKeyRequest) the deposit is
        // identified on the authenticated channel, never UAK on the anonymous
        // one — see `sync_to_own_devices`. That is not a degradation.
        let is_self = self.current_user_id().is_ok_and(|me| me.to_string() == recipient_id);
        let recipient_uak = if is_self { None } else { self.sealed_access_key(recipient_id, unrestricted) };

        // DEGRADATION, not a normal path: we hold no profile key for this
        // recipient and they do not accept sealed sender from anyone. We can
        // still deliver — but only over Basic auth, which makes the server
        // stamp our user id into `device_messages.source_user_id` and defeats
        // sealed sender entirely. Logged loudly and per-recipient on purpose:
        // a silent fallback here would report "sealed DM sent" while shipping
        // the exact opposite.
        if recipient_uak.is_none() && !is_self {
            tracing::warn!(
                recipient = recipient_id,
                reason = "no profile key held for the recipient to derive their UAK",
                "SEALED SENDER DEGRADED: falling back to identified (Basic auth) \
                 delivery — the server WILL record us as the sender of this message"
            );
        }

        let mut outcome = self
            .deposit_sealed_ciphertexts_to_set(
                message_client,
                recipient_id,
                ciphertexts,
                recipient_uak.as_ref(),
                device_set,
            )
            .await?;
        if recipient_uak.is_none() && !is_self {
            outcome.degraded = outcome.delivered;
        }
        outcome.registration_ids = registration_ids;
        if attempted && outcome.delivered == 0 {
            self.signal_service.forget_device_list(recipient_id);
        }
        Ok(outcome)
    }

    /// The `Unidentified-Access-Key` to seal a deposit to `recipient_id` with,
    /// as Signal's `UnidentifiedAccessUtil` picks it: derived from the profile
    /// key they shared with us; else, when they accept sealed sender from
    /// anyone, random bytes the server does not check; else `None` (deliver
    /// identified). The server never serves a UAK — holding the profile key is
    /// what authorizes a sealed sender.
    pub(super) fn sealed_access_key(&self, recipient_id: &str, unrestricted: bool) -> Option<[u8; 16]> {
        let profile_key = recipient_id.parse::<UserId>().ok().and_then(|peer| {
            let me = self.settings_repo.current_user_id().ok()?;
            self.user_repo.get_friend_profile_key(&me, &peer).ok().flatten()
        });
        pick_sealed_access_key(profile_key.as_ref(), unrestricted)
    }

    /// Deposit each device's sealed ciphertext via `SendSealedMessage`:
    /// UAK-authorized on the anonymous channel with `uak`, identified on
    /// `message_client` without — or when the server rejects the UAK (our
    /// copy of the recipient's profile key is stale: they rotated it). Returns
    /// how many devices it reached and how many of those went identified; a
    /// device the server has retired gets its session archived.
    ///
    /// The devices are addressed one by one with no device set named, so the
    /// server compares nothing: for a caller that chose its devices itself.
    pub(super) async fn deposit_sealed_ciphertexts(
        &self,
        message_client: &mut MessageClient,
        recipient_id: &str,
        ciphertexts: std::collections::HashMap<u32, Vec<u8>>,
        uak: Option<&[u8; 16]>,
    ) -> DepositOutcome {
        self.deposit_sealed_ciphertexts_to_set(message_client, recipient_id, ciphertexts, uak, Vec::new())
            .await
            .unwrap_or_else(|error| DepositOutcome { error: Some(MessageError::Network(error)), ..Default::default() })
    }

    /// [`Self::deposit_sealed_ciphertexts`] with `device_set` named on every
    /// request. `Err` is the server's `NetworkError::MismatchedDevices`: the
    /// set is not the recipient's registered devices and nothing was queued.
    async fn deposit_sealed_ciphertexts_to_set(
        &self,
        message_client: &mut MessageClient,
        recipient_id: &str,
        ciphertexts: std::collections::HashMap<u32, Vec<u8>>,
        uak: Option<&[u8; 16]>,
        device_set: Vec<DestinationDevice>,
    ) -> Result<DepositOutcome, crate::error::NetworkError> {
        let timestamp = crate::util::time::TimestampNanos::now_anonymized().as_u64();
        let server_key_id = self.signal_service.get_server_key_id();
        let mut anon_client = uak.map(|_| self.grpc_pool.anonymous_message_client());

        let mut delivered = 0;
        let mut degraded = 0;
        let mut deposits = Vec::new();
        let mut error = None;
        for (device_id, sealed_bytes) in ciphertexts {
            let sealed_message = SealedSenderMessage {
                sealed_body: sealed_bytes,
                server_key_id,
                timestamp,
            };

            // Deposit UAK-authorized on a RAW channel. Friend DMs are sealed
            // even though the server already stores the friendship: a static
            // friendship graph is not a behavioural delivery log. Knowing "A and
            // B are friends" is a fact B chose to publish to the server; knowing
            // "A sent B a message at 14:03, then three more by 14:05" is a
            // timing-and-frequency record neither of them agreed to. Signal
            // seals to known contacts for exactly this reason, and so do we.
            //
            // This deliberately does NOT go through `message_client`: that
            // client is interceptor-wrapped and would attach Basic auth next to
            // the UAK, which the server rejects as multiple auth methods.
            let mut fell_back = false;
            let result = match (uak, anon_client.as_mut()) {
                (Some(uak), Some(anon_client)) => {
                    match anon_client
                        .send_sealed_message_to_device_set_with_uak(
                            recipient_id,
                            device_id,
                            sealed_message.clone(),
                            device_set.clone(),
                            uak,
                        )
                        .await
                    {
                        Err(e) if e.is_auth_error() => {
                            tracing::warn!(
                                recipient = recipient_id,
                                device_id,
                                reason = "UAK from our stored profile key rejected",
                                "SEALED SENDER DEGRADED: delivering identified (Basic auth)"
                            );
                            fell_back = true;
                            message_client
                                .send_sealed_message_to_device_set(
                                    recipient_id,
                                    device_id,
                                    sealed_message,
                                    device_set.clone(),
                                )
                                .await
                        }
                        other => other,
                    }
                }
                _ => {
                    message_client
                        .send_sealed_message_to_device_set(
                            recipient_id,
                            device_id,
                            sealed_message,
                            device_set.clone(),
                        )
                        .await
                }
            };

            match result {
                // Every request of this send names the same set, and the
                // server checks it before queuing, so the first one is
                // refused and nothing has been queued — unless the
                // recipient's registered devices changed between two of
                // these requests; then the devices already reached get the
                // retry too (peers drop it by (sender, sent_at)).
                Err(e @ crate::error::NetworkError::MismatchedDevices { .. }) => return Err(e),
                Ok(response) => {
                    delivered += 1;
                    degraded += usize::from(fell_back);
                    deposits.push((device_id, response.server_timestamp));
                }
                // The server no longer knows this device (retired by Log Out /
                // unlink). Signal's 410 handling: archive the session so we
                // stop encrypting to it, and treat it as gone rather than as a
                // delivery failure.
                Err(e) if e.is_unknown_destination_device() => {
                    record_deposit_error(&mut error, MessageError::Network(e));
                    tracing::info!(
                        recipient = recipient_id,
                        device_id,
                        "Sealed send: device retired on server — archiving its session"
                    );
                    if let Err(e) = self
                        .signal_service
                        .actor
                        .archive_session_for_resend(recipient_id.to_string(), device_id, None)
                        .await
                    {
                        tracing::warn!(
                            recipient = recipient_id,
                            device_id,
                            error = %e,
                            "Failed to archive session for retired device"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        recipient = recipient_id,
                        device_id,
                        error = %e,
                        "Sealed DM: dropped device — failed to deposit to inbox"
                    );
                    record_deposit_error(&mut error, MessageError::Network(e));
                }
            }
        }

        Ok(DepositOutcome { delivered, degraded, error, deposits, ..Default::default() })
    }
}

// If any destination failed transiently, a retry can still make progress.
pub(super) fn record_deposit_error(current: &mut Option<MessageError>, error: MessageError) {
    if current.is_none() || error.permanent_send_failure_reason().is_none() {
        *current = Some(error);
    }
}

/// See [`MessageService::sealed_access_key`].
fn pick_sealed_access_key(profile_key: Option<&[u8; 32]>, unrestricted: bool) -> Option<[u8; 16]> {
    match profile_key {
        Some(pk) => Some(crate::crypto::derive_uak(pk)),
        None if unrestricted => {
            let mut key = [0u8; 16];
            rand::RngCore::fill_bytes(&mut rand::rng(), &mut key);
            Some(key)
        }
        None => None,
    }
}

/// Result of depositing one sealed DM to a recipient's devices.
///
/// `degraded` is tracked separately from `delivered` so a caller can never read
/// "it worked" without also being able to see that the sender was exposed to the
/// server on some of those deliveries.
#[derive(Debug, Default)]
pub(super) struct DepositOutcome {
    /// Devices the message actually reached.
    pub(super) delivered: usize,
    /// Preserve the typed failure when no device received the message.
    pub(super) error: Option<MessageError>,
    /// Of those, how many were sent identified (Basic auth) because no UAK was
    /// available — i.e. sealed sender did NOT engage and the server recorded us.
    pub(super) degraded: usize,
    /// `(device, server timestamp in epoch nanos)` of each deposit: the
    /// timestamp that device's inbox hands it the envelope under.
    pub(super) deposits: Vec<(u32, u64)>,
    /// Registration id of the session each device was encrypted with, for
    /// a deposit that named its device set
    /// ([`MessageService::deposit_sealed_to_all_devices`]); empty otherwise.
    pub(super) registration_ids: std::collections::HashMap<u32, u32>,
}

/// `DecryptionErrorMessage`s sent per failed message: Signal-Desktop's
/// `RETRY_LIMIT` (`ts/util/handleRetry.preload.ts:55`). An envelope the
/// inbox keeps re-delivering, or a sender that keeps answering with
/// undecryptable resends, stops costing a report after this many.
const DEM_RETRY_LIMIT: u32 = 5;

/// Distinct messages counted before the map is cleared wholesale, as
/// `group_retry_requests` is (`group.rs`). A forgotten count costs at most
/// `DEM_RETRY_LIMIT` more reports for that message.
const DEM_RETRY_CAP: usize = 4096;

/// Reports sent per `(sender, sender device, original timestamp in epoch
/// millis)` — the key of Signal-Desktop's `retryRecord`, in memory as there.
/// Every DEM goes out through `send_decryption_error_report` (1:1 from the
/// inbox processor, sender-key from `request_group_resend`), so the count is
/// kept there.
static DEM_RETRIES: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<(UserId, u32, u64), u32>>,
> = std::sync::LazyLock::new(Default::default);

/// Count one more report for `key`; `false` once it is past the limit.
fn count_dem_retry(retries: &mut std::collections::HashMap<(UserId, u32, u64), u32>, key: (UserId, u32, u64)) -> bool {
    if retries.len() >= DEM_RETRY_CAP && !retries.contains_key(&key) {
        retries.clear();
    }
    let count = retries.entry(key).or_insert(0);
    *count += 1;
    *count <= DEM_RETRY_LIMIT
}

/// Attempts and backoff for [`MessageService::send_profile_key_handshake`]:
/// 1s, 2s, 4s … capped at 60s, eight attempts (about two minutes).
const PROFILE_KEY_RETRY: Backoff = Backoff {
    attempts: 8,
    first: std::time::Duration::from_secs(1),
    max: std::time::Duration::from_secs(60),
};

#[derive(Clone, Copy)]
struct Backoff {
    attempts: u32,
    first: std::time::Duration,
    max: std::time::Duration,
}

/// Run `attempt` until it succeeds, retrying with doubling delays while
/// `wanted()` still holds after each wait. Returns the last error otherwise.
async fn retry_while<A, Fut>(
    backoff: Backoff,
    mut attempt: A,
    wanted: impl Fn() -> bool,
) -> Result<(), MessageError>
where
    A: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), MessageError>>,
{
    let mut delay = backoff.first;
    let mut n = 1;
    loop {
        let error = match attempt().await {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        if n >= backoff.attempts {
            return Err(error);
        }
        tracing::warn!(attempt = n, retry_in_ms = delay.as_millis() as u64, %error, "send failed; retrying");
        tokio::time::sleep(delay).await;
        if !wanted() {
            return Err(error);
        }
        delay = (delay * 2).min(backoff.max);
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::pick_sealed_access_key;

    #[test]
    fn decryption_error_reports_stop_after_five_per_message() {
        use super::{count_dem_retry, DEM_RETRY_CAP, DEM_RETRY_LIMIT};
        use crate::domain::ids::UserId;
        let mut retries = std::collections::HashMap::new();
        let (peer, ts) = (UserId::new(), 1_700_000_000_000);
        for _ in 0..DEM_RETRY_LIMIT {
            assert!(count_dem_retry(&mut retries, (peer, 2, ts)));
        }
        assert!(!count_dem_retry(&mut retries, (peer, 2, ts)), "sixth report for one message");
        assert!(count_dem_retry(&mut retries, (peer, 3, ts)), "another device's message");
        assert!(count_dem_retry(&mut retries, (peer, 2, ts + 1)), "another message");

        for i in 0..DEM_RETRY_CAP as u64 {
            count_dem_retry(&mut retries, (peer, 9, i));
        }
        assert!(retries.len() <= DEM_RETRY_CAP, "bounded");
    }

    #[tokio::test]
    async fn retry_while_retries_until_success_and_stops_when_unwanted() {
        use super::{retry_while, Backoff, MessageError};
        use std::cell::Cell;
        let quick = Backoff {
            attempts: 5,
            first: std::time::Duration::from_millis(1),
            max: std::time::Duration::from_millis(2),
        };
        let fail = || MessageError::DmError("transient".into());

        // Two transient failures, then delivered.
        let calls = Cell::new(0);
        let ok = retry_while(quick, || {
            calls.set(calls.get() + 1);
            let n = calls.get();
            async move { if n < 3 { Err(fail()) } else { Ok(()) } }
        }, || true).await;
        assert!(ok.is_ok());
        assert_eq!(calls.get(), 3);

        // Bounded: gives up after `attempts`.
        let calls = Cell::new(0);
        assert!(retry_while(quick, || { calls.set(calls.get() + 1); async { Err(fail()) } }, || true).await.is_err());
        assert_eq!(calls.get(), 5);

        // No longer a friend: no further attempt.
        let calls = Cell::new(0);
        assert!(retry_while(quick, || { calls.set(calls.get() + 1); async { Err(fail()) } }, || false).await.is_err());
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn deposit_failure_keeps_typed_retry_and_permanent_causes() {
        use super::{record_deposit_error, DepositOutcome, MessageError};
        use crate::error::NetworkError;
        for cause in [NetworkError::RateLimited, NetworkError::Timeout,
            NetworkError::MismatchedDevices { missing: vec![2], extra: vec![], stale: vec![] }] {
            let result = DepositOutcome { error: Some(MessageError::Network(cause)), ..Default::default() };
            assert_eq!(result.error.unwrap().permanent_send_failure_reason(), None);
        }
        let mut cause = None;
        record_deposit_error(&mut cause, MessageError::Network(NetworkError::PermissionDenied));
        assert_eq!(cause.as_ref().unwrap().permanent_send_failure_reason(), Some("Not delivered"));
        record_deposit_error(&mut cause, MessageError::Network(NetworkError::Unavailable));
        record_deposit_error(&mut cause, MessageError::Network(NetworkError::InvalidRequest("too large".into())));
        assert_eq!(cause.unwrap().permanent_send_failure_reason(), None);
    }


    #[test]
    fn sealed_access_key_comes_from_the_profile_key_we_hold() {
        let pk = [7u8; 32];
        let want = crate::crypto::derive_uak(&pk);
        // Held key wins even when the recipient is unrestricted.
        assert_eq!(pick_sealed_access_key(Some(&pk), false), Some(want));
        assert_eq!(pick_sealed_access_key(Some(&pk), true), Some(want));
        // Unrestricted and no key: any key is accepted, so seal anyway.
        assert!(pick_sealed_access_key(None, true).is_some());
        // Neither: no key to present — the caller delivers identified.
        assert_eq!(pick_sealed_access_key(None, false), None);
    }
}
