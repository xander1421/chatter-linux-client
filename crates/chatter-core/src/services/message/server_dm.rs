//! Server DM operations — sealed-sender, per-device, channel-free.
//!
//! Chatter's DM model mirrors Signal's 1:1 messaging: there is no
//! server-side DM channel. A message is Signal-Protocol-encrypted once per
//! recipient device and deposited directly into that device's inbox via
//! `SendSealedMessage` (UAK-authorized when the recipient's prekey bundle
//! carries one) — the same primitive `fanout_control_to_device_inboxes`
//! uses for channel control messages, just without a channel to read
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

use std::collections::HashMap;

use prost::Message as ProstMessage;

use crate::domain::ids::{AttachmentId, ChannelId, UserId};
use crate::domain::message::{ConversationType, Message, MessageContent, MixedContent};
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::message_client::MessageClient;
use crate::proto::chat_v1::{self, SealedSenderMessage};
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
    ) -> Result<DmSendResult, MessageError> {
        let trimmed = text.trim();
        if trimmed.is_empty() && attachment_pointers.is_empty() {
            return Err(MessageError::InvalidContent(
                "message must have text or attachments".to_string(),
            ));
        }

        tracing::info!(
            peer_id = %peer_id,
            text_len = trimmed.len(),
            attachment_count = attachment_pointers.len(),
            sent_at,
            "MessageService: send_server_dm called"
        );

        let attachment_ids: Vec<AttachmentId> = attachment_pointers
            .iter()
            .map(|p| AttachmentId::from_cdn_key(&p.cdn_key))
            .collect();

        let content = crate::services::build_data_message_content(
            trimmed,
            attachment_pointers,
            sent_at,
        );

        self.send_sealed_dm_content(peer_id, content).await?;

        let current_user_id = self.settings_repo.current_user_id()?;
        let conversation_id: ChannelId = (*peer_id).into();
        let mut message = Message::new_text(
            conversation_id,
            ConversationType::ServerDm,
            Some(current_user_id),
            trimmed.to_string(),
            sent_at,
        );

        if !attachment_ids.is_empty() {
            message.attachments = attachment_ids.clone();
            message.content = if trimmed.is_empty() {
                MessageContent::Mixed(MixedContent::attachments_only(attachment_ids.clone()))
            } else {
                MessageContent::Mixed(MixedContent::new(trimmed.to_string(), attachment_ids.clone()))
            };
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

    /// Send an arbitrary `Content` proto (reactions, typing, pin, edit,
    /// delete, receipts, or a DataMessage) to every device of `peer_id`,
    /// sealed-sender + UAK-authorized, with no server-side channel. Also
    /// fans the same content out to our own other devices (Signal-style
    /// multi-device sync).
    ///
    /// Callers that need local storage (e.g. `send_server_dm`) do it
    /// themselves after this returns — this method only handles delivery.
    pub async fn send_sealed_dm_content(
        &self,
        peer_id: &UserId,
        content: crate::proto::content_v1::Content,
    ) -> Result<(), MessageError> {
        let plaintext = content.encode_to_vec();
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
        let mut delivered = self
            .deposit_sealed_to_all_devices(&mut auth_client, &mut message_client, &peer_id_str, &plaintext, None)
            .await;

        if delivered == 0 {
            tracing::warn!(
                peer_id = %peer_id,
                "Sealed DM send reached zero peer devices, refetching prekeys and retrying once"
            );
            delivered = self
                .deposit_sealed_to_all_devices(&mut auth_client, &mut message_client, &peer_id_str, &plaintext, None)
                .await;
        }

        if delivered == 0 {
            return Err(MessageError::DmError(format!(
                "Failed to deliver sealed DM to any device of {peer_id}"
            )));
        }

        // Self-sync: every one of our own devices except this one.
        let self_delivered = self
            .deposit_sealed_to_all_devices(
                &mut auth_client,
                &mut message_client,
                &our_user_id.to_string(),
                &plaintext,
                Some(our_device_id),
            )
            .await;
        tracing::debug!(
            peer_id = %peer_id,
            peer_devices_delivered = delivered,
            self_devices_delivered = self_delivered,
            "Sealed DM delivery complete"
        );

        Ok(())
    }

    /// Encrypt `plaintext` for every device of `recipient_id` and deposit
    /// each ciphertext via `SendSealedMessage`, UAK-authorized when the
    /// recipient's prekey bundle carries one. `skip_device_id` excludes a
    /// single device (used for self-sync to skip the sending device).
    ///
    /// Returns the number of devices successfully delivered to. Every
    /// per-device failure is logged — dropped devices are never silently
    /// capped or swallowed.
    async fn deposit_sealed_to_all_devices(
        &self,
        auth_client: &mut AuthClient,
        message_client: &mut MessageClient,
        recipient_id: &str,
        plaintext: &[u8],
        skip_device_id: Option<u32>,
    ) -> usize {
        let ciphertexts = match self
            .signal_service
            .encrypt_message_all_devices(auth_client, recipient_id, plaintext)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    recipient = recipient_id,
                    error = %e,
                    "Sealed DM: failed to encrypt for recipient's devices"
                );
                return 0;
            }
        };

        let uaks = fetch_recipient_uaks(auth_client, recipient_id).await;
        let timestamp = crate::util::time::TimestampNanos::now().as_u64();
        let server_key_id = self.signal_service.get_server_key_id();

        let mut delivered = 0;
        for (device_id, sealed_bytes) in ciphertexts {
            if skip_device_id == Some(device_id) {
                continue;
            }

            let sealed_message = SealedSenderMessage {
                sealed_body: sealed_bytes,
                server_key_id,
                timestamp,
            };

            let result = match uaks.get(&device_id) {
                Some(uak) if !uak.is_empty() => {
                    message_client
                        .send_sealed_message_anonymous(recipient_id, device_id, sealed_message, uak)
                        .await
                }
                _ => {
                    tracing::debug!(
                        recipient = recipient_id,
                        device_id,
                        "No UAK for device, sending without UAK header"
                    );
                    message_client
                        .send_sealed_message(recipient_id, device_id, sealed_message)
                        .await
                }
            };

            match result {
                Ok(_) => delivered += 1,
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

        delivered
    }
}

/// Fetch the UAK (Unidentified Access Key) for every device of `user_id`
/// from their prekey bundles. Devices without a UAK (or bundles that
/// fail to fetch) are simply absent from the map — callers fall back to
/// unauthenticated `SendSealedMessage` for those devices.
async fn fetch_recipient_uaks(auth_client: &mut AuthClient, user_id: &str) -> HashMap<u32, Vec<u8>> {
    use chat_v1::get_pre_key_bundle_response;

    let response = match auth_client.get_pre_key_bundles_all_devices(user_id).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(user_id, error = %e, "Failed to fetch prekey bundles for UAK lookup");
            return HashMap::new();
        }
    };

    let mut uaks = HashMap::new();
    match response.result {
        Some(get_pre_key_bundle_response::Result::Devices(device_list)) => {
            for bundle in device_list.bundles {
                if let Some(uak) = bundle.unidentified_access_key {
                    uaks.insert(bundle.device_id, uak);
                }
            }
        }
        Some(get_pre_key_bundle_response::Result::Bundle(bundle)) => {
            if let Some(uak) = bundle.unidentified_access_key {
                uaks.insert(bundle.device_id, uak);
            }
        }
        None => {}
    }
    uaks
}
