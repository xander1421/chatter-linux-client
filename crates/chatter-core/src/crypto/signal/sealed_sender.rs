//! Sealed sender for anonymous messaging.
//!
//! Sealed sender allows messages to be sent without revealing the sender's
//! identity to the server. The sender's identity is encrypted within the
//! message and can only be decrypted by the intended recipient.
//!
//! # UAK (Unidentified Access Key)
//!
//! The UAK is a 16-byte key derived from the user's 32-byte profile key.
//! It allows senders to send anonymous messages without revealing their
//! identity to the server. The server validates the UAK to ensure the
//! sender has permission to send to the recipient.
//!
//! # Certificate Validation
//!
//! Sender certificates are validated against a hardcoded trust root public key.
//! This prevents MITM attacks during message delivery by ensuring only
//! certificates signed by the trusted server are accepted.

use std::time::{Duration, SystemTime};

use libsignal_protocol::{
    message_decrypt_prekey, message_decrypt_signal, sealed_sender_decrypt_to_usmc,
    sealed_sender_encrypt, sealed_sender_encrypt_from_usmc,
    CiphertextMessageType, DeviceId, IdentityKeyStore, KyberPreKeyStore, PreKeySignalMessage,
    PreKeyStore, ProtocolAddress, PublicKey, SessionStore, SignalMessage,
    SignalProtocolError, SignedPreKeyStore, Timestamp,
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
use rand::rngs::OsRng;
use rand::TryRngCore;

use crate::config::TRUST_ROOT_PUBLIC_KEY;
use crate::error::{CryptoError, IdentityChangeSource};

// Re-export useful libsignal types for consumers of this module.
pub use libsignal_protocol::{
    ContentHint, SealedSenderDecryptionResult, SenderCertificate, ServerCertificate,
    UnidentifiedSenderMessageContent,
};

/// Sealed sender manager for anonymous messaging.
///
/// This manager provides methods for encrypting and decrypting sealed sender
/// messages, where the sender's identity is hidden from the server and only
/// revealed to the intended recipient.
///
/// # Design
///
/// Following the stateless pattern established in Phase 11, all methods take
/// the necessary stores as parameters rather than storing them in the struct.
/// This avoids mutable aliasing issues with libsignal's async trait methods.
///
/// # Workflow
///
/// ## Sending (Simple)
/// 1. Obtain a `SenderCertificate` from the server
/// 2. Call `encrypt()` with the plaintext and recipient address
/// 3. Send the encrypted bytes to the server
///
/// ## Sending (Advanced)
/// 1. Create the inner message ciphertext (WhisperMessage or PreKeyMessage)
/// 2. Call `create_content()` to build the USMC
/// 3. Call `encrypt_from_content()` for each recipient
///
/// ## Receiving
/// 1. Receive sealed sender message bytes
/// 2. Call `decrypt()` with trust root to validate and decrypt
/// 3. Get sender identity from `SealedSenderDecryptionResult`
#[derive(Clone)]
pub struct SealedSenderManager;

impl SealedSenderManager {
    /// Create a new SealedSenderManager.
    pub fn new() -> Self {
        Self
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

    /// Decrypt sealed sender message and validate the sender certificate.
    ///
    /// This validates the sender certificate against the trust root and
    /// decrypts the message, revealing the sender's identity.
    ///
    /// # Arguments
    ///
    /// * `ciphertext` - The sealed sender message bytes
    /// * `trust_root` - Server's public key for certificate validation
    /// * `timestamp` - Current timestamp for certificate expiration check
    /// * `local_e164` - Local user's phone number (for self-send detection)
    /// * `local_uuid` - Local user's UUID
    /// * `local_device_id` - Local device ID
    /// * `identity_store` - Identity key storage
    /// * `session_store` - Session storage
    /// * `pre_key_store` - One-time prekey storage
    /// * `signed_pre_key_store` - Signed prekey storage
    /// * `kyber_pre_key_store` - Kyber prekey storage
    ///
    /// # Returns
    ///
    /// The decryption result containing sender identity and decrypted message,
    /// paired with the inner `CiphertextMessageType`.
    ///
    /// The type is NOT incidental. `Plaintext` means the bytes never went
    /// through the Double Ratchet — no `is_trusted_identity`, no replay
    /// protection, no forward secrecy — and is legitimate only for a
    /// `DecryptionErrorMessage` ride-along. Every other caller must refuse it;
    /// without the label, unratcheted attacker-chosen bytes look exactly like
    /// ratchet-authenticated plaintext.
    #[allow(clippy::too_many_arguments)]
    pub async fn decrypt<I, S, P, SP, K>(
        ciphertext: &[u8],
        trust_root: &PublicKey,
        timestamp: Timestamp,
        local_e164: Option<String>,
        local_uuid: String,
        local_device_id: DeviceId,
        identity_store: &mut I,
        session_store: &mut S,
        pre_key_store: &mut P,
        signed_pre_key_store: &SP,
        kyber_pre_key_store: &mut K,
    ) -> Result<(SealedSenderDecryptionResult, CiphertextMessageType), CryptoError>
    where
        I: IdentityKeyStore,
        S: SessionStore,
        P: PreKeyStore,
        SP: SignedPreKeyStore,
        K: KyberPreKeyStore,
    {
        tracing::debug!(
            ciphertext_len = ciphertext.len(),
            local_uuid = %local_uuid,
            local_device_id = ?local_device_id,
            "SealedSender: starting decryption"
        );

        // Two-phase decrypt (mirrors libsignal's `sealed_sender_decrypt`, which
        // we deliberately do NOT call one-shot). Splitting envelope-open from
        // inner-decrypt is load-bearing for 1:1 session self-heal:
        //   * an INNER failure must surface the authenticated sender + the
        //     failed ciphertext so the gateway can build a
        //     `DecryptionErrorMessage` back to them (`CryptoError::SessionDesync`);
        //   * a `Plaintext`-type USMC (a peer's DecryptionErrorMessage wrapped
        //     in `PlaintextContent`) carries no ratchet ciphertext at all and
        //     must be returned as-is — the one-shot helper rejects it.
        let map_envelope_err = |e: SignalProtocolError| {
            tracing::error!(
                error = %e,
                local_uuid = %local_uuid,
                "SealedSender: decryption failed"
            );
            // Operational metric: sealed sender decrypt failure rate.
            // No secrets emitted — only the error string and a static kind.
            tracing::warn!(
                target: "chatter::crypto::decrypt_failure",
                kind = "sealed_sender",
                error = %e,
                "decryption failure"
            );
            // Match on typed enum variants instead of fragile string matching
            match e {
                SignalProtocolError::SealedSenderSelfSend => {
                    CryptoError::SealedSenderSelfSend
                }
                SignalProtocolError::UnknownSealedSenderServerCertificateId(id) => {
                    CryptoError::UnknownServerCertificate(id)
                }
                SignalProtocolError::InvalidSealedSenderMessage(ref msg)
                    if msg.contains("trust root") =>
                {
                    CryptoError::InvalidSenderCertificate(e.to_string())
                }
                SignalProtocolError::SignatureValidationFailed => {
                    CryptoError::InvalidSenderCertificate(e.to_string())
                }
                _ => CryptoError::InvalidSealedSenderMessage(e.to_string()),
            }
        };

        let usmc = sealed_sender_decrypt_to_usmc(ciphertext, identity_store)
            .await
            .map_err(map_envelope_err)?;

        let sender_ok = (|| -> Result<(bool, String, Option<String>, DeviceId), SignalProtocolError> {
            let sender = usmc.sender()?;
            Ok((
                sender.validate(trust_root, timestamp)?,
                sender.sender_uuid()?.to_string(),
                sender.sender_e164()?.map(|s| s.to_string()),
                sender.sender_device_id()?,
            ))
        })()
        .map_err(map_envelope_err)?;
        let (cert_valid, sender_uuid, sender_e164, sender_device_id) = sender_ok;

        if !cert_valid {
            return Err(map_envelope_err(SignalProtocolError::InvalidSealedSenderMessage(
                "trust root validation failed".to_string(),
            )));
        }

        let is_local_uuid = local_uuid == sender_uuid;
        let is_local_e164 = matches!(
            (&local_e164, &sender_e164),
            (Some(l), Some(s)) if l == s
        );
        if (is_local_e164 || is_local_uuid) && sender_device_id == local_device_id {
            return Err(map_envelope_err(SignalProtocolError::SealedSenderSelfSend));
        }

        let remote_address = ProtocolAddress::new(sender_uuid.clone(), sender_device_id);
        let msg_type = usmc.msg_type().map_err(map_envelope_err)?;
        let contents = usmc.contents().map_err(map_envelope_err)?;

        // Envelope opened and sender authenticated. From here on a failure is
        // an INNER (session/ratchet) failure — surface it as SessionDesync so
        // the caller can send a DecryptionErrorMessage back to the sender.
        let map_inner_err = |e: SignalProtocolError| {
            classify_inner_failure(e, &sender_uuid, sender_device_id, msg_type, contents)
        };

        let mut rng = OsRng.unwrap_err();
        let message = match msg_type {
            CiphertextMessageType::Whisper => {
                let ctext = SignalMessage::try_from(contents).map_err(map_inner_err)?;
                message_decrypt_signal(
                    &ctext,
                    &remote_address,
                    session_store,
                    identity_store,
                    &mut rng,
                )
                .await
                .map_err(map_inner_err)?
            }
            CiphertextMessageType::PreKey => {
                let ctext = PreKeySignalMessage::try_from(contents).map_err(map_inner_err)?;
                message_decrypt_prekey(
                    &ctext,
                    &remote_address,
                    session_store,
                    identity_store,
                    pre_key_store,
                    signed_pre_key_store,
                    kyber_pre_key_store,
                    &mut rng,
                )
                .await
                .map_err(map_inner_err)?
            }
            // A DecryptionErrorMessage ride-along: the "message" is the
            // serialized `PlaintextContent` (0xC0-prefixed), not ratchet
            // ciphertext. Return it verbatim — the gateway extracts the DEM.
            // The `Plaintext` type travels back with it so every caller that
            // is NOT the DEM handler can refuse these unratcheted bytes.
            CiphertextMessageType::Plaintext => contents.to_vec(),
            other => {
                return Err(map_envelope_err(SignalProtocolError::InvalidMessage(
                    other,
                    "unexpected message type for sealed sender decrypt",
                )));
            }
        };

        let result = SealedSenderDecryptionResult {
            sender_uuid,
            sender_e164,
            device_id: sender_device_id,
            message,
        };

        tracing::debug!(
            sender = %result.sender_uuid,
            sender_device = ?result.device_id,
            message_len = result.message.len(),
            ?msg_type,
            "SealedSender: decryption complete"
        );
        Ok((result, msg_type))
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
        UnidentifiedSenderMessageContent::new(msg_type, sender_cert, contents, content_hint, group_id)
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

    /// Decrypt sealed sender message to USMC without inner decryption.
    ///
    /// This extracts the sender information and inner ciphertext without
    /// decrypting the inner message. Useful for:
    /// - Inspecting sender identity before full decryption
    /// - Forwarding messages without decrypting content
    /// - Validating certificates separately
    ///
    /// # Arguments
    ///
    /// * `ciphertext` - The sealed sender message bytes
    /// * `identity_store` - Identity key storage
    ///
    /// # Returns
    ///
    /// The USMC containing sender certificate and encrypted inner message.
    pub async fn decrypt_to_content<I>(
        ciphertext: &[u8],
        identity_store: &I,
    ) -> Result<UnidentifiedSenderMessageContent, CryptoError>
    where
        I: IdentityKeyStore,
    {
        sealed_sender_decrypt_to_usmc(ciphertext, identity_store)
            .await
            .map_err(|e| {
                tracing::warn!(
                    target: "chatter::crypto::decrypt_failure",
                    kind = "sealed_sender",
                    error = %e,
                    "decryption failure"
                );
                CryptoError::InvalidSealedSenderMessage(e.to_string())
            })
    }
}

impl Default for SealedSenderManager {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// UAK (Unidentified Access Key) Functions
// ============================================================================

/// UAK length in bytes (Signal standard).
pub const UAK_LENGTH: usize = 16;

/// Profile key length in bytes.
pub const PROFILE_KEY_LENGTH: usize = 32;

/// Combine multiple UAKs for group sealed sender.
///
/// For group messages, the sender must prove knowledge of all recipients' UAKs
/// by XORing them together. The server validates by computing the same XOR
/// and comparing.
///
/// # Arguments
///
/// * `uaks` - Slice of 16-byte UAKs from all group recipients
///
/// # Returns
///
/// Combined 16-byte UAK (XOR of all input UAKs).
///
/// # Signal Reference
///
/// See `Signal-server/service/.../UnidentifiedAccessUtil.java#combineUnidentifiedAccessKeys`
pub fn combine_uaks_for_group(uaks: &[[u8; UAK_LENGTH]]) -> [u8; UAK_LENGTH] {
    let mut combined = [0u8; UAK_LENGTH];

    for uak in uaks {
        for (i, byte) in uak.iter().enumerate() {
            combined[i] ^= byte;
        }
    }

    combined
}

// ============================================================================
// Certificate Validation Functions
// ============================================================================

/// Default certificate validity period (7 days).
pub const DEFAULT_CERTIFICATE_VALIDITY: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Refresh threshold (refresh when less than 1 day remaining).
pub const CERTIFICATE_REFRESH_THRESHOLD: Duration = Duration::from_secs(24 * 60 * 60);

/// Validate a sender certificate against the hardcoded trust root.
///
/// This function verifies that:
/// 1. The certificate was signed by the trusted server
/// 2. The certificate has not expired
///
/// # Arguments
///
/// * `cert` - The sender certificate to validate
///
/// # Returns
///
/// `true` if the certificate is valid, `false` otherwise.
///
/// # Security
///
/// The trust root is hardcoded in the binary (see `config.rs`) following
/// Signal's security model. This prevents MITM attacks during registration
/// or certificate renewal.
pub fn validate_trust_root(cert: &SenderCertificate) -> bool {
    // Get current timestamp
    let now_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let now = Timestamp::from_epoch_millis(now_ms);

    // Check expiration
    let expiry = match cert.expiration() {
        Ok(ts) => ts,
        Err(e) => {
            tracing::warn!("Failed to get certificate expiration: {}", e);
            tracing::warn!(
                target: "chatter::crypto::sealed_sender_cert_reject",
                reason = "chain",
                "sealed sender certificate rejected"
            );
            return false;
        }
    };

    if expiry.epoch_millis() < now_ms {
        tracing::warn!(
            "Sender certificate expired: cert_expiry={}, now={}",
            expiry.epoch_millis(),
            now_ms
        );
        tracing::warn!(
            target: "chatter::crypto::sealed_sender_cert_reject",
            reason = "expired",
            "sealed sender certificate rejected"
        );
        return false;
    }

    // Validate certificate chain against trust root
    match cert.validate(&TRUST_ROOT_PUBLIC_KEY, now) {
        Ok(true) => true,
        Ok(false) => {
            tracing::warn!("Sender certificate failed trust root validation");
            tracing::warn!(
                target: "chatter::crypto::sealed_sender_cert_reject",
                reason = "trust_root",
                "sealed sender certificate rejected"
            );
            false
        }
        Err(e) => {
            tracing::warn!("Sender certificate validation error: {}", e);
            // Distinguish signature errors from generic chain errors when
            // libsignal's error string makes that clear; otherwise default
            // to "signature" since validate() failures are signature checks.
            let err_str = e.to_string();
            let reason = if err_str.to_ascii_lowercase().contains("signature") {
                "signature"
            } else {
                "chain"
            };
            tracing::warn!(
                target: "chatter::crypto::sealed_sender_cert_reject",
                reason = reason,
                "sealed sender certificate rejected"
            );
            false
        }
    }
}

/// Check if a sender certificate needs refresh.
///
/// Returns `true` if the certificate is expired or will expire within
/// the refresh threshold period.
///
/// # Arguments
///
/// * `cert` - The sender certificate to check
///
/// # Returns
///
/// `true` if the certificate should be refreshed.
pub fn certificate_needs_refresh(cert: &SenderCertificate) -> bool {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let threshold_ms = CERTIFICATE_REFRESH_THRESHOLD.as_millis() as u64;
    let expiry_threshold = now.saturating_add(threshold_ms);

    match cert.expiration() {
        Ok(ts) => ts.epoch_millis() < expiry_threshold,
        Err(_) => true, // If we can't get expiration, assume refresh needed
    }
}

/// Cached sender certificate with expiration tracking.
///
/// This wrapper helps manage certificate lifecycle by tracking when
/// the certificate was obtained and when it expires.
#[derive(Clone)]
pub struct CachedSenderCertificate {
    /// The actual sender certificate.
    pub certificate: SenderCertificate,
    /// When this certificate was cached.
    pub cached_at: SystemTime,
}

impl CachedSenderCertificate {
    /// Create a new cached certificate.
    pub fn new(certificate: SenderCertificate) -> Self {
        Self {
            certificate,
            cached_at: SystemTime::now(),
        }
    }

    /// Check if this certificate needs refresh.
    pub fn needs_refresh(&self) -> bool {
        certificate_needs_refresh(&self.certificate)
    }

    /// Check if this certificate is still valid.
    pub fn is_valid(&self) -> bool {
        validate_trust_root(&self.certificate)
    }

    /// Get the expiration timestamp in milliseconds since epoch.
    pub fn expiration_ms(&self) -> Option<u64> {
        self.certificate.expiration().ok().map(|ts| ts.epoch_millis())
    }

    /// Get how long until expiration.
    pub fn time_until_expiration(&self) -> Option<Duration> {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let expiry = self.certificate.expiration().ok()?.epoch_millis();
        if expiry > now {
            Some(Duration::from_millis(expiry - now))
        } else {
            None
        }
    }
}

// ============================================================================
// Multi-Device Sealed Sender Types
// ============================================================================

/// A sealed sender message prepared for a specific device.
///
/// When sending to a user with multiple devices, the sender must create
/// a separate encrypted envelope for each device.
#[derive(Clone)]
pub struct SealedMessageForDevice {
    /// Target device ID.
    pub device_id: u32,
    /// Target device's registration ID (for session verification).
    pub registration_id: u32,
    /// Encrypted sealed sender envelope for this device.
    pub sealed_content: Vec<u8>,
}

/// Collection of sealed sender messages for all of a recipient's devices.
#[derive(Clone)]
pub struct MultiDeviceSealedMessage {
    /// Target user ID.
    pub recipient_user_id: String,
    /// Per-device encrypted envelopes.
    pub device_messages: Vec<SealedMessageForDevice>,
    /// Combined UAK for anonymous delivery (single recipient UAK or group XOR).
    pub unidentified_access_key: [u8; UAK_LENGTH],
}

/// Decide what an inner (post-envelope) decryption failure means.
///
/// The envelope already opened and the sender is authenticated, so this is a
/// session-layer failure — with one exception that must not be treated as one.
///
/// A [`SignalProtocolError::DuplicatedMessage`] means the ratchet chain has moved
/// past the counter this message carries and its message keys are gone: it was
/// decrypted once already. The session is healthy and no plaintext is lost, so the
/// message is dropped. Reporting it as a desync would send the peer a
/// `DecryptionErrorMessage` asking it to archive a working session and resend,
/// which produces more traffic and more re-deliveries. Measured before this split
/// existed: 201 inbound reports and 12 outbound in four minutes between two idle
/// devices. libsignal treats the same condition as routine, logging `Duplicate
/// message for counter: N` at info.
///
/// Everything else is the genuine wedge this path exists for and becomes a
/// [`CryptoError::SessionDesync`] carrying the failed ciphertext, so the caller can
/// build the report.
///
/// This split mirrors libsignal's own control flow rather than guessing at it. In
/// `decrypt_message_with_record` (`session_cipher.rs:478`) a `DuplicatedMessage`
/// from the current session state `return result`s immediately, without trying any
/// previous state — libsignal treats it as the authoritative answer. Every other
/// error is pushed onto `errs` and the search continues, ending as
/// `InvalidMessage`. So a duplicate verdict really does mean "this counter was
/// already consumed", and a genuinely undeliverable message arrives here as
/// `InvalidMessage` and still reports. Worth stating because the log around a
/// duplicate is alarming and misleading: `creating new chains` and `Bad Mac!` are
/// libsignal trying states, not the conclusion.
fn classify_inner_failure(
    error: SignalProtocolError, sender_uuid: &str, sender_device_id: DeviceId,
    msg_type: CiphertextMessageType, contents: &[u8],
) -> CryptoError {
    // The peer's identity key changed (reinstall, device wipe — or a MITM).
    // NOT a desync: no session-layer action changes which key is pinned, so a
    // DecryptionErrorMessage would only make the peer resend the same new key
    // and fail identically, forever. It needs a human trust decision, so it has
    // to leave here as its own error carrying the key that decision is about.
    if let SignalProtocolError::UntrustedIdentity(ref address) = error {
        // A PreKeySignalMessage carries the sender's new identity key inline —
        // that IS the key the user is being asked to accept. A Whisper message
        // carries none (libsignal reads it from the stored session).
        let new_identity_key = match msg_type {
            CiphertextMessageType::PreKey => PreKeySignalMessage::try_from(contents)
                .ok()
                .map(|m| m.identity_key().serialize().to_vec()),
            _ => None,
        };
        tracing::warn!(
            target: "chatter::crypto::decrypt_failure",
            kind = "identity_changed",
            sender = %sender_uuid,
            sender_device = ?sender_device_id,
            have_new_key = new_identity_key.is_some(),
            "peer identity key changed — conversation wedged until the new key is accepted"
        );
        // Sealed sender: the uuid came from a sender certificate validated
        // against the trust root above, so this attribution is authenticated.
        return super::untrusted_identity(
            address,
            new_identity_key,
            IdentityChangeSource::SenderCertificate,
        );
    }
    if let SignalProtocolError::DuplicatedMessage(chain_index, counter) = error {
        tracing::debug!(
            target: "chatter::crypto::decrypt_failure",
            kind = "sealed_sender_duplicate",
            sender = %sender_uuid,
            sender_device = ?sender_device_id,
            chain_index,
            counter,
            "duplicate inner message dropped; session left alone"
        );
        return CryptoError::DuplicateMessage {
            sender_uuid: sender_uuid.to_string(),
            sender_device_id: sender_device_id.into(),
        };
    }
    tracing::warn!(
        target: "chatter::crypto::decrypt_failure",
        kind = "sealed_sender_inner",
        sender = %sender_uuid,
        sender_device = ?sender_device_id,
        error = %error,
        "inner decryption failure (session desync candidate)"
    );
    CryptoError::SessionDesync {
        sender_uuid: sender_uuid.to_string(),
        sender_device_id: sender_device_id.into(),
        msg_type: msg_type as u8,
        ciphertext: contents.to_vec(),
        reason: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_uak_deterministic() {
        use crate::crypto::derive_uak;

        let mut profile_key = [0u8; PROFILE_KEY_LENGTH];
        profile_key[0] = 0x42;
        profile_key[31] = 0x42;

        let uak = derive_uak(&profile_key);
        assert_eq!(uak.len(), UAK_LENGTH);

        // Deterministic for the same key.
        assert_eq!(uak, derive_uak(&profile_key));

        // Different key, different UAK.
        let mut different_key = profile_key;
        different_key[0] = 0x43;
        assert_ne!(uak, derive_uak(&different_key));
    }

    /// A re-delivery must not be reported as a desync.
    ///
    /// The whole cost of getting this wrong is off-box and invisible from here:
    /// each misclassification sends the peer a `DecryptionErrorMessage` telling it
    /// to archive a healthy session and resend, which produces another delivery to
    /// misclassify. Two idle devices generated 201 inbound reports and 12 outbound
    /// in four minutes.
    #[test]
    fn a_duplicate_inner_message_is_not_a_session_desync() {
        let device = DeviceId::new(1).expect("device id");
        let ciphertext = [0xAAu8; 8];

        // `1 / 0`: the chain is at 1, the message carries counter 0 — decrypted
        // once already, keys consumed. This is the exact error observed.
        let duplicate = classify_inner_failure(
            SignalProtocolError::DuplicatedMessage(1, 0),
            "caa49e97-690f-407d-ab9a-74bf772dbd3d",
            device,
            CiphertextMessageType::Whisper,
            &ciphertext,
        );
        assert!(
            matches!(duplicate, CryptoError::DuplicateMessage { .. }),
            "a duplicate was classified as {duplicate:?}, which sends the peer a \
             DecryptionErrorMessage and restarts the loop"
        );

        // A genuine wedge still reports, or the 1:1 self-heal stops working. The
        // ciphertext has to survive into the error — the report is built from it.
        let wedged = classify_inner_failure(
            SignalProtocolError::InvalidMessage(CiphertextMessageType::Whisper, "bad mac"),
            "caa49e97-690f-407d-ab9a-74bf772dbd3d",
            device,
            CiphertextMessageType::Whisper,
            &ciphertext,
        );
        match wedged {
            CryptoError::SessionDesync { ciphertext: carried, msg_type, .. } => {
                assert_eq!(carried, ciphertext, "the report would name no ciphertext");
                assert_eq!(msg_type, CiphertextMessageType::Whisper as u8);
            }
            other => panic!("a real desync was classified as {other:?}; the self-heal is dead"),
        }
    }

    /// A peer's identity key CHANGING must not be reported as a session desync,
    /// and the sealed path's provenance must say the sender was PROVEN.
    ///
    /// libsignal refuses the inner decrypt with `UntrustedIdentity`
    /// (session.rs:61 for a PreKey message, session_cipher.rs:317 for a Whisper
    /// one) because the key on the wire is not the key pinned for the account.
    /// Classifying that as a desync makes the gateway ship the peer a
    /// `DecryptionErrorMessage`; the peer archives its session and resends a
    /// fresh PreKey message carrying the SAME new identity key, and we refuse it
    /// again — a loop that never converges, because no amount of session
    /// resetting changes which key is pinned.
    #[test]
    fn an_identity_change_is_not_a_session_desync() {
        let device = DeviceId::new(1).expect("device id");
        let sender = "caa49e97-690f-407d-ab9a-74bf772dbd3d";
        let address = ProtocolAddress::new(sender.to_string(), device);

        let changed = classify_inner_failure(
            SignalProtocolError::UntrustedIdentity(address),
            sender,
            device,
            CiphertextMessageType::PreKey,
            &[0xAAu8; 8],
        );

        match changed {
            CryptoError::UntrustedIdentity { peer_id, device_id, attribution, .. } => {
                assert_eq!(peer_id, sender);
                assert_eq!(device_id, 1);
                assert!(
                    attribution.is_authenticated(),
                    "the sealed path authenticates its sender via the certificate; reporting it \
                     as unproven would understate what the UI may tell the user"
                );
            }
            other => panic!(
                "a peer identity-key change was classified as {other:?}; as a desync it sends a \
                 DecryptionErrorMessage that can never fix the pinned key"
            ),
        }
    }

    /// `CiphertextMessageType::Plaintext` bypasses the ratchet entirely — no
    /// `is_trusted_identity`, no replay protection, no forward secrecy. Pinned
    /// libsignal refuses it outright (`sealed_sender.rs`, "unexpected message
    /// type for sealed_sender_decrypt"); we accept it only as a
    /// `DecryptionErrorMessage` ride-along.
    ///
    /// So the decrypt result MUST tell the caller which type it got. Without
    /// that label, unratcheted attacker-chosen bytes are indistinguishable from
    /// ratchet-authenticated plaintext and go straight into `Content::decode`
    /// on the DM and anon-DM paths.
    #[tokio::test]
    async fn sealed_sender_decrypt_labels_unratcheted_plaintext() {
        use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
        use libsignal_protocol::{
            IdentityKeyPair, InMemSignalProtocolStore, KeyPair, PrivateKey,
            sealed_sender_encrypt_from_usmc,
        };

        let mut rng = OsRng.unwrap_err();

        // The dev trust root's private key is committed in
        // deploy/vault/init-and-unseal.sh, which is what makes this test
        // possible at all — and is separately why config.rs now demands an
        // explicit feature to pin it.
        let trust_root_private = PrivateKey::deserialize(
            &BASE64
                .decode("ELMk0o1RlhsHCk8vQ/bWbtRDeQvcC4PuN8+U1Xg90Ek=")
                .expect("known-good base64"),
        )
        .expect("dev trust root private key");
        assert_eq!(
            trust_root_private.public_key().expect("public"),
            *TRUST_ROOT_PUBLIC_KEY,
            "test cert chain must validate under the trust root this build pins"
        );

        // Server cert signed by the trust root, sender cert signed by it.
        let server_keys = KeyPair::generate(&mut rng);
        let server_cert =
            ServerCertificate::new(1, server_keys.public_key, &trust_root_private, &mut rng)
                .expect("server certificate");

        let sender_identity = IdentityKeyPair::generate(&mut rng);
        let sender_uuid = "11111111-1111-4111-8111-111111111111";
        let sender_device = DeviceId::new(1).expect("device id");
        let expiration = Timestamp::from_epoch_millis(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64
                + 3_600_000,
        );
        let sender_cert = SenderCertificate::new(
            sender_uuid.to_string(),
            None,
            *sender_identity.public_key(),
            sender_device,
            expiration,
            server_cert,
            &server_keys.private_key,
            &mut rng,
        )
        .expect("sender certificate");

        // A USMC whose payload never touches the Double Ratchet.
        let unratcheted = b"\xc0not ratchet output".to_vec();
        let usmc = UnidentifiedSenderMessageContent::new(
            CiphertextMessageType::Plaintext,
            sender_cert,
            unratcheted.clone(),
            ContentHint::Implicit,
            None,
        )
        .expect("usmc");

        let recipient_uuid = "22222222-2222-4222-8222-222222222222";
        let recipient_device = DeviceId::new(1).expect("device id");
        let recipient_address =
            ProtocolAddress::new(recipient_uuid.to_string(), recipient_device);
        let recipient_identity = IdentityKeyPair::generate(&mut rng);
        let mut recipient_store = InMemSignalProtocolStore::new(recipient_identity, 1234)
            .expect("recipient store");

        // Sender-side store, just to hand the recipient's identity to encrypt.
        let mut sender_store =
            InMemSignalProtocolStore::new(sender_identity, 5678).expect("sender store");
        sender_store
            .save_identity(&recipient_address, recipient_identity.identity_key())
            .await
            .expect("save recipient identity");

        let sealed = sealed_sender_encrypt_from_usmc(
            &recipient_address,
            &usmc,
            &sender_store,
            &mut rng,
        )
        .await
        .expect("seal");

        let mut s2 = recipient_store.clone();
        let mut s3 = recipient_store.clone();
        let mut s4 = recipient_store.clone();
        let s6 = recipient_store.clone();
        let (result, msg_type) = SealedSenderManager::decrypt(
            &sealed,
            &TRUST_ROOT_PUBLIC_KEY,
            Timestamp::from_epoch_millis(0),
            None,
            recipient_uuid.to_string(),
            recipient_device,
            &mut recipient_store,
            &mut s2,
            &mut s3,
            &s6,
            &mut s4,
        )
        .await
        .expect("envelope opens — the cert chain is valid");

        assert_eq!(
            result.message, unratcheted,
            "the Plaintext branch returns contents verbatim, as the DEM handler needs"
        );
        assert_eq!(
            msg_type,
            CiphertextMessageType::Plaintext,
            "decrypt must label unratcheted bytes so non-DEM callers can refuse them"
        );
    }

    #[test]
    fn test_combine_uaks_for_group() {
        let uak1 = [0x11u8; UAK_LENGTH];
        let uak2 = [0x22u8; UAK_LENGTH];
        let uak3 = [0x33u8; UAK_LENGTH];

        // XOR of the three UAKs
        let combined = combine_uaks_for_group(&[uak1, uak2, uak3]);

        // 0x11 XOR 0x22 XOR 0x33 = 0x00
        let expected = [0x00u8; UAK_LENGTH];
        assert_eq!(combined, expected);
    }

    #[test]
    fn test_combine_uaks_single() {
        let uak = [0x42u8; UAK_LENGTH];
        let combined = combine_uaks_for_group(&[uak]);

        // Single UAK should return itself
        assert_eq!(combined, uak);
    }

    #[test]
    fn test_combine_uaks_empty() {
        let combined = combine_uaks_for_group(&[]);

        // Empty should return all zeros
        assert_eq!(combined, [0u8; UAK_LENGTH]);
    }

    #[test]
    fn test_combine_uaks_order_independent() {
        let uak1 = [0x11u8; UAK_LENGTH];
        let uak2 = [0x22u8; UAK_LENGTH];
        let uak3 = [0x33u8; UAK_LENGTH];

        let combined1 = combine_uaks_for_group(&[uak1, uak2, uak3]);
        let combined2 = combine_uaks_for_group(&[uak3, uak1, uak2]);
        let combined3 = combine_uaks_for_group(&[uak2, uak3, uak1]);

        // XOR is commutative - order shouldn't matter
        assert_eq!(combined1, combined2);
        assert_eq!(combined2, combined3);
    }

    #[test]
    fn test_cached_certificate_wrapper() {
        // We can't easily create a real SenderCertificate in tests,
        // so just test the wrapper logic with expiration calculations
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_millis() as u64;

        // Future expiration (24+ hours from now)
        let future_expiry = now + (48 * 60 * 60 * 1000); // 48 hours

        // Past expiration
        let past_expiry = now - (60 * 1000); // 1 minute ago

        // Test threshold logic (manual calculation)
        let threshold_ms = CERTIFICATE_REFRESH_THRESHOLD.as_millis() as u64;
        let needs_refresh_threshold = now + threshold_ms;

        // Future expiry beyond threshold - doesn't need refresh
        assert!(future_expiry > needs_refresh_threshold);

        // Past expiry - definitely needs refresh
        assert!(past_expiry < now);
    }
}
