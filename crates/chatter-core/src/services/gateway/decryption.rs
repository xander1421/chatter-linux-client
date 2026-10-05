//! Decryption operations for the message gateway.

use libsignal_protocol::{
    extract_decryption_error_message_from_serialized_content, CiphertextMessageType,
    DecryptionErrorMessage, PlaintextContent, Timestamp,
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
pub(crate) fn unratcheted_plaintext_rejected(sender: &str) -> ServiceError {
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
use super::{CallHangupReason, DecryptResult, DmContent, MessageGateway, SealedSenderResult};
use super::inbound::content_type_name;

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
            })) => {
                return self.build_decryption_error_report(
                    &sender_uuid,
                    sender_device_id,
                    msg_type,
                    &ciphertext,
                    &reason,
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
                Err(unratcheted_plaintext_rejected(&sender_address.name().to_string()))
            };
            plaintext.zeroize();
            return result;
        }

        // Use a scope guard to ensure plaintext is always zeroized
        let result = self.process_incoming_message_inner(&sender_address, &plaintext).await;

        // Zeroize decrypted plaintext before returning (security: prevent heap residue)
        plaintext.zeroize();

        result
    }

    /// RECEIVER side of the 1:1 self-heal: we could not decrypt an inner
    /// Whisper/PreKey message from an authenticated sender. Build a
    /// `DecryptionErrorMessage` naming the failed ciphertext so the caller can
    /// ship it back to the sender (sealed, session-less `PlaintextContent`).
    fn build_decryption_error_report(
        &self,
        sender_uuid: &str,
        sender_device_id: u32,
        msg_type: u8,
        failed_ciphertext: &[u8],
        reason: &str,
        envelope_timestamp: u64,
    ) -> Result<SealedSenderResult, ServiceError> {
        let sender_id: UserId = sender_uuid.parse().map_err(|e| {
            ServiceError::Validation(format!("Invalid sender ID in desync report: {}", e))
        })?;

        // libsignal discriminants: 2 = Whisper, 3 = PreKey. Only those two can
        // fail an inner decrypt (Plaintext has no ratchet; SenderKey never
        // reaches the 1:1 sealed path).
        let msg_type = match msg_type {
            t if t == CiphertextMessageType::Whisper as u8 => CiphertextMessageType::Whisper,
            t if t == CiphertextMessageType::PreKey as u8 => CiphertextMessageType::PreKey,
            other => {
                return Err(ServiceError::Validation(format!(
                    "unexpected inner message type {} in desync report",
                    other
                )));
            }
        };

        // Envelope timestamps are TimestampNanos on the deposit path; the DEM
        // wire field is epoch millis. The timestamp only helps the sender
        // locate the failed message — the ratchet key + device id do the
        // actual session matching.
        let ts_millis = envelope_timestamp / 1_000_000;
        let dem = DecryptionErrorMessage::for_original(
            failed_ciphertext,
            msg_type,
            Timestamp::from_epoch_millis(ts_millis),
            sender_device_id,
        )
        .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?;

        tracing::warn!(
            sender_id = %sender_id,
            sender_device_id,
            reason = %reason,
            "GATEWAY: 1:1 inner decrypt failed — reporting DecryptionErrorMessage to sender for session self-heal"
        );

        Ok(SealedSenderResult::DecryptionErrorNeedsReport {
            sender_id,
            sender_device_id,
            dem_serialized: dem.serialized().to_vec(),
        })
    }

    /// SENDER side of the 1:1 self-heal: a peer reported (via
    /// `DecryptionErrorMessage`) that it could not decrypt a message WE sent.
    /// If the report names this device and its ratchet key still matches our
    /// current session with that peer device, archive the session so the next
    /// send re-establishes via a fresh PreKeySignalMessage.
    async fn handle_decryption_error_report(
        &self,
        sender_address: &libsignal_protocol::ProtocolAddress,
        plaintext: &[u8],
    ) -> Result<SealedSenderResult, ServiceError> {
        let peer_id: UserId = sender_address.name().parse().map_err(|e| {
            ServiceError::Validation(format!("Invalid sender ID in Signal address: {}", e))
        })?;
        let peer_device_id: u32 = sender_address.device_id().into();

        // The report skipped `process_incoming_message_inner`, and with it
        // the block check every other sealed message gets. A blocked peer
        // must not reset our session with it or have a message sent again.
        if sender_is_blocked(&self.user_repo, &self.settings_repo, &peer_id)? {
            tracing::info!(
                sender_id = %peer_id,
                "GATEWAY: dropping a DecryptionErrorMessage from a blocked sender"
            );
            return Ok(SealedSenderResult::DroppedBlockedSender { sender_id: peer_id });
        }

        let plaintext_content = PlaintextContent::try_from(plaintext)
            .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?;
        let dem =
            extract_decryption_error_message_from_serialized_content(plaintext_content.body())
                .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?;

        // The DEM names the device that SENT the failed message. Only that
        // device holds the broken sending session — a copy landing on any
        // other of our devices is a no-op.
        let our_device_id = self
            .settings_repo
            .get::<u32>(crate::storage::repositories::settings_repo::keys::CURRENT_DEVICE_ID)?
            .ok_or(ServiceError::NotLoggedIn)?;
        if dem.device_id() != our_device_id {
            tracing::info!(
                peer_id = %peer_id,
                dem_device_id = dem.device_id(),
                our_device_id,
                "GATEWAY: DecryptionErrorMessage targets another of our devices — ignoring"
            );
            return Ok(SealedSenderResult::MessageStored);
        }

        // No ratchet key = the failed message was sender-key (group) content.
        // Our 1:1 session with the reporter is not what broke, so it must not
        // be archived (Signal-Desktop `archiveSessionOnMatch` refuses without a
        // ratchet key; `archive_session_for_resend(None)` would archive
        // unconditionally). What the reporter needs is the content itself,
        // resent pairwise (Signal-Desktop `onRetryRequest`).
        let Some(dem_ratchet_key) = dem.ratchet_key().map(|k| k.serialize().to_vec()) else {
            return Ok(SealedSenderResult::ResendRequested {
                peer_id,
                peer_device_id,
                timestamp_ms: dem.timestamp().epoch_millis(),
            });
        };
        let archived = self
            .signal_service
            .actor
            .archive_session_for_resend(peer_id.to_string(), peer_device_id, Some(dem_ratchet_key))
            .await?;

        // Split by outcome, because the two branches mean opposite things and only
        // one is worth waking anyone for.
        //
        // Archiving is consequential: a session was torn down and the next send has
        // to re-establish. Declining is the stale-report guard doing exactly its
        // job — Signal requires it, since a report naming a ratchet key we have
        // moved past must not archive a session that is currently working. That
        // branch is also the common one: a backlog of 201 stale reports drained in
        // a single inbox poll and logged 201 warnings for correct behaviour, which
        // is how a log stops being read.
        if archived {
            tracing::warn!(
                peer_id = %peer_id,
                peer_device_id,
                "GATEWAY: peer reported a decryption error on our message — session \
                 archived; next send re-establishes via PreKeySignalMessage"
            );
        } else {
            tracing::debug!(
                peer_id = %peer_id,
                peer_device_id,
                "GATEWAY: decryption-error report left the session untouched \
                 (no session, or it names a ratchet key we have moved past)"
            );
        }

        Ok(SealedSenderResult::SessionHealed {
            peer_id,
            peer_device_id,
            archived,
            timestamp_ms: dem.timestamp().epoch_millis(),
        })
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
        if let Ok(content_proto) = Content::decode(plaintext) {
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
                let channel_id: crate::domain::ids::ChannelId = request.channel_id.parse().map_err(|e| {
                    ServiceError::Validation(format!("Invalid channel_id in SenderKeyRequest: {}", e))
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
                let channel_id: crate::domain::ids::ChannelId = skdm.channel_id.parse().map_err(|e| {
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
                let Some(profile_key_changed) =
                    self.accept_skdm(&sender_id, sender_device_id, &channel_id, &skdm).await?
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
            if let Some(content::Content::GroupMailboxAddress(announcement)) = content_proto.content {
                let channel_id: crate::domain::ids::ChannelId = announcement.channel_id.parse().map_err(|e| {
                    ServiceError::Validation(format!("Invalid channel_id in GroupMailboxAddress: {}", e))
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
                    destination = %sync.destination_user_id,
                    "GATEWAY: Received SyncSent self-sync, filing as outgoing to destination"
                );
                return self.store_synced_sent_message(sync, sender_id).await;
            }

            // Handle DataMessage inside Content
            let content_ts = content_proto.timestamp;
            if let Some(content::Content::DataMessage(data_message)) = content_proto.content {
                // Delete-for-everyone rides an empty-body `DataMessage` carrying
                // the `delete` field on this same sealed ServerDm path
                // (builders::build_delete_for_everyone_content_with_channel). It
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
                    group_id: ack.group_id,
                    joiner_mailbox_pk: ack.joiner_mailbox_pk,
                    timestamp: ack.timestamp,
                    profile_key: ack.profile_key,
                    encrypted_display_name: ack.encrypted_display_name,
                    encrypted_avatar: ack.encrypted_avatar,
                });
            }

            // Handle group management messages (MemberList, Leave, Kick, ProfileShare, Update)
            // These arrive as top-level Content types on the group mailbox, NOT wrapped
            // in AnonGroupMessage envelope. Route them through AnonGroupMessageReceived
            // so InboxEventProcessor can handle them.
            let mgmt_group_id = match &content_proto.content {
                Some(content::Content::AnonGroupMemberList(m)) => Some(m.group_id.clone()),
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

    /// Persist a channel member's profile key carried on their SKDM. Returns
    /// whether it was new or rotated — the caller's cue to refetch their
    /// profile. Our own (from our other devices) is not a peer key.
    pub(crate) fn store_skdm_profile_key(&self, sender_id: &UserId, key: Option<&[u8]>) -> bool {
        let Some(key) = key.and_then(|k| <[u8; 32]>::try_from(k).ok()) else {
            return false;
        };
        use crate::services::current_user::CurrentUserProvider;
        let Ok(us) = self.settings_repo.current_user_id() else {
            return false;
        };
        if us == *sender_id {
            return false;
        }
        self.user_repo
            .set_friend_profile_key(&us, sender_id, &key)
            .inspect_err(|e| tracing::debug!(sender_id = %sender_id, error = %e, "SKDM: failed to store sender profile key"))
            .unwrap_or(false)
    }

    /// Decrypt an identified sender message (PreKey or Ciphertext).
    ///
    /// This is the fallback path when sealed sender is not used.
    /// The server knows the sender and passes source_user_id/source_device_id.
    pub async fn decrypt_identified_message(
        &self,
        envelope: &[u8],
        sender_id: &UserId,
        sender_device_id: u32,
    ) -> Result<SealedSenderResult, ServiceError> {
        // The identified path never STARTS a session. Its sender is the
        // server's claim alone, and a first identity key is trusted on first
        // use (`crypto/signal/store.rs` is_trusted_identity), so a PreKey
        // message here could pin a server-chosen key for a peer we never
        // talked to. Sessions are established over sealed sender, whose
        // certificate names the sender.
        let session_key = format!("{}:{}", sender_id, sender_device_id);
        if self.signal_service.key_store.load_session(&session_key).map_err(ServiceError::Storage)?.is_none() {
            tracing::warn!(%sender_id, sender_device_id,
                "Identified message with no session for the sender; refusing to start one");
            return Err(ServiceError::Crypto(CryptoError::Protocol(
                "No session available for an identified sender".into(),
            )));
        }
        // libsignal's SessionRecord keeps the archived states and, for a
        // Whisper message the current state cannot open, tries them and
        // promotes the one that decrypts (libsignal `session_management.rs`
        // `try_decrypt_from_record`).
        let mut plaintext = self
            .signal_service
            .decrypt_message_identified(envelope, &sender_id.to_string(), sender_device_id)
            .await?;

        // Process plaintext and ensure zeroization
        let result = self
            .decrypt_identified_message_inner(sender_id, sender_device_id, &plaintext)
            .await;

        // Zeroize decrypted plaintext before returning (security: prevent heap residue)
        plaintext.zeroize();

        result
    }

    /// Inner implementation for identified message decryption.
    /// Separated to ensure plaintext zeroization in the outer function.
    async fn decrypt_identified_message_inner(
        &self,
        sender_id: &UserId,
        sender_device_id: u32,
        plaintext: &[u8],
    ) -> Result<SealedSenderResult, ServiceError> {
        // Same refusal as the sealed path, before anything is parsed. The
        // sender here is not asserted by the server: `plaintext` came out of
        // the Double Ratchet session filed under this address, so only someone
        // holding that session could have produced it.
        if sender_is_blocked(&self.user_repo, &self.settings_repo, sender_id)? {
            tracing::info!(
                sender_id = %sender_id,
                "GATEWAY: dropping identified-sender message from a blocked sender"
            );
            return Ok(SealedSenderResult::DroppedBlockedSender { sender_id: *sender_id });
        }

        // First try to parse as Content proto (preferred, contains KeyExchange)
        if let Ok(content_proto) = Content::decode(plaintext) {
            // Check for KeyExchange
            if let Some(content::Content::KeyExchange(key_exchange)) = content_proto.content {
                tracing::debug!(
                    sender_id = %sender_id,
                    conversation_id = %key_exchange.conversation_id,
                    key_len = key_exchange.ephemeral_pk.len(),
                    "Received KeyExchange in identified sender message"
                );
                return Ok(SealedSenderResult::KeyExchange {
                    sender_id: *sender_id,
                    sender_device_id,
                    conversation_id: key_exchange.conversation_id,
                    ephemeral_public_key: key_exchange.ephemeral_pk,
                    timestamp: key_exchange.timestamp,
                });
            }

            // Handle DataMessage inside Content
            if let Some(content::Content::DataMessage(data_message)) = content_proto.content {
                return self.store_data_message(data_message, Some(*sender_id)).await;
            }

            // Other content types
            tracing::debug!(
                sender_id = %sender_id,
                "Received non-data content in identified sender message"
            );
            return Ok(SealedSenderResult::MessageStored);
        }

        Err(not_content(plaintext.len()))
    }

    /// Process an incoming DM message with layered decryption.
    ///
    /// DM messages have two encryption layers:
    /// 1. Mailbox layer (XChaCha20-Poly1305) - provides sender anonymity from server
    /// 2. Signal layer (sealed sender) - provides forward secrecy via Double Ratchet
    ///
    /// This method decrypts both layers, parses the Content proto,
    /// and returns the parsed content.
    ///
    /// # Arguments
    ///
    /// * `mailbox_encrypted` - The doubly-encrypted envelope from the mailbox
    /// * `shared_secret` - The X25519 shared secret for mailbox layer decryption
    /// * `server_timestamp_ns` - The mailbox message's server timestamp (sender
    ///   certificate validation time)
    ///
    /// # Returns
    ///
    /// A tuple of (sender_user_id, content) on success.
    pub async fn process_dm_message(
        &self,
        mailbox_encrypted: &[u8],
        shared_secret: &[u8; 32],
        server_timestamp_ns: u64,
    ) -> Result<(UserId, DmContent), ServiceError> {
        // Layer 1: Decrypt mailbox layer (XChaCha20-Poly1305)
        let signal_ciphertext = decrypt_dm_bytes(mailbox_encrypted, shared_secret)?;

        // Layer 2: Decrypt Signal layer (sealed sender with Double Ratchet)
        let (sender_address, mut plaintext, msg_type) = self
            .signal_service
            .decrypt_message(&signal_ciphertext, server_timestamp_ns)
            .await?;

        // No DEM ride-along is expected on the DM path, so `Plaintext` here is
        // simply ratchet-bypassing bytes. Refuse before `Content::decode`.
        if msg_type == CiphertextMessageType::Plaintext {
            plaintext.zeroize();
            return Err(unratcheted_plaintext_rejected(sender_address.name()));
        }

        // Extract sender ID from protocol address
        let sender_id: UserId = sender_address.name().parse().map_err(|e| {
            ServiceError::Validation(format!("Invalid sender ID in Signal address: {}", e))
        })?;

        // Block check before any parse, as on the other two paths. The mailbox
        // this envelope arrived in belongs to one peer, but the sealed sender
        // certificate names who actually sent it — so the authenticated
        // `sender_id` is what gets checked, not the mailbox owner.
        if sender_is_blocked(&self.user_repo, &self.settings_repo, &sender_id)? {
            plaintext.zeroize();
            tracing::info!(
                sender_id = %sender_id,
                "DM_DECRYPT: dropping mailbox message from a blocked sender"
            );
            return Ok((sender_id, DmContent::DroppedBlockedSender));
        }

        // Try to parse as Content proto first (preferred)
        if let Ok(content_proto) = Content::decode(plaintext.as_slice()) {
            // Log content type before parsing
            let ct_name = content_type_name(&content_proto.content);
            tracing::info!(
                sender_id = %sender_id,
                content_type = ct_name,
                timestamp = content_proto.timestamp,
                "DM_DECRYPT: Parsed Content proto from peer"
            );

            let mut dm_content = Self::parse_content(content_proto)?;
            if let DmContent::KeyExchange { sender_device_id, .. } = &mut dm_content {
                *sender_device_id = u32::from(sender_address.device_id());
            }
            tracing::info!(sender_id = %sender_id, dm_content_type = ?std::mem::discriminant(&dm_content), "DM_DECRYPT: Content parsed to DmContent");
            // Zeroize plaintext before returning
            plaintext.zeroize();
            return Ok((sender_id, dm_content));
        }

        let len = plaintext.len();
        plaintext.zeroize();
        Err(not_content(len))
    }

    /// Parse Content proto into DmContent.
    pub(crate) fn parse_content(content_proto: Content) -> Result<DmContent, ServiceError> {
        match content_proto.content {
            Some(content::Content::DataMessage(msg)) => {
                // Check if this is a delete request
                if let Some(delete) = msg.delete {
                    return Ok(DmContent::DeleteForEveryone {
                        target_sent_timestamp: delete.target_sent_timestamp,
                    });
                }

                let expire_timer = Some(crate::services::disappearing::message_ttl(&msg));
                let timer_version = msg.expire_timer_version;
                let timer_update = crate::services::disappearing::is_timer_update(&msg);
                // Extract profile key if present (32 bytes for display_name decryption)
                let profile_key = msg.profile_key.filter(|k| k.len() == 32);
                if profile_key.is_some() {
                    tracing::debug!("parse_content: DataMessage contains profile key");
                }

                // Log timestamp for debugging
                tracing::debug!(
                    dm_timestamp = msg.timestamp,
                    content_timestamp = content_proto.timestamp,
                    body_len = msg.body.len(),
                    has_profile_key = profile_key.is_some(),
                    "parse_content: DataMessage extracted"
                );
                // Otherwise it's a text message (possibly with attachments)
                let thread_id = (!msg.thread_id.is_empty()).then_some(msg.thread_id);
                Ok(DmContent::Text {
                    body: msg.body,
                    timestamp: msg.timestamp,
                    attachments: msg.attachments,
                    profile_key,
                    quote: msg.quote,
                    thread_id,
                    expire_timer,
                    timer_version,
                    timer_update,
                })
            }
            Some(content::Content::Typing(msg)) => {
                tracing::info!(
                    action = msg.action,
                    action_name = if msg.action == 1 { "STARTED" } else { "STOPPED" },
                    timestamp = msg.timestamp,
                    "DM_DECRYPT: Parsed Typing content from peer"
                );
                Ok(DmContent::Typing {
                    action: msg.action,
                    timestamp: msg.timestamp,
                })
            }
            Some(content::Content::Receipt(msg)) => {
                Ok(DmContent::Receipt {
                    receipt_type: msg.r#type,
                    timestamps: msg.timestamps,
                })
            }
            Some(content::Content::Reaction(msg)) => {
                Ok(DmContent::Reaction {
                    emoji: msg.emoji,
                    remove: msg.remove,
                    target_author_aci: msg.target_author_aci,
                    target_sent_timestamp: msg.target_sent_timestamp,
                    timestamp: msg.timestamp,
                })
            }
            Some(content::Content::Pin(msg)) => {
                Ok(DmContent::Pin {
                    target_author_aci: msg.target_author_aci,
                    target_sent_timestamp: msg.target_sent_timestamp,
                    unpin: msg.unpin,
                    timestamp: msg.timestamp,
                })
            }
            Some(content::Content::Edit(msg)) => {
                let (new_body, edit_timestamp) = msg.data_message
                    .map(|dm| (dm.body, dm.timestamp))
                    .unwrap_or_default();
                Ok(DmContent::Edit {
                    target_sent_timestamp: msg.target_sent_timestamp,
                    new_body,
                    edit_timestamp,
                })
            }
            Some(content::Content::DeleteForMeSync(_)) => {
                // Own-device only (`sync_to_own_devices`, device inbox): on
                // the mailbox it can only come from a peer.
                Err(ServiceError::Validation("DeleteForMeSync received on DM mailbox path".into()))
            }
            Some(content::Content::SavedMessageSync(_)) => {
                // Own-device only (saved.rs sends it to our device inbox): on
                // the mailbox it can only come from a peer.
                Err(ServiceError::Validation("SavedMessageSync received on DM mailbox path".into()))
            }
            Some(content::Content::KeyExchange(kx)) => {
                // Key exchange received in DM mailbox (bootstrap response from peer)
                Ok(DmContent::KeyExchange {
                    // Only the decrypting caller knows it; `process_dm_message` sets it.
                    sender_device_id: 0,
                    conversation_id: kx.conversation_id,
                    ephemeral_pk: kx.ephemeral_pk,
                    timestamp: kx.timestamp,
                })
            }
            Some(content::Content::CallKey(call_key)) => {
                // Call key exchange with ephemeral keys for PFS
                // key_material contains ephemeral X25519 public key (32 bytes)
                let ephemeral_pk: [u8; 32] = call_key.key_material
                    .as_slice()
                    .try_into()
                    .map_err(|_| ServiceError::Validation(format!(
                        "Invalid ephemeral key length: {} (expected 32)",
                        call_key.key_material.len()
                    )))?;

                if call_key.is_offer {
                    // Offer from caller - they're initiating a call
                    Ok(DmContent::CallKeyOffer {
                        call_id: call_key.call_id,
                        ephemeral_public_key: ephemeral_pk,
                        timestamp: call_key.timestamp,
                        call_type: call_key.call_type,
                        channel_id: call_key.channel_id,
                    })
                } else {
                    // Answer from callee - response to our offer
                    Ok(DmContent::CallKeyAnswer {
                        call_id: call_key.call_id,
                        ephemeral_public_key: ephemeral_pk,
                        timestamp: call_key.timestamp,
                    })
                }
            }
            Some(content::Content::GroupCallSignal(msg)) => {
                use crate::domain::call::GroupCallSignal;
                use crate::proto::content_v1::group_call_signal::Signal;
                let signal = match msg.signal {
                    Some(Signal::Ring(_)) => GroupCallSignal::Ring,
                    Some(Signal::KeyRequest(_)) => GroupCallSignal::KeyRequest,
                    Some(Signal::Key(update)) => {
                        let key: [u8; 32] = update.key_material.as_slice().try_into().map_err(|_| {
                            ServiceError::Validation(format!(
                                "Invalid group call key length: {} (expected 32)",
                                update.key_material.len()
                            ))
                        })?;
                        // Epoch 0 is a participant's own starting key, never sent.
                        if update.key_epoch == 0 {
                            return Err(ServiceError::Validation("group call key at epoch 0".into()));
                        }
                        GroupCallSignal::Key { key, epoch: update.key_epoch }
                    }
                    None => return Err(ServiceError::Validation("empty group call signal".into())),
                };
                Ok(DmContent::GroupCallSignal {
                    call_id: msg.call_id,
                    channel_id: msg.channel_id,
                    signal,
                    timestamp: msg.timestamp,
                })
            }
            Some(content::Content::CallHangup(hangup)) => {
                use crate::proto::content_v1::call_hangup;
                // Call hangup notification received
                let reason = match call_hangup::Reason::try_from(hangup.reason) {
                    Ok(call_hangup::Reason::Normal) => CallHangupReason::Normal,
                    Ok(call_hangup::Reason::Declined) => CallHangupReason::Declined,
                    Ok(call_hangup::Reason::Busy) => CallHangupReason::Busy,
                    Ok(call_hangup::Reason::Timeout) => CallHangupReason::Timeout,
                    Ok(call_hangup::Reason::NetworkError) => CallHangupReason::NetworkError,
                    Ok(call_hangup::Reason::AcceptedElsewhere) => CallHangupReason::AcceptedElsewhere,
                    Ok(call_hangup::Reason::DeclinedElsewhere) => CallHangupReason::DeclinedElsewhere,
                    _ => CallHangupReason::Normal,
                };
                Ok(DmContent::CallHangup {
                    call_id: hangup.call_id,
                    reason,
                    timestamp: hangup.timestamp,
                })
            }
            Some(content::Content::SenderKey(_)) |
            Some(content::Content::GroupEncrypted(_)) |
            Some(content::Content::SenderKeyRequest(_)) => {
                // Group messages shouldn't come through DM path
                Err(ServiceError::Validation("Group message received on DM path".into()))
            }
            // Anonymous group messages (NDSS'21 style - sent via pairwise DM)
            Some(content::Content::AnonGroupInvite(invite)) => {
                Ok(DmContent::AnonGroupInvite {
                    group_id: invite.group_id,
                    group_name: invite.group_name,
                    inviter_mailbox_pk: invite.inviter_mailbox_pk,
                    timestamp: invite.timestamp,
                    profile_key: invite.profile_key,
                    encrypted_display_name: invite.encrypted_display_name,
                    encrypted_avatar: invite.encrypted_avatar,
                })
            }
            Some(content::Content::AnonGroupJoinAck(ack)) => {
                Ok(DmContent::AnonGroupJoinAck {
                    group_id: ack.group_id,
                    joiner_mailbox_pk: ack.joiner_mailbox_pk,
                    timestamp: ack.timestamp,
                    profile_key: ack.profile_key,
                    encrypted_display_name: ack.encrypted_display_name,
                    encrypted_avatar: ack.encrypted_avatar,
                })
            }
            Some(content::Content::AnonGroupMemberList(list)) => {
                Ok(DmContent::AnonGroupMemberList {
                    group_id: list.group_id,
                    members: list.members,
                    timestamp: list.timestamp,
                })
            }
            Some(content::Content::AnonGroupLeave(leave)) => {
                Ok(DmContent::AnonGroupLeave {
                    group_id: leave.group_id,
                    timestamp: leave.timestamp,
                })
            }
            Some(content::Content::AnonGroupKick(kick)) => {
                Ok(DmContent::AnonGroupKick {
                    group_id: kick.group_id,
                    kicked_user_id: kick.kicked_user_id,
                    admin_signature: kick.admin_signature,
                    timestamp: kick.timestamp,
                })
            }
            Some(content::Content::AnonGroupUpdate(update)) => {
                let timestamp = update.timestamp;
                Ok(DmContent::AnonGroupUpdate {
                    group_id: update.group_id.clone(),
                    update,
                    timestamp,
                })
            }
            Some(content::Content::AnonGroupMessage(msg)) => {
                Ok(DmContent::AnonGroupMessage {
                    group_id: msg.group_id,
                    inner_content: msg.inner_content.ok_or_else(|| {
                        ServiceError::Validation("AnonGroupMessage missing inner_content".into())
                    })?,
                })
            }
            // Anonymous DM content types
            Some(content::Content::AnonDmRequest(request)) => {
                Ok(DmContent::AnonDmRequest {
                    conversation_id: request.conversation_id,
                    initiator_mailbox_pk: request.initiator_mailbox_pk,
                    intro_message: if request.intro_message.is_empty() {
                        None
                    } else {
                        Some(request.intro_message)
                    },
                    profile_key: request.profile_key,
                    encrypted_display_name: request.encrypted_display_name,
                    encrypted_avatar: request.encrypted_avatar,
                    timestamp: request.timestamp,
                })
            }
            Some(content::Content::AnonDmAccept(accept)) => {
                Ok(DmContent::AnonDmAccept {
                    conversation_id: accept.conversation_id,
                    responder_mailbox_pk: accept.responder_mailbox_pk,
                    profile_key: accept.profile_key,
                    encrypted_display_name: accept.encrypted_display_name,
                    encrypted_avatar: accept.encrypted_avatar,
                    timestamp: accept.timestamp,
                })
            }
            Some(content::Content::AnonDmMessage(msg)) => {
                Ok(DmContent::AnonDmMessage {
                    conversation_id: msg.conversation_id,
                    inner_content: msg.inner_content.ok_or_else(|| {
                        ServiceError::Validation("AnonDmMessage missing inner_content".into())
                    })?,
                })
            }
            // L4 mailbox rotation announce: peer minted a new mailbox and wants
            // future sends directed there. Does not touch the ratchet.
            Some(content::Content::DmMailboxRotation(rotation)) => {
                Ok(DmContent::MailboxRotation {
                    new_mailbox_pk: rotation.new_mailbox_pk,
                })
            }
            // Session switch after the peer reset its sessions with us: the
            // decrypt that produced this already adopted the new session.
            Some(content::Content::NullMessage(_)) => Ok(DmContent::NullMessage),
            _ => {
                Err(ServiceError::Validation("Unknown or empty content type".into()))
            }
        }
    }

    /// Process an incoming sender key (group) encrypted message.
    ///
    /// Sender key messages use Signal Protocol's sender keys for efficient
    /// one-to-many encryption in channels.
    ///
    /// # Arguments
    ///
    /// * `encrypted_data` - The EncryptedData containing serialized GroupEncrypted
    /// * `sender_id` - The sender's user ID
    /// * `sender_device_id` - The sender's device ID
    ///
    /// # Returns
    ///
    /// - `Ok(DecryptResult::Plaintext(bytes))` - Successfully decrypted
    /// - `Ok(DecryptResult::NeedsSenderKey { channel_id, sender_id })` - Need to request sender's SKDM
    /// - `Err(_)` - Other error
    /// Install a sealed-sender-authenticated SKDM, gated on the channel's
    /// VERIFIED membership (Design B). Its `(revision, state_hash)` stamp is
    /// cross-checked first — a sender ahead of us is recorded for a change
    /// fetch even when not (yet) a member of our state; a fork counts only
    /// from a verified member. A sender outside the verified distribution set
    /// is not stored (`Ok(None)`): the server alone cannot make anyone a
    /// sender in our channels. `Ok(Some(profile_key_changed))` once stored.
    pub(crate) async fn accept_skdm(
        &self,
        sender_id: &UserId,
        sender_device_id: u32,
        channel_id: &crate::domain::ids::ChannelId,
        skdm: &crate::proto::content_v1::SenderKeyDistribution,
    ) -> Result<Option<bool>, ServiceError> {
        let gate = crate::services::message::GroupStateGate::new(&self.settings_repo);
        let is_member = gate.admits(channel_id, sender_id);
        gate.cross_check(channel_id, is_member, skdm.state_revision, &skdm.state_hash);
        if !is_member {
            tracing::warn!(%sender_id, %channel_id,
                "GATEWAY: SKDM from a sender outside the verified membership; not stored");
            return Ok(None);
        }
        self.signal_service
            .process_sender_key(sender_id, sender_device_id, skdm)
            .await
            .inspect_err(|e| {
                tracing::error!(%sender_id, %channel_id, error = %e, "GATEWAY: Failed to process sender key");
            })?;
        tracing::info!(%sender_id, %channel_id, "GATEWAY: Successfully stored sender key from channel member");
        // The member's profile key rides their SKDM (Signal shares it with
        // the group), so a co-member who is not a friend can decrypt their
        // name and avatar. Stored against the sealed-sender-authenticated
        // sender only — the key is theirs; a blocked sender never reaches
        // this point (checked by the caller).
        Ok(Some(self.store_skdm_profile_key(sender_id, skdm.profile_key.as_deref())))
    }

    pub async fn process_group_encrypted_message(
        &self,
        encrypted_data: &crate::proto::chat_v1::EncryptedData,
        sender_id: &UserId,
        sender_device_id: u32,
    ) -> Result<DecryptResult, ServiceError> {
        use crate::domain::ids::ChannelId;

        tracing::info!(
            sender_id = %sender_id,
            sender_device_id = sender_device_id,
            ciphertext_len = encrypted_data.ciphertext.len(),
            associated_data = ?String::from_utf8_lossy(&encrypted_data.associated_data),
            "[MSG_RECV] Processing group encrypted message"
        );

        // Check if this is a sender key message (two formats supported)
        // - `sender_key` (direct channel messages)
        // - `sender:{user_id}:channel:{channel_id}` (group mailbox fanout)
        let is_sender_key_msg = {
            if encrypted_data.associated_data == b"sender_key" {
                true
            } else {
                let associated_str = String::from_utf8_lossy(&encrypted_data.associated_data);
                associated_str.starts_with("sender:") && associated_str.contains(":channel:")
            }
        };
        if !is_sender_key_msg {
            tracing::warn!(
                associated_data = ?String::from_utf8_lossy(&encrypted_data.associated_data),
                "[MSG_RECV] Not a sender key message, rejecting"
            );
            return Err(ServiceError::Validation(
                "Not a sender key encrypted message".into(),
            ));
        }

        // Deserialize the GroupEncrypted proto from ciphertext field
        let group_encrypted = GroupEncrypted::decode(encrypted_data.ciphertext.as_slice())
            .map_err(|e| {
                tracing::error!(
                    error = %e,
                    "[MSG_RECV] Failed to decode GroupEncrypted proto"
                );
                ServiceError::Validation(format!("Failed to decode GroupEncrypted: {}", e))
            })?;

        let channel_id: ChannelId = group_encrypted.channel_id.parse().map_err(|e| {
            tracing::error!(
                channel_id_str = %group_encrypted.channel_id,
                error = %e,
                "[MSG_RECV] Invalid channel_id in GroupEncrypted"
            );
            ServiceError::Validation(format!("Invalid channel_id in GroupEncrypted: {}", e))
        })?;

        tracing::debug!(
            channel_id = %channel_id,
            sender_id = %sender_id,
            sender_device_id = sender_device_id,
            inner_ciphertext_len = group_encrypted.ciphertext.len(),
            "[MSG_RECV] Extracted GroupEncrypted, attempting decryption"
        );

        // Only a member of our VERIFIED membership may speak in the channel
        // (Signal drops group messages from non-members); checked before
        // decrypting so a non-member cannot draw a SenderKeyRequest either.
        let gate = crate::services::message::GroupStateGate::new(&self.settings_repo);
        if !gate.admits(&channel_id, sender_id) {
            tracing::warn!(%channel_id, %sender_id,
                "[MSG_RECV] Group message from a sender outside the verified membership; dropped");
            return Err(ServiceError::Validation(
                "group message from a sender outside the verified channel membership".into(),
            ));
        }

        // Try to decrypt using sender keys
        match self
            .signal_service
            .decrypt_from_group(sender_id, sender_device_id, &group_encrypted)
            .await
        {
            Ok(plaintext) => {
                // The sender's verified (revision, state_hash) stamp. A peer
                // ahead is recorded; `MessageService::sync_channel_state_if_behind`
                // has the channel service fetch the changes.
                if let Some(content::Content::DataMessage(dm)) =
                    Content::decode(plaintext.as_slice()).ok().and_then(|c| c.content)
                {
                    gate.cross_check(&channel_id, true, dm.group_state_revision, &dm.group_state_hash);
                }
                tracing::info!(
                    sender_id = %sender_id,
                    channel_id = %channel_id,
                    plaintext_len = plaintext.len(),
                    "[MSG_RECV] Successfully decrypted sender key message"
                );
                Ok(DecryptResult::Plaintext(plaintext))
            }
            Err(e) => {
                // Check if this is a "no sender key" error
                let error_str = e.to_string();
                tracing::warn!(
                    sender_id = %sender_id,
                    channel_id = %channel_id,
                    error = %error_str,
                    "[MSG_RECV] Decryption failed, checking if sender key is missing"
                );

                if error_str.contains("no sender key")
                    || error_str.contains("SenderKeyRecord")
                    || error_str.contains("not found")
                {
                    tracing::warn!(
                        sender_id = %sender_id,
                        channel_id = %channel_id,
                        "[MSG_RECV] MISSING SENDER KEY - need to request SKDM from sender"
                    );
                    Ok(DecryptResult::NeedsSenderKey {
                        channel_id,
                        sender_id: *sender_id,
                    })
                } else {
                    // Other decryption error
                    tracing::error!(
                        sender_id = %sender_id,
                        channel_id = %channel_id,
                        error = %error_str,
                        "[MSG_RECV] Decryption failed with non-recoverable error"
                    );
                    Err(e)
                }
            }
        }
    }
}


#[cfg(test)]
mod thread_id_tests {
    //! Regression test for a real bug: `DmContent::Text` (and, downstream,
    //! `dispatch_dm_content` in `inbox/polling/dm.rs`) silently dropped
    //! `DataMessage.thread_id` on the mailbox (Shield friend DM) receive
    //! path. The ServerDm/channel receive path (`gateway/mod.rs`) already
    //! carried this through -- see its `thread_id: data_message.thread_id
    //! .parse().ok()` and comment. The mailbox path never got the same fix, so a
    //! thread reply received over that transport was filed into the main
    //! conversation instead of the open thread panel.
    use super::*;
    use crate::proto::content_v1::DataMessage;

    /// An edit keeps its own timestamp (`EditMessage.data_message.timestamp`):
    /// edits order and date by it.
    #[test]
    fn parse_content_carries_the_edit_timestamp() {
        let content = Content {
            content: Some(content::Content::Edit(crate::proto::content_v1::EditMessage {
                target_sent_timestamp: 100,
                data_message: Some(DataMessage { body: "fixed".into(), timestamp: 150, ..Default::default() }),
            })),
            ..Default::default()
        };
        match MessageGateway::parse_content(content).expect("parse_content failed") {
            DmContent::Edit { target_sent_timestamp, new_body, edit_timestamp } => {
                assert_eq!((target_sent_timestamp, new_body.as_str(), edit_timestamp), (100, "fixed", 150));
            }
            other => panic!("expected DmContent::Edit, got {other:?}"),
        }
    }

    /// §124 1k: a ring and a key request parse with no key; a key parses
    /// only as a 32-byte rotation at epoch >= 1 (epoch 0 is a participant's
    /// own starting key and is never sent).
    #[test]
    fn group_call_signals_parse_and_epoch_zero_keys_are_refused() {
        use crate::domain::call::GroupCallSignal;
        use crate::services::gateway::build_group_call_signal_content;
        let parse = |s| MessageGateway::parse_content(build_group_call_signal_content("c", "ch", s));
        for signal in [GroupCallSignal::Ring, GroupCallSignal::KeyRequest, GroupCallSignal::Key { key: [3; 32], epoch: 2 }] {
            match parse(signal).expect("parse_content failed") {
                DmContent::GroupCallSignal { call_id, channel_id, signal: parsed, .. } => {
                    assert_eq!((call_id.as_str(), channel_id.as_str()), ("c", "ch"));
                    assert_eq!(parsed, signal);
                }
                other => panic!("expected DmContent::GroupCallSignal, got {other:?}"),
            }
        }
        assert!(parse(GroupCallSignal::Key { key: [3; 32], epoch: 0 }).is_err(), "epoch-0 key accepted");
    }

    #[test]
    fn parse_content_carries_thread_id_through() {
        let content = Content {
            content: Some(content::Content::DataMessage(DataMessage {
                body: "a reply".to_string(),
                thread_id: "11111111-1111-1111-1111-111111111111".to_string(),
                ..Default::default()
            })),
            ..Default::default()
        };

        let parsed = MessageGateway::parse_content(content).expect("parse_content failed");
        match parsed {
            DmContent::Text { thread_id, .. } => {
                assert_eq!(
                    thread_id,
                    Some("11111111-1111-1111-1111-111111111111".to_string()),
                    "thread_id must survive parse_content, or a mailbox-delivered reply \
                     is indistinguishable from a top-level message"
                );
            }
            other => panic!("expected DmContent::Text, got {other:?}"),
        }
    }

    #[test]
    fn parse_content_thread_id_is_none_when_not_a_reply() {
        let content = Content {
            content: Some(content::Content::DataMessage(DataMessage {
                body: "not a reply".to_string(),
                thread_id: String::new(),
                ..Default::default()
            })),
            ..Default::default()
        };

        let parsed = MessageGateway::parse_content(content).expect("parse_content failed");
        match parsed {
            DmContent::Text { thread_id, .. } => {
                assert_eq!(thread_id, None, "empty wire thread_id must map to None, not Some(\"\")");
            }
            other => panic!("expected DmContent::Text, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod null_message_tests {
    use super::*;

    /// A peer's post-reset `NullMessage` is handled content with nothing in
    /// it — not the "unknown content" error the mailbox poll loop reports as
    /// a decrypt failure (`DmDecryptionFailed` + a delivery-issue marker).
    #[test]
    fn null_message_parses_as_handled_no_op() {
        let content = Content {
            content: Some(content::Content::NullMessage(crate::proto::content_v1::NullMessage::default())),
            padding: vec![0; 200],
            ..Default::default()
        };
        assert_eq!(content_type_name(&content.content), "NullMessage");
        let parsed = MessageGateway::parse_content(content).expect("NullMessage must not be an error");
        assert!(matches!(parsed, DmContent::NullMessage), "got {parsed:?}");
    }
}

/// A decrypted plaintext that is not a `Content` proto. Every sender wraps
/// its plaintext in `Content` (the bare-`DataMessage` and raw-UTF-8 fallbacks
/// that used to accept anything here were removed), so this is refused.
fn not_content(len: usize) -> ServiceError {
    ServiceError::Validation(format!("decrypted plaintext ({len} bytes) is not a Content proto"))
}
