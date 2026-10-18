use std::time::SystemTime;

use libsignal_protocol::{
    CiphertextMessageType, IdentityKeyStore, ProtocolAddress, SessionStore, SignalProtocolError,
    message_encrypt, sealed_sender_encrypt, sealed_sender_encrypt_from_usmc,
};
// SECURITY: each ephemeral-key draw must hit the OS directly so a process
// memory disclosure cannot rewind the RNG.
//
// In rand 0.9, `OsRng` implements only `TryRngCore + TryCryptoRng` (its
// syscalls are fallible). libsignal-protocol wants the infallible
// `RngCore + CryptoRng` traits. `OsRng.unwrap_err()` returns an
// `UnwrapErr<OsRng>` adapter that satisfies both - each draw still goes to
// the OS getrandom() syscall; on the (effectively impossible on Linux post-
// boot) failure path it panics rather than silently producing weak output.
//
// We deliberately do NOT use `StdRng::from_os_rng()` here: that seeds a
// userspace ChaCha PRG once and then every draw within the call comes from
// that single seed - a process memory disclosure recovers the seed and
// reproduces every ephemeral key.
use rand::TryRngCore;
use rand::rngs::OsRng;

use crate::error::CryptoError;

use super::{
    ContentHint, MAX_SEALED_BODY_BYTES, SealedSenderManager, SenderCertificate,
    UnidentifiedSenderMessageContent, validate_sealed_body_size,
};

impl SealedSenderManager {
    /// Encrypt against an in-memory write overlay, then admit ALL envelopes
    /// before persisting any ratchet state. Return these exact ciphertexts:
    /// never probe-and-re-encrypt, estimate Kyber/SPQR overhead or classify a
    /// server error as safe. The actor serializes preparation and commit.
    pub(crate) async fn encrypt_bounded_batch(
        plaintext: &[u8],
        sender_cert: &SenderCertificate,
        destinations: &[ProtocolAddress],
        group_id: Option<&[u8]>,
        base: &crate::crypto::SignalStore,
        permit: &crate::services::gateway::submission::SubmissionPermit,
    ) -> Result<Vec<(ProtocolAddress, Vec<u8>, u32)>, crate::error::ServiceError> {
        let mut staged = super::super::store::StagedSignalStore::new(base.clone());
        let mut identities = staged.clone();
        let mut envelopes = Vec::with_capacity(destinations.len());
        for destination in destinations {
            if envelopes
                .iter()
                .any(|(address, _, _)| address == destination)
            {
                return Err(crate::error::ServiceError::Validation(
                    "duplicate sealed destination".into(),
                ));
            }
            let ciphertext = Self::encrypt_in_group(
                plaintext,
                sender_cert,
                destination,
                group_id,
                &mut staged,
                &mut identities,
            )
            .await
            .map_err(crate::error::ServiceError::Crypto)?;
            validate_sealed_body_size(ciphertext.len(), MAX_SEALED_BODY_BYTES)?;
            let record = staged
                .load_session(destination)
                .await
                .map_err(|e| {
                    crate::error::ServiceError::Crypto(CryptoError::Protocol(e.to_string()))
                })?
                .ok_or_else(|| {
                    crate::error::ServiceError::Validation("missing staged session".into())
                })?;
            let registration_id = record.remote_registration_id().map_err(|e| {
                crate::error::ServiceError::Crypto(CryptoError::Protocol(e.to_string()))
            })?;
            if registration_id == 0 {
                return Err(crate::error::ServiceError::Validation(
                    "staged session has no registration id".into(),
                ));
            }
            envelopes.push((destination.clone(), ciphertext, registration_id));
        }
        if !envelopes.is_empty() {
            permit
                .start()
                .map_err(crate::error::ServiceError::Storage)?;
            staged.commit().await.map_err(|e| {
                crate::error::ServiceError::Crypto(CryptoError::Protocol(e.to_string()))
            })?;
        }
        Ok(envelopes)
    }

    /// Encrypt plaintext to sealed sender format.
    ///
    /// This is the simplest way to send a sealed sender message. It encrypts
    /// the plaintext, creates the USMC, and produces the final sealed envelope.
    ///
    /// # Arguments
    ///
    /// * `plaintext` - The message content to encrypt
    /// * `sender_cert` - The sender's certificate (obtained from server)
    /// * `destination` - The recipient's protocol address
    /// * `session_store` - Session storage for the encryption
    /// * `identity_store` - Identity key storage
    ///
    /// # Returns
    ///
    /// The encrypted sealed sender message bytes.
    pub async fn encrypt<S, I>(
        plaintext: &[u8],
        sender_cert: &SenderCertificate,
        destination: &ProtocolAddress,
        session_store: &mut S,
        identity_store: &mut I,
    ) -> Result<Vec<u8>, CryptoError>
    where
        S: SessionStore,
        I: IdentityKeyStore,
    {
        tracing::debug!(
            destination = %destination,
            plaintext_len = plaintext.len(),
            "SealedSender: starting encryption"
        );

        let mut rng = OsRng.unwrap_err();
        let now = SystemTime::now();

        tracing::debug!(destination = %destination, "SealedSender: calling sealed_sender_encrypt");
        let result = sealed_sender_encrypt(
            destination,
            sender_cert,
            plaintext,
            session_store,
            identity_store,
            now,
            &mut rng,
        )
        .await
        .map_err(|e| {
            tracing::error!(
                destination = %destination,
                error = %e,
                "SealedSender: encryption failed"
            );
            CryptoError::EncryptionFailed(e.to_string())
        })?;

        tracing::debug!(
            destination = %destination,
            ciphertext_len = result.len(),
            "SealedSender: encryption complete"
        );
        Ok(result)
    }

    /// [`Self::encrypt`] with the USMC's group id set (Signal's `groupId`),
    /// libsignal's `sealed_sender_encrypt` otherwise verbatim. A pairwise copy
    /// of a channel message names its channel there, inside the sealed layer,
    /// so a recipient that cannot decrypt it still knows where the loss
    /// belongs.
    pub(crate) async fn encrypt_in_group<S, I>(
        plaintext: &[u8],
        sender_cert: &SenderCertificate,
        destination: &ProtocolAddress,
        group_id: Option<&[u8]>,
        session_store: &mut S,
        identity_store: &mut I,
    ) -> Result<Vec<u8>, CryptoError>
    where
        S: SessionStore,
        I: IdentityKeyStore,
    {
        let Some(group_id) = group_id else {
            return Self::encrypt(
                plaintext,
                sender_cert,
                destination,
                session_store,
                identity_store,
            )
            .await;
        };
        let failed = |e: SignalProtocolError| CryptoError::EncryptionFailed(e.to_string());
        let mut rng = OsRng.unwrap_err();
        let sender_address = ProtocolAddress::new(
            sender_cert.sender_uuid().map_err(failed)?.to_owned(),
            sender_cert.sender_device_id().map_err(failed)?,
        );
        let message = message_encrypt(
            plaintext,
            destination,
            &sender_address,
            session_store,
            identity_store,
            SystemTime::now(),
            &mut rng,
        )
        .await
        .map_err(failed)?;
        let usmc = UnidentifiedSenderMessageContent::new(
            message.message_type(),
            sender_cert.clone(),
            message.serialize().to_vec(),
            ContentHint::Default,
            Some(group_id.to_vec()),
        )
        .map_err(failed)?;
        sealed_sender_encrypt_from_usmc(destination, &usmc, identity_store, &mut rng)
            .await
            .map_err(failed)
    }

    /// Create UnidentifiedSenderMessageContent from pre-encrypted message.
    ///
    /// Use this when you already have an encrypted message (WhisperMessage or
    /// PreKeyMessage) and want to wrap it in sealed sender format.
    ///
    /// # Arguments
    ///
    /// * `msg_type` - The type of the inner message (Whisper or PreKey)
    /// * `sender_cert` - The sender's certificate
    /// * `contents` - The encrypted message bytes
    /// * `content_hint` - Hint about the content type for recipient
    /// * `group_id` - Optional group ID for group messages
    ///
    /// # Returns
    ///
    /// The USMC ready for encryption with `encrypt_from_content()`.
    pub fn create_content(
        msg_type: CiphertextMessageType,
        sender_cert: SenderCertificate,
        contents: Vec<u8>,
        content_hint: ContentHint,
        group_id: Option<Vec<u8>>,
    ) -> Result<UnidentifiedSenderMessageContent, CryptoError> {
        UnidentifiedSenderMessageContent::new(
            msg_type,
            sender_cert,
            contents,
            content_hint,
            group_id,
        )
        .map_err(|e| CryptoError::Protocol(e.to_string()))
    }

    /// Encrypt a pre-built USMC to sealed sender format.
    ///
    /// This is useful when sending to multiple recipients - you can create the
    /// USMC once and encrypt it for each recipient.
    ///
    /// # Arguments
    ///
    /// * `usmc` - The UnidentifiedSenderMessageContent to encrypt
    /// * `destination` - The recipient's protocol address
    /// * `identity_store` - Identity key storage
    ///
    /// # Returns
    ///
    /// The encrypted sealed sender message bytes.
    pub async fn encrypt_from_content<I>(
        usmc: &UnidentifiedSenderMessageContent,
        destination: &ProtocolAddress,
        identity_store: &I,
    ) -> Result<Vec<u8>, CryptoError>
    where
        I: IdentityKeyStore,
    {
        let mut rng = OsRng.unwrap_err();

        sealed_sender_encrypt_from_usmc(destination, usmc, identity_store, &mut rng)
            .await
            .map_err(|e| CryptoError::EncryptionFailed(e.to_string()))
    }
}
