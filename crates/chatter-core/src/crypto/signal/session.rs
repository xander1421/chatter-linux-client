//! Session management.
//!
//! Signal Protocol session creation and management for encrypted messaging.

use libsignal_protocol::{
    Fingerprint, IdentityKey, IdentityKeyStore, PreKeyBundle, ProtocolAddress,
    SessionRecord, SessionStore, process_prekey_bundle,
};
use rand::{CryptoRng, Rng};
use std::time::SystemTime;

use crate::error::CryptoError;

/// Session manager for Signal Protocol.
///
/// Provides functions for session establishment, lookup, and fingerprint generation
/// for end-to-end encrypted communication.
///
/// Note: This struct is stateless - all session operations are performed on
/// the stores passed to each method. This design avoids mutable aliasing issues
/// with libsignal's trait-based API.
#[derive(Clone)]
pub struct SessionManager;

impl SessionManager {
    /// Establish a new session with a remote user using their prekey bundle.
    ///
    /// This performs the PQXDH (post-quantum extended Diffie-Hellman) key exchange
    /// to create a shared session for encrypted messaging.
    ///
    /// Note: libsignal no longer supports X3DH - only PQXDH. The bundle must
    /// include Kyber prekeys.
    ///
    /// # Arguments
    /// * `remote_address` - The address of the remote user
    /// * `session_store` - Mutable reference to session storage
    /// * `identity_store` - Mutable reference to identity storage
    /// * `bundle` - The remote user's prekey bundle
    /// * `rng` - Cryptographically secure random number generator
    pub async fn establish_session<R, S, I>(
        remote_address: &ProtocolAddress,
        session_store: &mut S,
        identity_store: &mut I,
        bundle: &PreKeyBundle,
        rng: &mut R,
    ) -> Result<(), CryptoError>
    where
        R: CryptoRng + Rng,
        S: SessionStore,
        I: IdentityKeyStore,
    {
        tracing::debug!(
            address = %remote_address,
            "SessionManager: establishing session with prekey bundle"
        );

        process_prekey_bundle(
            remote_address,
            session_store,
            identity_store,
            bundle,
            SystemTime::now(),
            rng,
        )
        .await
        .map_err(|e| {
            tracing::error!(
                address = %remote_address,
                error = %e,
                "SessionManager: failed to process prekey bundle"
            );
            CryptoError::Protocol(e.to_string())
        })?;

        tracing::debug!(
            address = %remote_address,
            "SessionManager: session established successfully"
        );
        Ok(())
    }

    /// Check if a session exists with the given remote address.
    pub async fn has_session<S: SessionStore>(
        store: &S,
        remote_address: &ProtocolAddress,
    ) -> Result<bool, CryptoError> {
        tracing::debug!(
            address = %remote_address,
            "SessionManager: checking if session exists"
        );

        let session = store
            .load_session(remote_address)
            .await
            .map_err(|e| {
                tracing::error!(
                    address = %remote_address,
                    error = %e,
                    "SessionManager: failed to load session"
                );
                CryptoError::Protocol(e.to_string())
            })?;

        let exists = session.is_some();
        tracing::debug!(
            address = %remote_address,
            exists,
            "SessionManager: session check complete"
        );
        Ok(exists)
    }

    /// Check if a session exists AND is usable (can be serialized/deserialized).
    ///
    /// This goes beyond `has_session()` by validating that the session record
    /// is structurally intact. A session that exists but fails to serialize
    /// is considered unusable.
    pub async fn is_session_usable<S: SessionStore>(
        store: &S,
        remote_address: &ProtocolAddress,
    ) -> Result<bool, CryptoError> {
        let session = store
            .load_session(remote_address)
            .await
            .map_err(|e| CryptoError::Protocol(e.to_string()))?;

        match session {
            Some(record) => {
                // Validate: can the session be round-tripped?
                match record.serialize() {
                    Ok(bytes) => {
                        match SessionRecord::deserialize(&bytes) {
                            Ok(_) => Ok(true),
                            Err(e) => {
                                tracing::warn!(
                                    address = %remote_address,
                                    error = %e,
                                    "SessionManager: session exists but is corrupted (deserialize failed)"
                                );
                                Ok(false)
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            address = %remote_address,
                            error = %e,
                            "SessionManager: session exists but is corrupted (serialize failed)"
                        );
                        Ok(false)
                    }
                }
            }
            None => Ok(false),
        }
    }

    /// Get the session record for a remote address.
    ///
    /// Returns `None` if no session exists.
    pub async fn get_session<S: SessionStore>(
        store: &S,
        remote_address: &ProtocolAddress,
    ) -> Result<Option<SessionRecord>, CryptoError> {
        store
            .load_session(remote_address)
            .await
            .map_err(|e| CryptoError::Protocol(e.to_string()))
    }

    /// Generate a safety number fingerprint for identity verification.
    ///
    /// The fingerprint allows users to verify they are communicating with
    /// the intended person by comparing a 60-digit number or scanning QR codes.
    ///
    /// Uses Signal's fingerprint format: version 2, 5200 iterations.
    pub fn generate_fingerprint(
        local_id: &str,
        local_key: &IdentityKey,
        remote_id: &str,
        remote_key: &IdentityKey,
    ) -> Result<String, CryptoError> {
        let fingerprint = Fingerprint::new(
            2, // Version 2 fingerprints
            5200, // Iterations (Signal spec)
            local_id.as_bytes(),
            local_key,
            remote_id.as_bytes(),
            remote_key,
        )
        .map_err(|e| CryptoError::FingerprintFailed(e.to_string()))?;

        fingerprint
            .display_string()
            .map_err(|e| CryptoError::FingerprintFailed(e.to_string()))
    }

    /// Get the fingerprint as scannable data for QR code generation.
    ///
    /// Returns the raw bytes suitable for encoding into a QR code.
    pub fn generate_scannable_fingerprint(
        local_id: &str,
        local_key: &IdentityKey,
        remote_id: &str,
        remote_key: &IdentityKey,
    ) -> Result<Vec<u8>, CryptoError> {
        let fingerprint = Fingerprint::new(
            2,
            5200,
            local_id.as_bytes(),
            local_key,
            remote_id.as_bytes(),
            remote_key,
        )
        .map_err(|e| CryptoError::FingerprintFailed(e.to_string()))?;

        fingerprint
            .scannable
            .serialize()
            .map_err(|e| CryptoError::FingerprintFailed(e.to_string()))
    }
}
