use super::*;

/// Commands sent to the Signal Actor.
pub(crate) enum SignalCommand {
    /// FIFO fence: earlier submitted crypto has finished before acknowledgement.
    Barrier { response: oneshot::Sender<()> },
    #[cfg(test)]
    TestPause { entered: oneshot::Sender<()>, resume: oneshot::Receiver<()> },
    GetIdentityFingerprint {
        response: oneshot::Sender<Result<String, ServiceError>>,
    },
    GetIdentityPublicKey {
        response: oneshot::Sender<Result<[u8; 33], ServiceError>>,
    },
    /// Get the identity private key (32 bytes X25519 scalar) for the key
    /// backups. HPKE opens use `OpenIdentityHpke` instead.
    GetIdentityPrivateKey {
        response: oneshot::Sender<Result<[u8; 32], ServiceError>>,
    },
    /// Open an HPKE message sealed to our identity key (`crypto::ecies`)
    /// here, so the identity secret never leaves the actor for it.
    OpenIdentityHpke {
        purpose: crate::crypto::ecies::IdentityHpkeUse,
        ciphertext: Vec<u8>,
        response: oneshot::Sender<Result<Vec<u8>, ServiceError>>,
    },
    /// Sign a channel membership change (`crypto::signal::identity::sign_channel_change`):
    /// the only identity signature besides prekeys and the registration
    /// challenge. Takes typed `Actions`, never bytes. Returns
    /// `(actions_bytes, signature)`.
    SignChannelChange {
        actions: crate::proto::chat_v1::channel_change::Actions,
        response: oneshot::Sender<Result<(Vec<u8>, Vec<u8>), ServiceError>>,
    },
    RefreshPrekeys {
        count: u32,
        response: oneshot::Sender<
            Result<Vec<crate::proto::chat_v1::signal_pre_key_bundle::PreKeyRecord>, ServiceError>,
        >,
    },
    /// Generate `count` one-time Kyber prekeys (PQOPK) signed by the IK and
    /// return their proto-encoded form for upload via RefreshPreKeys. PQOPKs
    /// are spec-mandated alongside classical OPKs (PQXDH §3.2) and consumed
    /// 1:1 with them, so the client refills both in lockstep.
    RefreshKyberPrekeys {
        count: u32,
        response: oneshot::Sender<
            Result<
                Vec<crate::proto::chat_v1::signal_pre_key_bundle::KyberPreKeyRecord>,
                ServiceError,
            >,
        >,
    },
    EstablishSession {
        address: ProtocolAddress,
        bundle: PreKeyBundle,
        response: oneshot::Sender<Result<(), ServiceError>>,
    },
    EncryptSealed {
        plaintext: Vec<u8>,
        sender_cert: SenderCertificate,
        recipient_address: ProtocolAddress,
        response: oneshot::Sender<Result<Vec<u8>, ServiceError>>,
    },
    EncryptSealedBoundedBatch {
        plaintext: Vec<u8>,
        sender_cert: SenderCertificate,
        destinations: Vec<ProtocolAddress>,
        group_id: Option<Vec<u8>>,
        permit: crate::services::gateway::submission::SubmissionPermit,
        response: oneshot::Sender<Result<Vec<(ProtocolAddress, Vec<u8>, u32)>, ServiceError>>,
    },
    EncryptIdentified {
        plaintext: Vec<u8>,
        recipient_address: ProtocolAddress,
        response: oneshot::Sender<Result<(Vec<u8>, u8), ServiceError>>,
    },
    /// The `CiphertextMessageType` rides back with the plaintext on purpose:
    /// `Plaintext` means the bytes bypassed the Double Ratchet entirely and
    /// only the DecryptionErrorMessage handler may accept them.
    ///
    /// `validation_time` is the envelope's SERVER timestamp: the sender
    /// certificate is checked against when the server accepted the message,
    /// not when this device got round to reading it (Signal-Desktop
    /// `MessageReceiver.preload.ts` passes `envelope.serverTimestamp`).
    DecryptSealed {
        ciphertext: Vec<u8>,
        trust_root: PublicKey,
        validation_time: libsignal_protocol::Timestamp,
        local_uuid: String,
        local_device_id: DeviceId,
        response: oneshot::Sender<
            Result<(ProtocolAddress, Vec<u8>, CiphertextMessageType), ServiceError>,
        >,
    },
    DecryptReactionDurable {
        ciphertext: Vec<u8>,
        trust_root: PublicKey,
        key: crate::storage::receive_artifact::ReceiveKey,
        response: oneshot::Sender<Result<DurableReceive, DurableReceiveError>>,
    },
    DecryptIdentified {
        ciphertext: Vec<u8>,
        sender_address: ProtocolAddress,
        response: oneshot::Sender<Result<Vec<u8>, ServiceError>>,
    },
    CreateSenderKey {
        channel_id: ChannelId,
        our_address: ProtocolAddress,
        response: oneshot::Sender<Result<Vec<u8>, ServiceError>>,
    },
    ProcessSenderKey {
        sender_address: ProtocolAddress,
        skdm_bytes: Vec<u8>,
        response: oneshot::Sender<Result<(), ServiceError>>,
    },
    EncryptForGroup {
        channel_id: ChannelId,
        our_address: ProtocolAddress,
        plaintext: Vec<u8>,
        response: oneshot::Sender<Result<Vec<u8>, ServiceError>>,
    },
    DecryptFromGroup {
        sender_address: ProtocolAddress,
        ciphertext: Vec<u8>,
        response: oneshot::Sender<Result<Vec<u8>, ServiceError>>,
    },
    /// Derive an ephemeral call key with identity binding (for DM calls with PFS).
    ///
    /// Completes ephemeral call key exchange: ECDH + identity lookup + HKDF.
    /// All crypto stays in the service — UI just passes raw key bytes.
    CompleteCallKeyExchange {
        our_ephemeral_private: [u8; 32],
        peer_ephemeral_public: [u8; 32],
        peer_id: UserId,
        call_id: String,
        we_are_caller: bool,
        response: oneshot::Sender<Result<[u8; 32], ServiceError>>,
    },
    HasSenderKey {
        channel_id: ChannelId,
        our_address: ProtocolAddress,
        response: oneshot::Sender<Result<bool, ServiceError>>,
    },
    HasSession {
        address: ProtocolAddress,
        response: oneshot::Sender<Result<bool, ServiceError>>,
    },
    /// Delete every Signal session row whose address matches `{peer_user_id}:*`.
    /// Used by the PEER_DEVICES_CHANGED handler so a peer's device change
    /// forces fresh prekey fetch + session rebuild on the next send to them.
    /// Returns the number of session rows deleted.
    InvalidatePeerSessions {
        peer_user_id: String,
        response: oneshot::Sender<Result<u32, ServiceError>>,
    },
    /// Archive (not delete) the session for `(peer_user_id, device_id)` in
    /// response to a peer's DecryptionErrorMessage, so the next send
    /// re-establishes via a fresh PreKeySignalMessage while in-flight
    /// messages from the peer under the old ratchet still decrypt.
    ///
    /// If `dem_ratchet_key` is present it must match the session's CURRENT
    /// sender ratchet key — a stale DEM about an already-rotated session is
    /// ignored (Signal's guard against archiving a healthy new session).
    /// Returns true if the session was archived.
    ArchiveSessionForResend {
        peer_user_id: String,
        device_id: u32,
        dem_ratchet_key: Option<Vec<u8>>,
        response: oneshot::Sender<Result<bool, ServiceError>>,
    },
    /// Wrap a serialized DecryptionErrorMessage in `PlaintextContent` and
    /// seal it for `recipient_address`. Deliberately session-LESS: the
    /// ratchet with the recipient may be exactly what is broken, so the DEM
    /// rides as a `CiphertextMessageType::Plaintext` USMC, which sealed
    /// sender can encrypt with only the recipient's identity key.
    EncryptDecryptionErrorSealed {
        dem_serialized: Vec<u8>,
        sender_cert: SenderCertificate,
        recipient_address: ProtocolAddress,
        response: oneshot::Sender<Result<Vec<u8>, ServiceError>>,
    },
    ClearChannelSenderKeys {
        channel_id: ChannelId,
        response: oneshot::Sender<Result<(), ServiceError>>,
    },
    MarkChannelForRotation {
        channel_id: ChannelId,
        response: oneshot::Sender<Result<(), ServiceError>>,
    },
    /// Generate a Signal safety number for a peer (60-digit fingerprint).
    GenerateSafetyNumber {
        local_user_id: String,
        peer_user_id: String,
        response: oneshot::Sender<Result<String, ServiceError>>,
    },
    /// Generate scannable fingerprint bytes for QR code.
    GenerateScannableFingerprint {
        local_user_id: String,
        peer_user_id: String,
        response: oneshot::Sender<Result<Vec<u8>, ServiceError>>,
    },
    /// Rotate signed prekey + Kyber prekey (PQXDH pairing).
    /// Returns (signed_prekey_proto, kyber_prekey_proto) for upload.
    RotateSignedPreKey {
        response: oneshot::Sender<
            Result<
                (
                    crate::proto::chat_v1::signal_pre_key_bundle::SignedPreKeyRecord,
                    crate::proto::chat_v1::signal_pre_key_bundle::KyberPreKeyRecord,
                ),
                ServiceError,
            >,
        >,
    },
}
