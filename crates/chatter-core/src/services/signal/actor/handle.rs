use super::*;

impl SignalActorHandle {
    pub(crate) async fn drain_submitted(&self) -> Result<(), ServiceError> {
        self.send(|response| SignalCommand::Barrier { response }).await
    }
    #[cfg(test)]
    pub(crate) async fn test_pause(&self, entered: oneshot::Sender<()>, resume: oneshot::Receiver<()>) {
        self.sender.send(SignalCommand::TestPause { entered, resume }).await.unwrap();
    }

    async fn send<T>(
        &self,
        cmd_fn: impl FnOnce(oneshot::Sender<T>) -> SignalCommand,
    ) -> Result<T, ServiceError> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(cmd_fn(tx))
            .await
            .map_err(|_| ServiceError::Internal("Actor dropped".into()))?;
        rx.await
            .map_err(|_| ServiceError::Internal("Response dropped".into()))
    }

    pub async fn get_identity_fingerprint(&self) -> Result<String, ServiceError> {
        self.send(|tx| SignalCommand::GetIdentityFingerprint { response: tx })
            .await?
    }

    pub async fn get_identity_public_key(&self) -> Result<[u8; 33], ServiceError> {
        self.send(|tx| SignalCommand::GetIdentityPublicKey { response: tx })
            .await?
    }

    /// Get the identity private key (32 bytes X25519 scalar) for the key
    /// backups. HPKE opens use [`Self::open_identity_hpke`] instead.
    pub async fn get_identity_private_key(&self) -> Result<[u8; 32], ServiceError> {
        self.send(|tx| SignalCommand::GetIdentityPrivateKey { response: tx })
            .await?
    }

    /// Sign a channel membership change with the identity key, inside the
    /// actor. Returns `(actions_bytes, signature)`.
    pub async fn sign_channel_change(
        &self,
        actions: crate::proto::chat_v1::channel_change::Actions,
    ) -> Result<(Vec<u8>, Vec<u8>), ServiceError> {
        self.send(|tx| SignalCommand::SignChannelChange {
            actions,
            response: tx,
        })
        .await?
    }

    /// Open an HPKE message sealed to our identity key, inside the actor.
    pub async fn open_identity_hpke(
        &self,
        purpose: crate::crypto::ecies::IdentityHpkeUse,
        ciphertext: Vec<u8>,
    ) -> Result<Vec<u8>, ServiceError> {
        self.send(|tx| SignalCommand::OpenIdentityHpke {
            purpose,
            ciphertext,
            response: tx,
        })
        .await?
    }

    pub async fn refresh_prekeys(
        &self,
        count: u32,
    ) -> Result<Vec<crate::proto::chat_v1::signal_pre_key_bundle::PreKeyRecord>, ServiceError> {
        self.send(|tx| SignalCommand::RefreshPrekeys {
            count,
            response: tx,
        })
        .await?
    }

    /// Generate a batch of one-time Kyber prekeys (PQOPK) for upload.
    pub async fn refresh_kyber_prekeys(
        &self,
        count: u32,
    ) -> Result<Vec<crate::proto::chat_v1::signal_pre_key_bundle::KyberPreKeyRecord>, ServiceError>
    {
        self.send(|tx| SignalCommand::RefreshKyberPrekeys {
            count,
            response: tx,
        })
        .await?
    }

    pub async fn establish_session(
        &self,
        address: ProtocolAddress,
        bundle: libsignal_protocol::PreKeyBundle,
    ) -> Result<(), ServiceError> {
        self.send(|tx| SignalCommand::EstablishSession {
            address,
            bundle,
            response: tx,
        })
        .await?
    }

    pub async fn encrypt_sealed(
        &self,
        plaintext: Vec<u8>,
        sender_cert: SenderCertificate,
        recipient_address: ProtocolAddress,
    ) -> Result<Vec<u8>, ServiceError> {
        admit_outgoing_content(&plaintext)?;
        self.send(|tx| SignalCommand::EncryptSealed {
            plaintext,
            sender_cert,
            recipient_address,
            response: tx,
        })
        .await?
    }

    pub(crate) async fn encrypt_sealed_bounded_batch(
        &self,
        plaintext: Vec<u8>,
        sender_cert: SenderCertificate,
        destinations: Vec<ProtocolAddress>,
        group_id: Option<Vec<u8>>,
    ) -> Result<Vec<(ProtocolAddress, Vec<u8>, u32)>, ServiceError> {
        admit_outgoing_content(&plaintext)?;
        let permit = crate::services::gateway::submission::SubmissionPermit::capture();
        self.send(|tx| SignalCommand::EncryptSealedBoundedBatch {
            plaintext,
            sender_cert,
            destinations,
            group_id,
            permit,
            response: tx,
        })
        .await?
    }

    pub async fn encrypt_identified(
        &self,
        plaintext: Vec<u8>,
        recipient_address: ProtocolAddress,
    ) -> Result<(Vec<u8>, u8), ServiceError> {
        admit_outgoing_content(&plaintext)?;
        self.send(|tx| SignalCommand::EncryptIdentified {
            plaintext,
            recipient_address,
            response: tx,
        })
        .await?
    }

    pub async fn decrypt_sealed(
        &self,
        ciphertext: Vec<u8>,
        trust_root: libsignal_protocol::PublicKey,
        validation_time: libsignal_protocol::Timestamp,
        local_uuid: String,
        local_device_id: DeviceId,
    ) -> Result<(ProtocolAddress, Vec<u8>, CiphertextMessageType), ServiceError> {
        self.send(|tx| SignalCommand::DecryptSealed {
            ciphertext,
            trust_root,
            validation_time,
            local_uuid,
            local_device_id,
            response: tx,
        })
        .await?
    }

    pub async fn decrypt_reaction_durable(
        &self,
        ciphertext: Vec<u8>,
        trust_root: PublicKey,
        key: crate::storage::receive_artifact::ReceiveKey,
    ) -> Result<DurableReceive, DurableReceiveError> {
        self.send(|response| SignalCommand::DecryptReactionDurable {
            ciphertext,
            trust_root,
            key,
            response,
        })
        .await?
    }

    pub async fn decrypt_identified(
        &self,
        ciphertext: Vec<u8>,
        sender_address: ProtocolAddress,
    ) -> Result<Vec<u8>, ServiceError> {
        self.send(|tx| SignalCommand::DecryptIdentified {
            ciphertext,
            sender_address,
            response: tx,
        })
        .await?
    }

    pub async fn create_sender_key(
        &self,
        channel_id: ChannelId,
        our_address: ProtocolAddress,
    ) -> Result<Vec<u8>, ServiceError> {
        self.send(|tx| SignalCommand::CreateSenderKey {
            channel_id,
            our_address,
            response: tx,
        })
        .await?
    }

    pub async fn process_sender_key(
        &self,
        sender_address: ProtocolAddress,
        skdm_bytes: Vec<u8>,
    ) -> Result<(), ServiceError> {
        self.send(|tx| SignalCommand::ProcessSenderKey {
            sender_address,
            skdm_bytes,
            response: tx,
        })
        .await?
    }

    pub async fn encrypt_for_group(
        &self,
        channel_id: ChannelId,
        our_address: ProtocolAddress,
        plaintext: Vec<u8>,
    ) -> Result<Vec<u8>, ServiceError> {
        admit_outgoing_content(&plaintext)?;
        self.send(|tx| SignalCommand::EncryptForGroup {
            channel_id,
            our_address,
            plaintext,
            response: tx,
        })
        .await?
    }

    pub async fn decrypt_from_group(
        &self,
        sender_address: ProtocolAddress,
        ciphertext: Vec<u8>,
    ) -> Result<Vec<u8>, ServiceError> {
        self.send(|tx| SignalCommand::DecryptFromGroup {
            sender_address,
            ciphertext,
            response: tx,
        })
        .await?
    }

    /// Derive an ephemeral call key with identity binding (for DM calls with PFS).
    ///
    /// Completes ephemeral call key exchange: ECDH + identity lookup + HKDF.
    ///
    /// Takes raw ephemeral key bytes so callers never perform crypto operations.
    /// The actor handles ECDH(our_ephemeral, peer_ephemeral), looks up both
    /// identity keys, and derives the final call encryption key via HKDF.
    pub async fn complete_call_key_exchange(
        &self,
        our_ephemeral_private: [u8; 32],
        peer_ephemeral_public: [u8; 32],
        peer_id: UserId,
        call_id: String,
        we_are_caller: bool,
    ) -> Result<[u8; 32], ServiceError> {
        self.send(|tx| SignalCommand::CompleteCallKeyExchange {
            our_ephemeral_private,
            peer_ephemeral_public,
            peer_id,
            call_id,
            we_are_caller,
            response: tx,
        })
        .await?
    }

    pub async fn has_sender_key(
        &self,
        channel_id: ChannelId,
        our_address: ProtocolAddress,
    ) -> Result<bool, ServiceError> {
        self.send(|tx| SignalCommand::HasSenderKey {
            channel_id,
            our_address,
            response: tx,
        })
        .await?
    }

    pub async fn clear_channel_sender_keys(
        &self,
        channel_id: ChannelId,
    ) -> Result<(), ServiceError> {
        self.send(|tx| SignalCommand::ClearChannelSenderKeys {
            channel_id,
            response: tx,
        })
        .await?
    }

    pub async fn mark_channel_for_rotation(
        &self,
        channel_id: ChannelId,
    ) -> Result<(), ServiceError> {
        self.send(|tx| SignalCommand::MarkChannelForRotation {
            channel_id,
            response: tx,
        })
        .await?
    }

    pub async fn has_session(&self, address: ProtocolAddress) -> Result<bool, ServiceError> {
        self.send(|tx| SignalCommand::HasSession {
            address,
            response: tx,
        })
        .await?
    }

    /// Delete every Signal session for `peer_user_id` across all devices.
    /// Returns the number of sessions dropped. Caller must separately mark
    /// affected group channels for sender-key rotation (see
    /// `mark_channel_for_rotation`) when the peer is a group member.
    pub async fn invalidate_peer_sessions(
        &self,
        peer_user_id: String,
    ) -> Result<u32, ServiceError> {
        self.send(|tx| SignalCommand::InvalidatePeerSessions {
            peer_user_id,
            response: tx,
        })
        .await?
    }

    /// Archive the session for `(peer_user_id, device_id)` after receiving the
    /// peer's DecryptionErrorMessage. See [`SignalCommand::ArchiveSessionForResend`].
    pub async fn archive_session_for_resend(
        &self,
        peer_user_id: String,
        device_id: u32,
        dem_ratchet_key: Option<Vec<u8>>,
    ) -> Result<bool, ServiceError> {
        self.send(|tx| SignalCommand::ArchiveSessionForResend {
            peer_user_id,
            device_id,
            dem_ratchet_key,
            response: tx,
        })
        .await?
    }

    /// Seal a serialized DecryptionErrorMessage (as `PlaintextContent`) for a
    /// recipient device without touching the (possibly broken) session.
    pub async fn encrypt_decryption_error_sealed(
        &self,
        dem_serialized: Vec<u8>,
        sender_cert: SenderCertificate,
        recipient_address: ProtocolAddress,
    ) -> Result<Vec<u8>, ServiceError> {
        self.send(|tx| SignalCommand::EncryptDecryptionErrorSealed {
            dem_serialized,
            sender_cert,
            recipient_address,
            response: tx,
        })
        .await?
    }

    /// Rotate signed prekey + Kyber prekey (PQXDH pairing).
    /// Returns proto types ready for upload via RefreshPreKeysRequest.
    pub async fn rotate_signed_prekey(
        &self,
    ) -> Result<
        (
            crate::proto::chat_v1::signal_pre_key_bundle::SignedPreKeyRecord,
            crate::proto::chat_v1::signal_pre_key_bundle::KyberPreKeyRecord,
        ),
        ServiceError,
    > {
        self.send(|tx| SignalCommand::RotateSignedPreKey { response: tx })
            .await?
    }

    /// Generate a 60-digit safety number for verifying a peer's identity.
    pub async fn generate_safety_number(
        &self,
        local_user_id: String,
        peer_user_id: String,
    ) -> Result<String, ServiceError> {
        self.send(|tx| SignalCommand::GenerateSafetyNumber {
            local_user_id,
            peer_user_id,
            response: tx,
        })
        .await?
    }

    /// Generate scannable fingerprint bytes for QR code verification.
    pub async fn generate_scannable_fingerprint(
        &self,
        local_user_id: String,
        peer_user_id: String,
    ) -> Result<Vec<u8>, ServiceError> {
        self.send(|tx| SignalCommand::GenerateScannableFingerprint {
            local_user_id,
            peer_user_id,
            response: tx,
        })
        .await?
    }
}
