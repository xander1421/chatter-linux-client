use libsignal_protocol::{
    CiphertextMessageType, DeviceId, IdentityKeyStore, KyberPreKeyStore, PreKeySignalMessage,
    PreKeyStore, ProtocolAddress, PublicKey, SessionStore, SignalMessage, SignalProtocolError,
    SignedPreKeyStore, Timestamp, message_decrypt_prekey, message_decrypt_signal,
    sealed_sender_decrypt_to_usmc,
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

use crate::error::{CryptoError, IdentityChangeSource};

use super::{SealedSenderDecryptionResult, SealedSenderManager};

impl SealedSenderManager {
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
                SignalProtocolError::SealedSenderSelfSend => CryptoError::SealedSenderSelfSend,
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

        let sender_ok =
            (|| -> Result<(bool, String, Option<String>, DeviceId), SignalProtocolError> {
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
            return Err(map_envelope_err(
                SignalProtocolError::InvalidSealedSenderMessage(
                    "trust root validation failed".to_string(),
                ),
            ));
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
        // libsignal >= 0.99 takes the LOCAL address too: sessions are keyed by
        // both ends now, so the decrypt side has to say which identity of ours
        // the message was sent to. These are the same values the self-send check
        // above uses.
        let local_address = ProtocolAddress::new(local_uuid.clone(), local_device_id);
        let msg_type = usmc.msg_type().map_err(map_envelope_err)?;
        let contents = usmc.contents().map_err(map_envelope_err)?;

        // Envelope opened and sender authenticated. From here on a failure is
        // an INNER (session/ratchet) failure — surface it as SessionDesync so
        // the caller can send a DecryptionErrorMessage back to the sender.
        let group_id = usmc.group_id().ok().flatten().map(<[u8]>::to_vec);
        let map_inner_err = |e: SignalProtocolError| {
            classify_inner_failure(
                e,
                &sender_uuid,
                sender_device_id,
                msg_type,
                contents,
                group_id.clone(),
            )
        };

        let mut rng = OsRng.unwrap_err();
        let message = match msg_type {
            CiphertextMessageType::Whisper => {
                let ctext = SignalMessage::try_from(contents).map_err(map_inner_err)?;
                message_decrypt_signal(
                    &ctext,
                    &remote_address,
                    &local_address,
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
                    &local_address,
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
                    // libsignal >= 0.99 takes an owned String here.
                    "unexpected message type for sealed sender decrypt".to_string(),
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
pub(super) fn classify_inner_failure(
    error: SignalProtocolError,
    sender_uuid: &str,
    sender_device_id: DeviceId,
    msg_type: CiphertextMessageType,
    contents: &[u8],
    group_id: Option<Vec<u8>>,
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
        return super::super::untrusted_identity(
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
        group_id,
    }
}
