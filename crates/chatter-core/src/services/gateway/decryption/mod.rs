//! Decryption operations for the message gateway.

use libsignal_protocol::{
    CiphertextMessageType, DecryptionErrorMessage, PlaintextContent, Timestamp,
    extract_decryption_error_message_from_serialized_content,
};
use prost::Message as ProstMessage;
use zeroize::Zeroize;

use crate::crypto::dm_crypto::decrypt_dm_bytes;
use crate::domain::ids::UserId;
use crate::error::{CryptoError, ServiceError};
use crate::proto::content_v1::{Content, GroupEncrypted, content};

/// First byte of a serialized `PlaintextContent` (libsignal's
/// `PLAINTEXT_CONTEXT_IDENTIFIER_BYTE`). A decrypted sealed-sender "message"
/// starting with this byte is not ratchet plaintext but a peer's
/// DecryptionErrorMessage ride-along.
const PLAINTEXT_CONTENT_IDENTIFIER_BYTE: u8 = 0xC0;

/// Refusal for a sealed-sender message whose inner type is
/// `CiphertextMessageType::Plaintext` on a path that is not the
/// DecryptionErrorMessage handler.
///
/// Such bytes never entered `message_decrypt_*`, so nothing checked the
/// sender's identity key, nothing rejected a replay, and there is no forward
/// secrecy. Pinned libsignal refuses the type outright in
/// `sealed_sender_decrypt`; we only carve out the DEM ride-along.
fn unratcheted_plaintext_rejected(sender: &str) -> ServiceError {
    tracing::warn!(
        target: "chatter::crypto::decrypt_failure",
        kind = "sealed_sender",
        sender = %sender,
        "rejected unratcheted Plaintext sealed-sender message outside the DEM path"
    );
    ServiceError::Crypto(CryptoError::Protocol(
        "sealed sender message bypassed the ratchet (CiphertextMessageType::Plaintext) \
         and is not a DecryptionErrorMessage"
            .into(),
    ))
}

use super::block_filter::sender_is_blocked;
use super::inbound::content_type_name;
use super::{CallHangupReason, DecryptResult, DmContent, MessageGateway, SealedSenderResult};

mod dm;
mod group;
mod identified;
mod self_heal;

impl MessageGateway {
    /// Process an incoming sealed sender message.
    ///
    /// Decrypts the message and processes it based on content type:
    /// - DataMessage: stores to repository
    /// - KeyExchange: returns for bootstrap handling
    pub async fn process_incoming_message(
        &self,
        envelope: crate::proto::chat_v1::SealedSenderMessage,
    ) -> Result<SealedSenderResult, ServiceError> {
        // Decrypt the sealed sender envelope. An INNER (ratchet) failure with
        // an authenticated sender is the desynced-1:1-session wedge: turn it
        // into a DecryptionErrorMessage report instead of a dead error, so the
        // sender can archive its broken session and re-establish (Signal's
        // 1:1 self-heal).
        let (sender_address, mut plaintext, msg_type) = match self
            .signal_service
            .decrypt_message(&envelope.sealed_body, envelope.timestamp)
            .await
        {
            Ok(v) => v,
            Err(ServiceError::Crypto(CryptoError::SessionDesync {
                sender_uuid,
                sender_device_id,
                msg_type,
                ciphertext,
                reason,
                group_id,
            })) => {
                return self.build_decryption_error_report(
                    &sender_uuid,
                    sender_device_id,
                    msg_type,
                    &ciphertext,
                    &reason,
                    group_id,
                    envelope.timestamp,
                );
            }
            // Already decrypted once. Nothing to store, nothing to report, and
            // nothing lost — `MessageStored` is this file's existing "handled,
            // caller need do no more" answer.
            Err(ServiceError::Crypto(CryptoError::DuplicateMessage {
                sender_uuid,
                sender_device_id,
            })) => {
                tracing::debug!(
                    sender_uuid = %sender_uuid,
                    sender_device_id,
                    "GATEWAY: duplicate inner message dropped (re-delivery, not a desync)"
                );
                return Ok(SealedSenderResult::MessageStored);
            }
            Err(e) => return Err(e),
        };

        // A `Plaintext` USMC skipped the ratchet: no is_trusted_identity, no
        // replay protection, no forward secrecy. The ONLY thing we accept in
        // that shape is a peer's DecryptionErrorMessage about a message WE
        // sent — a `PlaintextContent` ride-along (0xC0-prefixed). Anything else
        // of that type is unratcheted attacker-chosen bytes; drop it rather
        // than let it reach `Content::decode`.
        if msg_type == CiphertextMessageType::Plaintext {
            let result = if plaintext.first() == Some(&PLAINTEXT_CONTENT_IDENTIFIER_BYTE) {
                self.handle_decryption_error_report(&sender_address, &plaintext)
                    .await
            } else {
                Err(unratcheted_plaintext_rejected(
                    &sender_address.name().to_string(),
                ))
            };
            plaintext.zeroize();
            return result;
        }

        // Use a scope guard to ensure plaintext is always zeroized
        let result = self
            .process_incoming_message_inner(&sender_address, &plaintext)
            .await;

        // Zeroize decrypted plaintext before returning (security: prevent heap residue)
        plaintext.zeroize();

        result
    }

    /// Anonymous mailbox receive: journal validated plaintext before routing.
    pub(crate) async fn process_incoming_reaction_durable(
        &self,
        envelope: crate::proto::chat_v1::SealedSenderMessage,
        key: &crate::storage::receive_artifact::ReceiveKey,
    ) -> Result<
        (
            Option<crate::storage::receive_artifact::ReceiveArtifact>,
            Option<SealedSenderResult>,
        ),
        ServiceError,
    > {
        let mut received = match self
            .signal_service
            .decrypt_reaction_durable(&envelope.sealed_body, key)
            .await
        {
            Ok(received) => received,
            // The staged receive rejected Plaintext before persistent mutation.
            // Only the unchanged gateway DEM gate may admit that envelope.
            Err(crate::services::signal::actor::DurableReceiveError::PlaintextRejected) => {
                return self
                    .process_incoming_message(envelope)
                    .await
                    .map(|outcome| (None, Some(outcome)));
            }
            Err(crate::services::signal::actor::DurableReceiveError::Failure(
                ServiceError::Crypto(CryptoError::DuplicateMessage { .. }),
            )) => {
                return Ok((None, Some(SealedSenderResult::MessageStored)));
            }
            Err(crate::services::signal::actor::DurableReceiveError::Failure(
                ServiceError::Crypto(CryptoError::SessionDesync {
                    sender_uuid,
                    sender_device_id,
                    msg_type,
                    ciphertext,
                    reason,
                    group_id,
                }),
            )) => {
                return self
                    .build_decryption_error_report(
                        &sender_uuid,
                        sender_device_id,
                        msg_type,
                        &ciphertext,
                        &reason,
                        group_id,
                        envelope.timestamp,
                    )
                    .map(|outcome| (None, Some(outcome)));
            }
            Err(crate::services::signal::actor::DurableReceiveError::Failure(error)) => {
                return Err(error);
            }
        };
        if let Some(artifact) = received.artifact {
            // The matching handoff consumes journaled reactions and data posts.
            // Running the normal router here would race its detached store task.
            received.content.zeroize();
            return Ok((Some(artifact), None));
        }
        let result = self
            .process_incoming_message_inner(&received.address, &received.content)
            .await;
        received.content.zeroize();
        result.map(|outcome| (None, Some(outcome)))
    }

    /// Inner implementation that processes decrypted plaintext.
    /// Separated to ensure plaintext zeroization happens in the outer function.
    async fn process_incoming_message_inner(
        &self,
        sender_address: &libsignal_protocol::ProtocolAddress,
        plaintext: &[u8],
    ) -> Result<SealedSenderResult, ServiceError> {
        // Extract sender ID from protocol address
        let sender_id: UserId = sender_address.name().parse().map_err(|e| {
            ServiceError::Validation(format!("Invalid sender ID in Signal address: {}", e))
        })?;

        // The sender is authenticated as of the line above and nothing has been
        // parsed yet, so this is the only place the block can be enforced
        // without either trusting an unauthenticated claim or letting the
        // message act first. One check here covers DataMessage, ControlContent
        // (reactions/typing/receipts/edits/pins), KeyExchange, SenderKeyRequest,
        // SKDM and mailbox announcements.
        if sender_is_blocked(&self.user_repo, &self.settings_repo, &sender_id)? {
            tracing::info!(
                sender_id = %sender_id,
                "GATEWAY: dropping sealed sender message from a blocked sender"
            );
            return Ok(SealedSenderResult::DroppedBlockedSender { sender_id });
        }

        tracing::info!(
            sender_id = %sender_id,
            plaintext_len = plaintext.len(),
            "GATEWAY: Decrypted sealed sender, parsing Content proto"
        );

        // First try to parse as Content proto (preferred, contains KeyExchange)
        if let Ok(content_proto) = crate::network::validation::protovalidate::decode_inbound_content(
            plaintext,
            "sealed_sender",
        ) {
            let ct_name = content_type_name(&content_proto.content);

            tracing::info!(
                sender_id = %sender_id,
                content_type = ct_name,
                timestamp = content_proto.timestamp,
                "GATEWAY: Parsed Content proto, content type = {}", ct_name
            );

            // Session-switch carrier after the sender reset its sessions with
            // us (Signal's NullMessage): decrypting it moved us onto the new
            // session, and that was its whole job. Nothing to store or show.
            if let Some(content::Content::NullMessage(_)) = content_proto.content {
                return Ok(SealedSenderResult::MessageStored);
            }

            // Check for KeyExchange
            if let Some(content::Content::KeyExchange(key_exchange)) = content_proto.content {
                tracing::info!(
                    sender_id = %sender_id,
                    conversation_id = %key_exchange.conversation_id,
                    key_len = key_exchange.ephemeral_pk.len(),
                    "GATEWAY: Found KeyExchange! key_len={} (0=REQUEST, 96=MESSAGE)",
                    key_exchange.ephemeral_pk.len()
                );
                return Ok(SealedSenderResult::KeyExchange {
                    sender_id,
                    sender_device_id: u32::from(sender_address.device_id()),
                    conversation_id: key_exchange.conversation_id,
                    ephemeral_public_key: key_exchange.ephemeral_pk,
                    timestamp: key_exchange.timestamp,
                });
            }

            // Check for SenderKeyRequest - someone wants our SKDM
            if let Some(content::Content::SenderKeyRequest(request)) = content_proto.content {
                let channel_id: crate::domain::ids::ChannelId =
                    request.channel_id.parse().map_err(|e| {
                        ServiceError::Validation(format!(
                            "Invalid channel_id in SenderKeyRequest: {}",
                            e
                        ))
                    })?;
                tracing::info!(
                    sender_id = %sender_id,
                    channel_id = %channel_id,
                    "GATEWAY: Found SenderKeyRequest via sealed sender"
                );
                return Ok(SealedSenderResult::SenderKeyRequested {
                    requester_id: sender_id,
                    channel_id,
                });
            }

            // Handle SenderKey (SKDM) - someone is distributing their sender key to us
            if let Some(content::Content::SenderKey(skdm)) = content_proto.content {
                let channel_id: crate::domain::ids::ChannelId =
                    skdm.channel_id.parse().map_err(|e| {
                        ServiceError::Validation(format!("Invalid channel_id in SenderKey: {}", e))
                    })?;
                tracing::info!(
                    sender_id = %sender_id,
                    channel_id = %channel_id,
                    skdm_len = skdm.distribution_message.len(),
                    "GATEWAY: Received SKDM via sealed sender, processing sender key"
                );
                // Process the SKDM so we can decrypt future messages from this sender.
                // Use the sender's REAL device id from the verified sealed-sender
                // address, not a hardcoded primary (1) — otherwise SKDMs from linked
                // devices install under the wrong address and group messages from
                // those devices fail to decrypt.
                let sender_device_id: u32 = sender_address.device_id().into();
                let Some(profile_key_changed) = self
                    .accept_skdm(&sender_id, sender_device_id, &channel_id, &skdm)
                    .await?
                else {
                    // Not stored: the sender is outside our verified membership.
                    return Ok(SealedSenderResult::MessageStored);
                };
                // Signal that the key is now installed so the caller can drain
                // any group messages that arrived before it (F14c). Carry the
                // exact identifiers the pending-decrypt buffer is keyed on:
                // (channel_id, sender_id, sender_device_id).
                return Ok(SealedSenderResult::SenderKeyProcessed {
                    sender_id,
                    sender_device_id,
                    channel_id,
                    distribution_message: skdm.distribution_message.clone(),
                    timestamp: content_proto.timestamp,
                    profile_key_changed,
                });
            }

            // Handle GroupMailboxAddress - member is announcing their ephemeral mailbox (NDSS '21)
            if let Some(content::Content::GroupMailboxAddress(announcement)) = content_proto.content
            {
                let channel_id: crate::domain::ids::ChannelId =
                    announcement.channel_id.parse().map_err(|e| {
                        ServiceError::Validation(format!(
                            "Invalid channel_id in GroupMailboxAddress: {}",
                            e
                        ))
                    })?;
                tracing::info!(
                    sender_id = %sender_id,
                    channel_id = %channel_id,
                    mailbox_pk_len = announcement.mailbox_pk.len(),
                    "GATEWAY: Received GroupMailboxAddress announcement via sealed sender"
                );
                // Return the announcement for processing by group coordinator
                return Ok(SealedSenderResult::MailboxAnnouncement {
                    sender_id,
                    channel_id,
                    announcement,
                });
            }

            // Handle SyncSent — a copy of a message WE sent, synced to our own
            // other devices (Signal's SyncMessage.Sent). File it as our OUTGOING
            // message in the conversation with the carried destination, NOT as an
            // inbound message from ourselves. This is the receive-side F8 fix.
            if let Some(content::Content::SyncSent(sync)) = content_proto.content {
                tracing::info!(
                    sender_id = %sender_id,
                    destination = ?sync.destination,
                    "GATEWAY: Received SyncSent self-sync, filing as outgoing to destination"
                );
                return self.store_synced_sent_message(sync, sender_id).await;
            }

            // Handle DataMessage inside Content
            let content_ts = content_proto.timestamp;
            if let Some(content::Content::DataMessage(data_message)) = content_proto.content {
                // Delete-for-everyone rides an empty-body `DataMessage` carrying
                // the `delete` field on this same sealed ServerDm path
                // (builders::build_targeted_delete_content). It
                // is CONTROL, not stored content: `store_data_message` ignores
                // `delete`, so routing it there would persist a junk empty-body
                // row and leave the TARGET message intact — the recipient would
                // never actually delete (the whole point of "delete for
                // everyone"). Hand it to the control dispatcher instead, which
                // already turns it into DmDeleteForEveryone / ChannelDeleteForEveryone
                // (inbox/polling/device.rs::dispatch_control_content) and reuses
                // the existing delete path (`MessageService::apply_delete`).
                if data_message.delete.is_some() {
                    return Ok(SealedSenderResult::ControlContent {
                        sender_id,
                        sender_device_id: u32::from(sender_address.device_id()),
                        content: Box::new(Content {
                            timestamp: content_ts,
                            content: Some(content::Content::DataMessage(data_message)),
                            ..Default::default()
                        }),
                    });
                }
                return self.store_data_message(data_message, Some(sender_id)).await;
            }

            // Handle AnonGroupMessage - extract and return for UI notification
            if let Some(content::Content::AnonGroupMessage(msg)) = content_proto.content {
                tracing::info!(
                    sender_id = %sender_id,
                    group_id_len = msg.group_id.len(),
                    has_inner = msg.inner_content.is_some(),
                    "GATEWAY: Received AnonGroupMessage, returning for UI notification"
                );
                let inner_content = msg.inner_content.ok_or_else(|| {
                    ServiceError::Validation("AnonGroupMessage missing inner_content".into())
                })?;
                return Ok(SealedSenderResult::AnonGroupMessageReceived {
                    sender_id,
                    group_id: msg.group_id,
                    inner_content,
                });
            }

            // Handle AnonGroupJoinAck - joiner sending their mailbox key and profile
            if let Some(content::Content::AnonGroupJoinAck(ack)) = content_proto.content {
                tracing::info!(
                    sender_id = %sender_id,
                    group_id_len = ack.group_id.len(),
                    joiner_mailbox_pk_len = ack.joiner_mailbox_pk.len(),
                    has_profile_key = ack.profile_key.is_some(),
                    has_display_name = ack.encrypted_display_name.is_some(),
                    "GATEWAY: Received AnonGroupJoinAck, returning for member mailbox update"
                );
                return Ok(SealedSenderResult::AnonGroupJoinAckReceived {
                    sender_id,
                    sender_device_id: u32::from(sender_address.device_id()),
                    group_id: ack.group_id,
                    joiner_mailbox_pk: ack.joiner_mailbox_pk,
                    timestamp: ack.timestamp,
                    profile_key: ack.profile_key,
                    encrypted_display_name: ack.encrypted_display_name,
                    avatar: ack.avatar,
                });
            }

            // Handle group management messages (MemberList, Leave, Kick, ProfileShare, Update)
            // These arrive as top-level Content types on the group mailbox, NOT wrapped
            // in AnonGroupMessage envelope. Route them through AnonGroupMessageReceived
            // so InboxEventProcessor can handle them.
            let mgmt_group_id = match &content_proto.content {
                Some(content::Content::AnonGroupMemberList(m)) => Some(m.group_id.clone()),
                Some(content::Content::AnonGroupDecline(m)) => Some(m.group_id.clone()),
                Some(content::Content::AnonGroupLeave(m)) => Some(m.group_id.clone()),
                Some(content::Content::AnonGroupKick(m)) => Some(m.group_id.clone()),
                Some(content::Content::AnonGroupProfileShare(m)) => Some(m.group_id.clone()),
                Some(content::Content::AnonGroupUpdate(m)) => Some(m.group_id.clone()),
                _ => None,
            };

            if let Some(group_id) = mgmt_group_id {
                tracing::info!(
                    sender_id = %sender_id,
                    group_id_hex = %hex::encode(&group_id),
                    content_type = ct_name,
                    "GATEWAY: Received group management message, routing as AnonGroupMessage"
                );
                return Ok(SealedSenderResult::AnonGroupMessageReceived {
                    sender_id,
                    group_id,
                    inner_content: Box::new(content_proto),
                });
            }

            // Secret-chat bootstraps are sealed to ONE device inbox; the device
            // they came from is the one that chat is bound to.
            if matches!(
                content_proto.content,
                Some(content::Content::AnonDmRequest(_))
                    | Some(content::Content::AnonGroupInvite(_))
            ) {
                return Ok(SealedSenderResult::SecretChatBootstrap {
                    sender_id,
                    sender_device_id: u32::from(sender_address.device_id()),
                    content: Box::new(content_proto),
                });
            }

            // Control content (reaction, pin, edit, typing, receipt, delete,
            // and normal-mode call signaling) delivered as per-device sealed
            // sender. Caller dispatches the inner variant.
            let is_control = matches!(
                content_proto.content,
                Some(content::Content::Reaction(_))
                    | Some(content::Content::Pin(_))
                    | Some(content::Content::Edit(_))
                    | Some(content::Content::Typing(_))
                    | Some(content::Content::Receipt(_))
                    | Some(content::Content::DeleteForMeSync(_))
                    | Some(content::Content::ReadSync(_))
                    | Some(content::Content::ProfileKeySync(_))
                    | Some(content::Content::ConfigurationSync(_))
                    | Some(content::Content::SavedMessageSync(_))
                    | Some(content::Content::CallKey(_))
                    | Some(content::Content::CallHangup(_))
                    | Some(content::Content::GroupCallSignal(_))
                    | Some(content::Content::CallEvent(_))
            );
            if is_control {
                tracing::info!(
                    sender_id = %sender_id,
                    content_type = ct_name,
                    "GATEWAY: Channel control content received via sealed sender"
                );
                return Ok(SealedSenderResult::ControlContent {
                    sender_id,
                    sender_device_id: u32::from(sender_address.device_id()),
                    content: Box::new(content_proto),
                });
            }

            // Other content types in sealed sender — nothing actionable.
            tracing::info!(
                sender_id = %sender_id,
                content_type = ct_name,
                "GATEWAY: Non-data/non-keyexchange content - returning MessageStored"
            );
            return Ok(SealedSenderResult::MessageStored);
        }

        // Every sender wraps its plaintext in `Content`; anything else is
        // refused rather than guessed at.
        Err(not_content(plaintext.len()))
    }
}

/// A decrypted plaintext that is not a `Content` proto. Every sender wraps
/// its plaintext in `Content` (the bare-`DataMessage` and raw-UTF-8 fallbacks
/// that used to accept anything here were removed), so this is refused.
fn not_content(len: usize) -> ServiceError {
    ServiceError::Validation(format!(
        "decrypted plaintext ({len} bytes) is not a Content proto"
    ))
}
