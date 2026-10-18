use super::*;

impl SignalService {
    /// Decrypt an incoming Sealed Sender message.
    ///
    /// Returns the authenticated sender, the plaintext, and the inner
    /// `CiphertextMessageType`. Callers MUST reject
    /// `CiphertextMessageType::Plaintext` unless they are the
    /// DecryptionErrorMessage handler: those bytes skipped the Double Ratchet,
    /// so nothing checked the sender's identity key, replayed counters, or
    /// forward secrecy.
    ///
    /// `server_timestamp_ns` is the envelope's server timestamp; the sender
    /// certificate's expiry is judged against it, not the local clock, so a
    /// message read late (device offline) or under clock skew still opens.
    /// Zero means the server sent none and is refused: it would disable the
    /// expiry check outright.
    pub async fn decrypt_message(
        &self,
        ciphertext: &[u8],
        server_timestamp_ns: u64,
    ) -> Result<(ProtocolAddress, Vec<u8>, CiphertextMessageType), ServiceError> {
        if server_timestamp_ns == 0 {
            return Err(ServiceError::Validation(
                "sealed sender envelope has no server timestamp".into(),
            ));
        }
        let validation_time =
            libsignal_protocol::Timestamp::from_epoch_millis(server_timestamp_ns / 1_000_000);
        let local_uuid = self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)?
            .ok_or(ServiceError::NotLoggedIn)?;
        let local_device_id = self
            .settings_repo
            .get::<u32>(keys::CURRENT_DEVICE_ID)?
            .ok_or(ServiceError::NotLoggedIn)?;
        let device_id_struct = DeviceId::new(
            local_device_id
                .try_into()
                .map_err(|_| ServiceError::Validation("ID too large".into()))?,
        )
        .map_err(|_| ServiceError::Validation("Invalid ID".into()))?;

        self.actor
            .decrypt_sealed(
                ciphertext.to_vec(),
                self.trust_root,
                validation_time,
                local_uuid,
                device_id_struct,
            )
            .await
            .inspect_err(|e| self.note_identity_change(e))
    }

    /// Anonymous reaction receive journal. Caller supplies the actual transport
    /// scope/envelope binding; the authenticated account/device are checked here
    /// and again by the actor's SQLite commit/replay boundary.
    pub async fn decrypt_reaction_durable(
        &self,
        ciphertext: &[u8],
        key: &crate::storage::receive_artifact::ReceiveKey,
    ) -> Result<super::super::actor::DurableReceive, super::super::actor::DurableReceiveError> {
        key.validate()?;
        let account = self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)?
            .ok_or(ServiceError::NotLoggedIn)?;
        let device = self
            .settings_repo
            .get::<u32>(keys::CURRENT_DEVICE_ID)?
            .ok_or(ServiceError::NotLoggedIn)?;
        if account != key.local_account || device != key.local_device {
            return Err(ServiceError::Validation("receive account/device mismatch".into()).into());
        }
        self.actor
            .decrypt_reaction_durable(ciphertext.to_vec(), self.trust_root, key.clone())
            .await
            .inspect_err(|e| {
                if let super::super::actor::DurableReceiveError::Failure(source) = e {
                    self.note_identity_change(source)
                }
            })
    }

    /// Decrypt an incoming identified sender message.
    pub async fn decrypt_message_identified(
        &self,
        ciphertext: &[u8],
        sender_id: &str,
        sender_device_id: u32,
    ) -> Result<Vec<u8>, ServiceError> {
        let device_id_struct = DeviceId::try_from(sender_device_id)
            .map_err(|_| ServiceError::Validation("Invalid ID".into()))?;
        let sender_address = ProtocolAddress::new(sender_id.to_string(), device_id_struct);

        self.actor
            .decrypt_identified(ciphertext.to_vec(), sender_address)
            .await
            .inspect_err(|e| self.note_identity_change(e))
    }
}
