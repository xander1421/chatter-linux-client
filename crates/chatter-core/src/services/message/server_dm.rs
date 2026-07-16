//! Server DM operations — sealed-sender, per-device, channel-free.
//!
//! Chatter's DM model mirrors Signal's 1:1 messaging: there is no
//! server-side DM channel. A message is Signal-Protocol-encrypted once per
//! recipient device and deposited directly into that device's inbox via
//! `SendSealedMessage`, authorized with the recipient's Unidentified Access
//! Key (read off their prekey bundle) rather than our own credentials — so
//! the server learns the recipient but never the sender. This is the same
//! primitive `fanout_control_to_device_inboxes` uses for channel control
//! messages, just without a channel to read
//! membership from. The server never creates or reads a DM channel row;
//! the local conversation is keyed by the peer's `UserId`
//! (`ConversationHandle`/`ActiveConversation::ServerDm { peer_id }`, local
//! storage key `ChannelId::from(peer_id)`).
//!
//! This intentionally does NOT reuse the `dm.rs` mailbox path: that path
//! is the NDSS'21 ephemeral-mailbox transport (two encryption layers,
//! rotating mailbox addresses, bootstrap handshake) used for
//! `ConversationType::MailboxDm` — a stronger, slower-to-establish privacy
//! model for peers without a server-visible friendship. `ServerDm` is for
//! server-known friends: standard Signal 1:1 sealed sender, no bootstrap.


use prost::Message as ProstMessage;

use crate::domain::ids::{AttachmentId, ChannelId, UserId};
use crate::domain::message::{ConversationType, Message, MessageContent, MixedContent};
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::message_client::MessageClient;
use crate::proto::chat_v1::SealedSenderMessage;
use crate::proto::content_v1::{content, AttachmentPointer, Content, SyncSent};
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

        self.send_sealed_dm_content(peer_id, content).await?;

        let current_user_id = self.settings_repo.current_user_id()?;
        let conversation_id: ChannelId = (*peer_id).into();
        let mut message = Message::new_text(
            conversation_id,
            ConversationType::ServerDm,
            Some(current_user_id),
            text.to_string(),
            sent_at,
        );

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

        if let Err(e) = self.message_repo.insert(&message) {
            tracing::warn!(
                peer_id = %peer_id,
                error = %e,
                "Failed to store sent ServerDm locally"
            );
        }

        Ok(DmSendResult {
            message: Some(message),
            attachment_ids,
            gif_attachment_ids: vec![],
            sent_at,
            bootstrap_initiated: false,
        })
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
    pub async fn send_profile_key_handshake(&self, peer_id: &UserId) -> Result<(), MessageError> {
        let content = crate::services::build_data_message_content(
            "",
            vec![],
            crate::util::time::TimestampNanos::now().as_u64(),
        );

        // Auto-enables profile sharing with an accepted friend and sets
        // `profile_key` on the DataMessage.
        let content = self.inject_profile_key_if_sharing(peer_id, content)?;

        self.send_sealed_dm_content(peer_id, content).await
    }

    /// Send an arbitrary `Content` proto (reactions, typing, pin, edit,
    /// delete, receipts, or a DataMessage) to every device of `peer_id`,
    /// sealed-sender + UAK-authorized, with no server-side channel. Also
    /// fans the same content out to our own other devices (Signal-style
    /// multi-device sync), sealed the same way.
    ///
    /// If the recipient's UAK is unavailable this degrades to identified
    /// (Basic auth) delivery, which exposes us to the server as the sender.
    /// That case is loud in the logs and never silent — see
    /// `deposit_sealed_to_all_devices`.
    ///
    /// Callers that need local storage (e.g. `send_server_dm`) do it
    /// themselves after this returns — this method only handles delivery.
    pub async fn send_sealed_dm_content(
        &self,
        peer_id: &UserId,
        content: crate::proto::content_v1::Content,
    ) -> Result<(), MessageError> {
        let plaintext = content.encode_to_vec();

        // Self-sync copy: for the TEXT DM path, the copy we ship to our OWN other
        // devices is NOT the byte-identical DataMessage the recipient got. That
        // bare copy carries no record of who it was for, so a receiving device
        // files it under a conversation with ourselves (bug F8). Instead we wrap
        // it in a `SyncSent` that names the real recipient as `destination`,
        // exactly as Signal's `SyncMessage.Sent.destination` does, so our other
        // devices file it as our OUTGOING message in the right conversation.
        //
        // SCOPE: the self-sync copy is decided per content type. The other
        // control types (reaction, read receipt, pin, edit, delete-for-everyone,
        // delete-for-me) are message-anchored by Signal's `(author, ns-timestamp)`
        // key or carry their own `conversation_id`, so the bare copy files
        // correctly on the linked device with no destination on the wire — see
        // the receive side (dispatch_control_content / inbox_processor).
        //
        // `None` means: do NOT self-sync this content to our other devices.
        let self_plaintext: Option<Vec<u8>> = match &content.content {
            // Text/attachment DataMessage → Signal-style `SyncSent` wrapper (F8
            // fix, V13): carries the real destination so our other devices file
            // it as our OUTGOING message in the peer's conversation.
            Some(content::Content::DataMessage(dm))
                if (!dm.body.is_empty() || !dm.attachments.is_empty()) && dm.delete.is_none() =>
            {
                let sync = Content {
                    timestamp: content.timestamp,
                    content: Some(content::Content::SyncSent(SyncSent {
                        destination_user_id: peer_id.to_string(),
                        timestamp: dm.timestamp,
                        message: Some(dm.clone()),
                    })),
                    ..Default::default()
                };
                Some(sync.encode_to_vec())
            }
            // Typing: never sync your OWN typing to your other devices. Signal
            // does not, and a linked device echoing "you are typing" in a phantom
            // self-thread is noise, not a feature. Drop the self-copy entirely.
            Some(content::Content::Typing(_)) => None,
            // Delivery receipts are auto-acks ("I received your message"). A copy
            // on our other devices has nothing meaningful to display (delivery
            // status is a sender-side concept about the PEER's messages) and would
            // mis-file on the A-self thread. Drop it. Read receipts ARE synced —
            // the receive side turns a self-synced read receipt into a local
            // mark-read so the unread badge clears across our devices.
            Some(content::Content::Receipt(r))
                if r.r#type == crate::proto::content_v1::receipt_message::Type::Delivery as i32 =>
            {
                None
            }
            _ => Some(plaintext.clone()),
        };

        let mut auth_client = self.auth_client();
        let mut message_client = self.message_client();

        let our_user_id = self.settings_repo.current_user_id()?;
        let our_device_id = self
            .settings_repo
            .get::<u32>(crate::storage::repositories::settings_repo::keys::CURRENT_DEVICE_ID)
            .map_err(MessageError::Storage)?
            .ok_or(MessageError::NotLoggedIn)?;

        let peer_id_str = peer_id.to_string();

        // Recipient: every device. Chatter's analog of Signal's 409/410 —
        // if we reach zero devices (stale/mismatched session state),
        // refetch the peer's device list/prekeys and retry once.
        let mut peer = self
            .deposit_sealed_to_all_devices(&mut auth_client, &mut message_client, &peer_id_str, &plaintext, None)
            .await;

        if peer.delivered == 0 {
            tracing::warn!(
                peer_id = %peer_id,
                "Sealed DM send reached zero peer devices, refetching prekeys and retrying once"
            );
            peer = self
                .deposit_sealed_to_all_devices(&mut auth_client, &mut message_client, &peer_id_str, &plaintext, None)
                .await;
        }

        if peer.delivered == 0 {
            return Err(MessageError::DmError(format!(
                "Failed to deliver sealed DM to any device of {peer_id}"
            )));
        }

        // Self-sync: every one of our own devices except this one. Ships the
        // SyncSent-wrapped copy on the text DM path (F8 fix, V13) or the bare
        // (message-anchored) copy for other control types. `None` means this
        // content type is deliberately not self-synced (typing, delivery receipt).
        let own = match self_plaintext.as_ref() {
            Some(bytes) => {
                self.deposit_sealed_to_all_devices(
                    &mut auth_client,
                    &mut message_client,
                    &our_user_id.to_string(),
                    bytes,
                    Some(our_device_id),
                )
                .await
            }
            None => DepositOutcome::default(),
        };

        // Surface degradation at the top of the send, not just at the deposit
        // site: this is the one place that knows the whole message failed to be
        // sealed, and it must not look like an ordinary successful send.
        if peer.degraded > 0 || own.degraded > 0 {
            tracing::warn!(
                peer_id = %peer_id,
                peer_devices_degraded = peer.degraded,
                self_devices_degraded = own.degraded,
                "SEALED SENDER DEGRADED: sealed DM was delivered identified on some \
                 devices — the server recorded us as the sender for those"
            );
        }

        tracing::debug!(
            peer_id = %peer_id,
            peer_devices_delivered = peer.delivered,
            self_devices_delivered = own.delivered,
            "Sealed DM delivery complete"
        );

        Ok(())
    }

    /// Encrypt `plaintext` for every device of `recipient_id` and deposit
    /// each ciphertext via `SendSealedMessage`, UAK-authorized. `skip_device_id`
    /// excludes a single device (used for self-sync to skip the sending device).
    ///
    /// Returns how many devices were delivered to, and how many of those were
    /// delivered in the DEGRADED identified-sender mode — see [`DepositOutcome`].
    /// Every per-device failure is logged — dropped devices are never silently
    /// capped or swallowed.
    async fn deposit_sealed_to_all_devices(
        &self,
        auth_client: &mut AuthClient,
        message_client: &mut MessageClient,
        recipient_id: &str,
        plaintext: &[u8],
        skip_device_id: Option<u32>,
    ) -> DepositOutcome {
        let (ciphertexts, recipient_uak) = match self
            .signal_service
            .encrypt_message_all_devices_with_uak(auth_client, recipient_id, plaintext)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    recipient = recipient_id,
                    error = %e,
                    "Sealed DM: failed to encrypt for recipient's devices"
                );
                return DepositOutcome::default();
            }
        };

        // DEGRADATION, not a normal path. The UAK rides on the recipient's
        // prekey bundle, which the server populates for every registered user,
        // so `None` means the server omitted it (missing/corrupt UAK row, or an
        // old server). We can still deliver — but only over Basic auth, which
        // makes the server stamp our user id into
        // `device_messages.source_user_id` and defeats sealed sender entirely.
        // This is logged loudly and per-recipient on purpose: a silent fallback
        // here would report "sealed DM sent" while shipping the exact opposite,
        // which is the failure mode this path exists to prevent.
        if recipient_uak.is_none() {
            tracing::warn!(
                recipient = recipient_id,
                reason = "prekey bundle carried no unidentified_access_key",
                "SEALED SENDER DEGRADED: falling back to identified (Basic auth) \
                 delivery — the server WILL record us as the sender of this message"
            );
        }

        let timestamp = crate::util::time::TimestampNanos::now().as_u64();
        let server_key_id = self.signal_service.get_server_key_id();
        let mut anon_client = self.grpc_pool.anonymous_message_client();

        let mut outcome = DepositOutcome::default();
        for (device_id, sealed_bytes) in ciphertexts {
            if skip_device_id == Some(device_id) {
                continue;
            }

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
            let result = match &recipient_uak {
                Some(uak) => {
                    anon_client
                        .send_sealed_message_with_uak(recipient_id, device_id, sealed_message, uak)
                        .await
                }
                None => {
                    message_client
                        .send_sealed_message(recipient_id, device_id, sealed_message)
                        .await
                }
            };

            match result {
                Ok(_) => {
                    outcome.delivered += 1;
                    if recipient_uak.is_none() {
                        outcome.degraded += 1;
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        recipient = recipient_id,
                        device_id,
                        error = %e,
                        "Sealed DM: dropped device — failed to deposit to inbox"
                    );
                }
            }
        }

        outcome
    }
}

/// Result of depositing one sealed DM to a recipient's devices.
///
/// `degraded` is tracked separately from `delivered` so a caller can never read
/// "it worked" without also being able to see that the sender was exposed to the
/// server on some of those deliveries.
#[derive(Debug, Default, Clone, Copy)]
struct DepositOutcome {
    /// Devices the message actually reached.
    delivered: usize,
    /// Of those, how many were sent identified (Basic auth) because no UAK was
    /// available — i.e. sealed sender did NOT engage and the server recorded us.
    degraded: usize,
}
