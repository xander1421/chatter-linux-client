use super::*;

impl MessageGateway {
    /// RECEIVER side of the 1:1 self-heal: we could not decrypt an inner
    /// Whisper/PreKey message from an authenticated sender. Build a
    /// `DecryptionErrorMessage` naming the failed ciphertext so the caller can
    /// ship it back to the sender (sealed, session-less `PlaintextContent`).
    pub(super) fn build_decryption_error_report(
        &self,
        sender_uuid: &str,
        sender_device_id: u32,
        msg_type: u8,
        failed_ciphertext: &[u8],
        reason: &str,
        group_id: Option<Vec<u8>>,
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
            channel_id: group_id
                .as_deref()
                .and_then(crate::domain::ChannelId::try_from_slice),
        })
    }

    /// SENDER side of the 1:1 self-heal: a peer reported (via
    /// `DecryptionErrorMessage`) that it could not decrypt a message WE sent.
    /// If the report names this device and its ratchet key still matches our
    /// current session with that peer device, archive the session so the next
    /// send re-establishes via a fresh PreKeySignalMessage.
    pub(super) async fn handle_decryption_error_report(
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
}
