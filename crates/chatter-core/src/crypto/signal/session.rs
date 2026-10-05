//! Session management.
//!
//! Signal Protocol session creation and management for encrypted messaging.

use libsignal_protocol::{
    Fingerprint, IdentityKey, IdentityKeyStore, PreKeyBundle, ProtocolAddress, SessionRecord,
    SessionStore, SignalProtocolError, process_prekey_bundle,
};
use rand::{CryptoRng, Rng};
use std::time::SystemTime;

use crate::error::{CryptoError, IdentityChangeSource};

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
        // libsignal >= 0.99 keys a session by both ends, so processing a bundle
        // needs our own address as well as theirs.
        local_address: &ProtocolAddress,
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

        // Defense-in-depth: locally verify XEdDSA signatures over the signed
        // prekey and Kyber prekey BEFORE handing the bundle to libsignal.
        //
        // libsignal's `process_prekey_bundle` performs the same verification
        // internally (rust/protocol/src/session.rs ~L191), but doing it here
        // first guarantees that any future libsignal refactor (or misuse of
        // an internal helper) cannot silently bypass the signature check on
        // our hot path.
        let identity_key = bundle.identity_key().map_err(|e| {
            tracing::error!(error = %e, "SessionManager: bundle missing identity key");
            CryptoError::Protocol(e.to_string())
        })?;

        let spk_public = bundle.signed_pre_key_public().map_err(|e| {
            tracing::error!(error = %e, "SessionManager: bundle missing signed prekey");
            CryptoError::Protocol(e.to_string())
        })?;
        let spk_signature = bundle.signed_pre_key_signature().map_err(|e| {
            tracing::error!(error = %e, "SessionManager: bundle missing signed prekey signature");
            CryptoError::Protocol(e.to_string())
        })?;

        if !identity_key
            .public_key()
            .verify_signature(&spk_public.serialize(), spk_signature)
        {
            tracing::error!(
                address = %remote_address,
                "SessionManager: local XEdDSA verify FAILED for signed prekey"
            );
            return Err(CryptoError::InvalidSignature);
        }

        // PQXDH bundles always carry a Kyber prekey; verify it too.
        let kyber_public = bundle.kyber_pre_key_public().map_err(|e| {
            tracing::error!(error = %e, "SessionManager: bundle missing kyber prekey");
            CryptoError::Protocol(e.to_string())
        })?;
        let kyber_signature = bundle.kyber_pre_key_signature().map_err(|e| {
            tracing::error!(error = %e, "SessionManager: bundle missing kyber prekey signature");
            CryptoError::Protocol(e.to_string())
        })?;

        if !identity_key
            .public_key()
            .verify_signature(&kyber_public.serialize(), kyber_signature)
        {
            tracing::error!(
                address = %remote_address,
                "SessionManager: local XEdDSA verify FAILED for kyber prekey"
            );
            return Err(CryptoError::InvalidSignature);
        }

        process_prekey_bundle(
            remote_address,
            local_address,
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
            // A bundle whose identity key is not the pinned one is the outgoing
            // half of the peer-reinstall wedge (libsignal session.rs:186). It
            // must stay distinguishable from a generic protocol failure, and it
            // must carry the bundle's key — that is the key the user is being
            // asked to accept.
            if let SignalProtocolError::UntrustedIdentity(ref address) = e {
                return super::untrusted_identity(
                    address,
                    Some(identity_key.serialize().to_vec()),
                    IdentityChangeSource::PreKeyBundle,
                );
            }
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

        let session = store.load_session(remote_address).await.map_err(|e| {
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
                    Ok(bytes) => match SessionRecord::deserialize(&bytes) {
                        Ok(_) => Ok(true),
                        Err(e) => {
                            tracing::warn!(
                                address = %remote_address,
                                error = %e,
                                "SessionManager: session exists but is corrupted (deserialize failed)"
                            );
                            Ok(false)
                        }
                    },
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
            2,    // Version 2 fingerprints
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

#[cfg(test)]
mod tests {
    use super::*;
    use libsignal_protocol::{DeviceId, IdentityKeyPair, InMemSignalProtocolStore, KeyPair, kem};
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    /// Build a fresh `InMemSignalProtocolStore` for one side of a session.
    /// Alice's own address — the local side of the session under test.
    fn alice_addr() -> ProtocolAddress {
        ProtocolAddress::new("alice".to_string(), DeviceId::new(1).unwrap())
    }

    fn make_store(rng: &mut StdRng) -> InMemSignalProtocolStore {
        let identity = IdentityKeyPair::generate(rng);
        // Valid registration IDs fit in 14 bits.
        let registration_id: u32 = rng.random::<u32>() & 0x3fff;
        InMemSignalProtocolStore::new(identity, registration_id).expect("in-mem store")
    }

    /// Construct a fully-signed PQXDH PreKeyBundle for `bob_store`.
    async fn build_bundle(bob_store: &InMemSignalProtocolStore, rng: &mut StdRng) -> PreKeyBundle {
        let signed_pre_key_pair = KeyPair::generate(rng);
        let kyber_pre_key_pair = kem::KeyPair::generate(kem::KeyType::Kyber1024, rng);

        let identity_kp = bob_store.get_identity_key_pair().await.unwrap();
        let signed_pre_key_public = signed_pre_key_pair.public_key.serialize();
        let signed_pre_key_signature = identity_kp
            .private_key()
            .calculate_signature(&signed_pre_key_public, rng)
            .unwrap();

        let kyber_pre_key_public = kyber_pre_key_pair.public_key.serialize();
        let kyber_pre_key_signature = identity_kp
            .private_key()
            .calculate_signature(&kyber_pre_key_public, rng)
            .unwrap();

        let device_id: DeviceId = DeviceId::new(1).expect("nonzero device id");
        let signed_pre_key_id: u32 = rng.random();
        let kyber_pre_key_id: u32 = rng.random();

        PreKeyBundle::new(
            bob_store.get_local_registration_id().await.unwrap(),
            device_id,
            None, // no one-time prekey for this test
            signed_pre_key_id.into(),
            signed_pre_key_pair.public_key,
            signed_pre_key_signature.to_vec(),
            kyber_pre_key_id.into(),
            kyber_pre_key_pair.public_key.clone(),
            kyber_pre_key_signature.to_vec(),
            *identity_kp.identity_key(),
        )
        .expect("valid bundle")
    }

    #[tokio::test]
    async fn establish_session_succeeds_with_valid_bundle() {
        let mut rng = StdRng::seed_from_u64(0xC0FFEE);
        let mut alice_store = make_store(&mut rng);
        let bob_store = make_store(&mut rng);

        let bundle = build_bundle(&bob_store, &mut rng).await;
        let bob_addr = ProtocolAddress::new("bob".to_string(), DeviceId::new(1).unwrap());

        let mut alice_store2 = alice_store.clone();
        let res = SessionManager::establish_session(
            &bob_addr,
            &alice_addr(),
            &mut alice_store,
            &mut alice_store2,
            &bundle,
            &mut rng,
        )
        .await;

        assert!(res.is_ok(), "valid bundle must establish session: {res:?}");
        let exists = SessionManager::has_session(&alice_store, &bob_addr)
            .await
            .expect("has_session ok");
        assert!(exists, "session should be present after establish");
    }

    #[tokio::test]
    async fn establish_session_rejects_tampered_signed_prekey_signature() {
        let mut rng = StdRng::seed_from_u64(0xDEADBEEF);
        let mut alice_store = make_store(&mut rng);
        let bob_store = make_store(&mut rng);

        let bundle = build_bundle(&bob_store, &mut rng).await;

        // Flip one byte of the signed_pre_key_signature.
        let tampered = bundle
            .modify(|content| {
                let sig = content
                    .signed_pre_key_signature
                    .as_mut()
                    .expect("signature present");
                sig[0] ^= 0x01;
            })
            .expect("modify bundle");

        let bob_addr = ProtocolAddress::new("bob".to_string(), DeviceId::new(1).unwrap());
        let mut alice_store2 = alice_store.clone();
        let res = SessionManager::establish_session(
            &bob_addr,
            &alice_addr(),
            &mut alice_store,
            &mut alice_store2,
            &tampered,
            &mut rng,
        )
        .await;

        match res {
            Err(CryptoError::InvalidSignature) => {
                // Expected: our local defense-in-depth verify caught it,
                // not libsignal's internal verify (which would surface as
                // CryptoError::Protocol).
            }
            other => {
                panic!("expected CryptoError::InvalidSignature from local verify, got {other:?}")
            }
        }

        let exists = SessionManager::has_session(&alice_store, &bob_addr)
            .await
            .expect("has_session ok");
        assert!(!exists, "no session should exist after rejected bundle");
    }
}
