use super::*;

impl MessageService {
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
        let original_ms =
            libsignal_protocol::DecryptionErrorMessage::try_from(dem_serialized.as_slice())
                .map_err(|e| MessageError::InvalidContent(format!("DecryptionErrorMessage: {e}")))?
                .timestamp()
                .epoch_millis();
        if !count_dem_retry(
            &mut DEM_RETRIES.lock(),
            (*peer_id, peer_device_id, original_ms),
        ) {
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
        // To our own user the deposit is identified by design (see
        // `deposit_sealed_to_session_devices`), not a degradation.
        if peer_uak.is_none() && !self.current_user_id().is_ok_and(|me| me == *peer_id) {
            tracing::warn!(
                peer_id = %peer_id,
                reason = "no stored profile key to derive the peer's UAK",
                "SEALED SENDER DEGRADED: DecryptionErrorMessage delivered identified \
                 (Basic auth) — the server WILL record us as the sender"
            );
        }
        let message = SealedDeviceMessage {
            device_id: peer_device_id,
            registration_id: 0,
            sealed_message: Some(sealed_message),
        };
        let (_, uak_rejected) = self
            .send_sealed_batch(
                &mut self.message_client(),
                &peer_id_str,
                vec![message],
                SealedDelivery::NamedDevices,
                peer_uak.as_ref(),
                &uuid::Uuid::new_v4().to_string(),
            )
            .await
            .map_err(MessageError::Network)?;
        if uak_rejected {
            tracing::warn!(peer_id = %peer_id, reason = "UAK from our stored profile key rejected", "SEALED SENDER DEGRADED: DecryptionErrorMessage delivered identified");
        }

        tracing::info!(
            peer_id = %peer_id,
            peer_device_id,
            "DecryptionErrorMessage report delivered — peer will archive its session and re-establish"
        );
        Ok(())
    }
}

/// `DecryptionErrorMessage`s sent per failed message: Signal-Desktop's
/// `RETRY_LIMIT` (`ts/util/handleRetry.preload.ts:55`). An envelope the
/// inbox keeps re-delivering, or a sender that keeps answering with
/// undecryptable resends, stops costing a report after this many.
pub(super) const DEM_RETRY_LIMIT: u32 = 5;

/// Distinct messages counted before the map is cleared wholesale, as
/// `group_retry_requests` is (`group.rs`). A forgotten count costs at most
/// `DEM_RETRY_LIMIT` more reports for that message.
pub(super) const DEM_RETRY_CAP: usize = 4096;

/// Reports sent per `(sender, sender device, original timestamp in epoch
/// millis)` — the key of Signal-Desktop's `retryRecord`, in memory as there.
/// Every DEM goes out through `send_decryption_error_report` (1:1 from the
/// inbox processor, sender-key from `request_group_resend`), so the count is
/// kept there.
static DEM_RETRIES: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<(UserId, u32, u64), u32>>,
> = std::sync::LazyLock::new(Default::default);

/// Count one more report for `key`; `false` once it is past the limit.
pub(super) fn count_dem_retry(
    retries: &mut std::collections::HashMap<(UserId, u32, u64), u32>,
    key: (UserId, u32, u64),
) -> bool {
    if retries.len() >= DEM_RETRY_CAP && !retries.contains_key(&key) {
        retries.clear();
    }
    let count = retries.entry(key).or_insert(0);
    *count += 1;
    *count <= DEM_RETRY_LIMIT
}
