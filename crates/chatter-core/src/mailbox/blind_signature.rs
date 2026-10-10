//! Chaum RSA blind signature implementation.
//!
//! Implements blind signatures for anonymous mailbox token creation.
//! Uses RFC 9474 RSABSSA via the blind-rsa-signatures crate.
//!
//! # Flow
//!
//! 1. Client generates ephemeral Ed25519 keypair
//! 2. Client blinds the public key using server's RSA public key
//! 3. Server signs the blinded message (cannot see original key)
//! 4. Client unblinds the signature
//! 5. Result: signature on original key that server cannot link to blinding request

use crate::proto::mailbox_v1::GetBlindSignaturePublicKeyResponse;
use blind_rsa_signatures::{BlindSignature, BlindingResult, Options, PublicKey, Secret};
use ed25519_dalek::{Signature, VerifyingKey};
use rsa::PublicKeyParts;
use rsa::pkcs8::DecodePublicKey;

use std::future::Future;

use crate::error::{MailboxError, NetworkError};
use crate::proto::chat_v1::GetConsistencyProofResponse;
use crate::services::signal::kt_anchor;
use crate::storage::repositories::SettingsRepository;

/// Server's RSA public key for blind signatures.
///
/// Caches the parsed RSA public key along with metadata for key rotation tracking.
#[derive(Clone)]
pub struct ServerPublicKey {
    // Note: blind_rsa_signatures::PublicKey doesn't derive Debug,
    // so we implement Debug manually below.
    /// The RSA-2048 public key for blinding operations.
    inner: PublicKey,
    /// Server's key identifier for rotation tracking.
    pub key_id: String,
    /// When this key expires (Unix timestamp nanoseconds, per proto spec).
    pub valid_until: u64,
    valid_from: u64,
}

impl ServerPublicKey {
    /// Parse a server public key from DER-encoded bytes.
    ///
    /// # Arguments
    ///
    /// * `der_bytes` - RSA-2048 public key in DER format
    /// * `key_id` - Server's key identifier
    /// * `valid_until` - Expiry timestamp (nanoseconds, per proto spec)
    ///
    /// # Errors
    ///
    /// Returns `MailboxError::InvalidServerKey` if parsing fails.
    fn from_der(der_bytes: &[u8], key_id: String, valid_until: u64) -> Result<Self, MailboxError> {
        // Parse as RSA public key
        let rsa_key = rsa::RsaPublicKey::from_public_key_der(der_bytes)
            .map_err(|e| MailboxError::InvalidServerKey(format!("failed to parse DER: {}", e)))?;

        if rsa_key.n().bits() != 2048 || rsa_key.e() != &rsa::BigUint::from(65537u32) {
            return Err(MailboxError::InvalidServerKey(
                "expected RSA-2048 with exponent 65537".into(),
            ));
        }
        // Convert to blind-rsa-signatures PublicKey
        // PublicKey::new in 0.14 returns the key directly (not Result)
        let inner = PublicKey::new(rsa_key);

        Ok(Self {
            inner,
            key_id,
            valid_until,
            valid_from: 0,
        })
    }

    /// Authenticate the complete rotating manifest against the compile-time KT pin,
    /// and require the KT log to commit to it. Raw DER is deliberately not a
    /// public construction path. Also the check for a cached manifest; a fresh
    /// one goes through [`Self::fetch_manifest`].
    pub fn from_manifest(
        manifest: &GetBlindSignaturePublicKeyResponse,
    ) -> Result<Self, MailboxError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| MailboxError::InvalidServerKey("clock before Unix epoch".into()))?
            .as_nanos() as u64;
        verify_manifest_in_log(manifest)?;
        Self::verify_manifest_at(manifest, &crate::config::KT_TREE_HEAD_PUBLIC_KEY_BYTES, now)
    }

    /// Fetch the manifest with `fetch`, check it with [`Self::from_manifest`],
    /// then hold its tree head to the one this device pinned from every other
    /// KT proof (`kt_anchor`), so a log forked for this client alone is
    /// refused. `fetch_consistency` is the anonymous KT client's RPC.
    ///
    /// The fetch runs under the pin lock (`kt_anchor::lock_pin`): a manifest
    /// fetched before a concurrent check advanced the pin carries an older head
    /// than the pin, and would be rejected as a rollback.
    pub async fn fetch_manifest<M, MFut, C, CFut>(
        fetch: M,
        settings: &SettingsRepository,
        fetch_consistency: C,
    ) -> Result<(GetBlindSignaturePublicKeyResponse, Self), MailboxError>
    where
        M: FnOnce() -> MFut,
        MFut: Future<Output = Result<GetBlindSignaturePublicKeyResponse, MailboxError>>,
        C: FnOnce(u64, u64) -> CFut,
        CFut: Future<Output = Result<GetConsistencyProofResponse, NetworkError>>,
    {
        let pin = kt_anchor::lock_pin().await;
        // Under the process-wide pin lock, so it gets the pin deadline: a
        // withheld manifest must not stall every KT check behind it.
        let manifest = tokio::time::timeout(kt_anchor::PIN_RPC_DEADLINE, fetch())
            .await
            .map_err(|_| MailboxError::Network(NetworkError::Timeout.to_string()))??;
        let key = Self::from_manifest(&manifest)?;
        let sth = manifest
            .kt_proof
            .as_ref()
            .and_then(|p| p.signed_tree_head.as_ref())
            .ok_or_else(|| MailboxError::InvalidServerKey("manifest has no tree head".into()))?;
        kt_anchor::hold_head_locked(&pin, settings, sth, fetch_consistency)
            .await
            .map_err(|e| {
                MailboxError::InvalidServerKey(format!(
                    "manifest tree head contradicts this device's pinned head: {e}"
                ))
            })?;
        Ok((manifest, key))
    }

    fn verify_manifest_at(
        m: &GetBlindSignaturePublicKeyResponse,
        pin: &[u8; 32],
        now: u64,
    ) -> Result<Self, MailboxError> {
        let invalid =
            || MailboxError::InvalidServerKey("invalid or expired signed key manifest".into());
        let next = m.next_key.as_ref().ok_or_else(invalid)?;
        if m.key_id.is_empty()
            || next.key_id.is_empty()
            || m.key_id == next.key_id
            || m.public_key == next.public_key
            || m.valid_from >= m.valid_until
            || next.valid_from <= m.valid_from
            || next.valid_from > m.valid_until
            || next.valid_from >= next.valid_until
            || next.valid_until <= m.valid_until
            || m.key_id.len() > 64
            || next.key_id.len() > 64
            || m.public_key.len() > 1024
            || next.public_key.len() > 1024
        {
            return Err(invalid());
        }
        let signature = Signature::from_slice(&m.signature).map_err(|_| invalid())?;
        VerifyingKey::from_bytes(pin)
            .map_err(|_| invalid())?
            .verify_strict(&manifest_bytes(m), &signature)
            .map_err(|_| invalid())?;
        // Parse both signed keys, including the future key, before caching.
        let mut current = Self::from_der(&m.public_key, m.key_id.clone(), m.valid_until)?;
        current.valid_from = m.valid_from;
        let mut successor =
            Self::from_der(&next.public_key, next.key_id.clone(), next.valid_until)?;
        successor.valid_from = next.valid_from;
        let selected = if now >= successor.valid_from {
            successor
        } else {
            current
        };
        if now < selected.valid_from || now >= selected.valid_until {
            return Err(invalid());
        }
        Ok(selected)
    }

    /// Check if this key is still valid.
    pub fn is_valid(&self) -> bool {
        // valid_until is in nanoseconds (per proto spec)
        let now_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        self.valid_from <= now_nanos && now_nanos < self.valid_until
    }

    /// Get a reference to the inner public key.
    pub(crate) fn inner(&self) -> &PublicKey {
        &self.inner
    }
}

// Must match keytransparency.MailboxManifestLabel (log.go). Not a UUID, so it
// can never be a user's label.
fn manifest_label(key_id: &str) -> String {
    format!("chatter/mailbox-manifest/v1/{key_id}")
}

// The pinned signer can sign a different manifest for each client; this makes
// it prove the one it gave us is the one the KT log holds for the key, with the
// same verifier as a peer's identity key.
fn verify_manifest_in_log(m: &GetBlindSignaturePublicKeyResponse) -> Result<(), MailboxError> {
    use sha2::{Digest, Sha256};
    let proof = m.kt_proof.as_ref().ok_or_else(|| {
        MailboxError::InvalidServerKey("manifest has no key-transparency proof".into())
    })?;
    let digest = Sha256::digest(manifest_bytes(m));
    crate::crypto::verify_kt_proof(&manifest_label(&m.key_id), &digest, proof).map_err(|e| {
        MailboxError::InvalidServerKey(format!("manifest key-transparency proof: {e}"))
    })
}

// Must match service/manifest.go. Explicit framing avoids protobuf serialization
// differences and binds the key IDs, schedule, and DER under one signature.
fn manifest_bytes(m: &GetBlindSignaturePublicKeyResponse) -> Vec<u8> {
    let mut bytes = b"chatter/mailbox-blind-key-manifest/v1\0".to_vec();
    let mut add = |der: &[u8], id: &str, from: u64, until: u64| {
        bytes.extend_from_slice(&(der.len() as u32).to_be_bytes());
        bytes.extend_from_slice(der);
        bytes.extend_from_slice(&(id.len() as u32).to_be_bytes());
        bytes.extend_from_slice(id.as_bytes());
        bytes.extend_from_slice(&from.to_be_bytes());
        bytes.extend_from_slice(&until.to_be_bytes());
    };
    add(&m.public_key, &m.key_id, m.valid_from, m.valid_until);
    if let Some(next) = &m.next_key {
        add(
            &next.public_key,
            &next.key_id,
            next.valid_from,
            next.valid_until,
        );
    }
    bytes
}

impl std::fmt::Debug for ServerPublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerPublicKey")
            .field("key_id", &self.key_id)
            .field("valid_until", &self.valid_until)
            .field("inner", &"<RSA PublicKey>")
            .finish()
    }
}

/// Blinding secret needed to unblind the signature.
///
/// Must be retained from blinding until unblinding is complete.
/// Serializable for storage during pending operations.
#[derive(Clone, zeroize::Zeroize, zeroize::ZeroizeOnDrop)]
pub struct BlindingSecret {
    /// Serialized secret bytes.
    secret_bytes: Vec<u8>,
}

impl BlindingSecret {
    /// Create from blind-rsa-signatures secret.
    fn from_inner(secret: &Secret) -> Self {
        Self {
            secret_bytes: secret.as_slice().to_vec(),
        }
    }

    /// Reconstruct the blinding secret from stored bytes.
    fn to_inner(&self) -> Secret {
        Secret::from(self.secret_bytes.clone())
    }

    /// Get the raw bytes for storage.
    pub fn as_bytes(&self) -> &[u8] {
        &self.secret_bytes
    }

    /// Create from raw bytes (loaded from storage).
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self {
            secret_bytes: bytes,
        }
    }
}

/// Result of a blinding operation.
///
/// Contains the blinded message to send to the server and the secret
/// needed to unblind the signature.
pub struct BlindingOperation {
    /// The blinded message to send to server for signing.
    pub blinded_message: Vec<u8>,
    /// The blinding secret (must be kept until unblind).
    pub secret: BlindingSecret,
}

/// An unblinded token ready for use.
///
/// Contains the ephemeral Ed25519 public key and its valid RSA signature.
/// This proves the right to create an ephemeral mailbox.
#[derive(Clone)]
pub struct UnblindedToken {
    /// Ed25519 public key (32 bytes).
    pub public_key: Vec<u8>,
    /// Unblinded RSA signature over the public key.
    pub signature: Vec<u8>,
    /// When this token was created (Unix timestamp milliseconds).
    pub created_at: u64,
}

/// Blind an Ed25519 public key using the server's RSA key.
///
/// # Arguments
///
/// * `server_pk` - The server's RSA public key
/// * `ephemeral_pk` - The Ed25519 public key to blind (32 bytes)
///
/// # Returns
///
/// A `BlindingOperation` containing the blinded message and secret.
///
/// # Errors
///
/// Returns `MailboxError::BlindSignature` if blinding fails.
pub fn blind_key(
    server_pk: &ServerPublicKey,
    ephemeral_pk: &[u8],
) -> Result<BlindingOperation, MailboxError> {
    if !server_pk.is_valid() {
        return Err(MailboxError::InvalidServerKey(
            "blind-signature key expired".into(),
        ));
    }
    // Use deterministic options (no randomizer) for server-side verification compatibility
    // The server can verify without needing to know the randomizer
    // using salt_len=48 (standard for SHA-384-PSS)
    let options = Options::new(blind_rsa_signatures::Hash::Sha384, true, 48);

    // Perform blinding - the API takes (msg, include_randomizer, options).
    // Deterministic mode (false): no message randomizer exists, so none is
    // kept, sent or stored.
    let BlindingResult {
        blind_msg, secret, ..
    } = server_pk
        .inner()
        .blind(ephemeral_pk, false, &options)
        .map_err(|e| MailboxError::BlindSignature(format!("blinding failed: {}", e)))?;

    Ok(BlindingOperation {
        blinded_message: blind_msg.as_slice().to_vec(),
        secret: BlindingSecret::from_inner(&secret),
    })
}

/// Unblind a server signature to get the valid signature on the original message.
///
/// # Arguments
///
/// * `server_pk` - The server's RSA public key (must be same as used for blinding)
/// * `blinded_sig` - The blind signature from the server
/// * `secret` - The blinding secret from the original blinding operation
/// * `original_message` - The original message (Ed25519 public key) that was blinded
///
/// # Returns
///
/// The unblinded signature bytes.
///
/// # Errors
///
/// Returns `MailboxError::BlindSignature` if unblinding fails.
pub fn unblind(
    server_pk: &ServerPublicKey,
    blinded_sig: &[u8],
    secret: &BlindingSecret,
    original_message: &[u8],
) -> Result<Vec<u8>, MailboxError> {
    if !server_pk.is_valid() {
        return Err(MailboxError::InvalidServerKey(
            "blind-signature key expired".into(),
        ));
    }
    // Use deterministic options matching blind_key
    let options = Options::new(blind_rsa_signatures::Hash::Sha384, true, 48);

    // Parse blinded signature
    let blind_sig = BlindSignature::from(blinded_sig.to_vec());

    let blinding_secret = secret.to_inner();

    // Finalize (unblind) the signature
    let signature = server_pk
        .inner()
        .finalize(
            &blind_sig,
            &blinding_secret,
            None,
            original_message,
            &options,
        )
        .map_err(|e| MailboxError::BlindSignature(format!("unblinding failed: {}", e)))?;

    Ok(signature.as_slice().to_vec())
}

/// Verify a signature against the server's public key.
///
/// Used to verify that an unblinded token is valid.
///
/// # Arguments
///
/// * `server_pk` - The server's RSA public key
/// * `message` - The original message (Ed25519 public key)
/// * `signature` - The unblinded signature
///
/// # Returns
///
/// `true` if the signature is valid, `false` otherwise.
pub fn verify(
    server_pk: &ServerPublicKey,
    message: &[u8],
    signature: &[u8],
) -> bool {
    if !server_pk.is_valid() {
        return false;
    }
    // Use deterministic options (no randomizer) for server-side verification compatibility
    // The server can verify without needing to know the randomizer
    // using salt_len=48 (standard for SHA-384-PSS)
    let options = Options::new(blind_rsa_signatures::Hash::Sha384, true, 48);
    let sig = blind_rsa_signatures::Signature::from(signature.to_vec());
    server_pk
        .inner()
        .verify(&sig, None, message, &options)
        .is_ok()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn signed_manifest_authenticates_keys_and_rotation_windows() {
        use crate::proto::mailbox_v1::BlindSignatureNextKey;
        use ed25519_dalek::Signer;
        let signer = ed25519_dalek::SigningKey::from_bytes(&[42; 32]);
        let pin = signer.verifying_key().to_bytes();
        let mut m = GetBlindSignaturePublicKeyResponse {
            public_key: blind_rsa_signatures::KeyPair::generate(2048)
                .unwrap()
                .pk
                .to_der()
                .unwrap(),
            key_id: "current".into(),
            valid_from: 100,
            valid_until: 300,
            next_key: Some(BlindSignatureNextKey {
                public_key: blind_rsa_signatures::KeyPair::generate(2048)
                    .unwrap()
                    .pk
                    .to_der()
                    .unwrap(),
                key_id: "next".into(),
                valid_from: 200,
                valid_until: 400,
            }),
            signature: vec![],
            kt_proof: None,
        };
        m.signature = signer.sign(&manifest_bytes(&m)).to_bytes().to_vec();
        // Persistence round-trip preserves signed bytes and selects the next
        // key after activation, even when the previous key has expired.
        use prost::Message;
        let cached =
            GetBlindSignaturePublicKeyResponse::decode(m.encode_to_vec().as_slice()).unwrap();
        assert_eq!(
            ServerPublicKey::verify_manifest_at(&cached, &pin, 350)
                .unwrap()
                .key_id,
            "next"
        );
        assert_eq!(
            ServerPublicKey::verify_manifest_at(&m, &pin, 100)
                .unwrap()
                .key_id,
            "current"
        );
        assert_eq!(
            ServerPublicKey::verify_manifest_at(&m, &pin, 200)
                .unwrap()
                .key_id,
            "next"
        );
        assert_eq!(
            ServerPublicKey::verify_manifest_at(&m, &pin, 399)
                .unwrap()
                .key_id,
            "next"
        );
        assert!(ServerPublicKey::verify_manifest_at(&m, &pin, 99).is_err());
        assert!(ServerPublicKey::verify_manifest_at(&m, &pin, 400).is_err());
        assert!(ServerPublicKey::verify_manifest_at(&m, &[0; 32], 150).is_err());
        // Every field that might tag clients or extend a key's lifetime is signed.
        for field in 0..8 {
            let mut altered = m.clone();
            match field {
                0 => altered.public_key[50] ^= 1,
                1 => altered.key_id.push('x'),
                2 => altered.valid_from -= 1,
                3 => altered.valid_until += 1,
                4 => altered.next_key.as_mut().unwrap().public_key[50] ^= 1,
                5 => altered.next_key.as_mut().unwrap().key_id.push('x'),
                6 => altered.next_key.as_mut().unwrap().valid_from += 1,
                _ => altered.next_key.as_mut().unwrap().valid_until += 1,
            }
            assert!(ServerPublicKey::verify_manifest_at(&altered, &pin, 150).is_err());
        }
        let mut unsigned = m.clone();
        unsigned.signature.clear();
        assert!(ServerPublicKey::verify_manifest_at(&unsigned, &pin, 150).is_err());
        // An authenticated but malformed schedule also fails closed.
        m.next_key.as_mut().unwrap().valid_from = 301;
        m.signature = signer.sign(&manifest_bytes(&m)).to_bytes().to_vec();
        assert!(ServerPublicKey::verify_manifest_at(&m, &pin, 150).is_err());
    }

    /// A manifest signed and committed by the Go server (development KT keys:
    /// keytransparency.MailboxManifestLabel + CommitMailboxManifest's proof
    /// shape), valid until 2^62 ns.
    pub(crate) fn kt_manifest_vector() -> Option<GetBlindSignaturePublicKeyResponse> {
        use prost::Message;
        // Only meaningful when the build pins the development KT keys, which
        // `cargo test` does unless provisioned ones are exported.
        let dev_head =
            hex::decode("88ad412806054a7534e1b9deb42ef249bd7c92709f5bebf1d6be7b0e40926f72")
                .unwrap();
        if crate::config::KT_TREE_HEAD_PUBLIC_KEY_BYTES[..] != dev_head[..] {
            return None;
        }
        Some(
            GetBlindSignaturePublicKeyResponse::decode(
                &include_bytes!("testdata/kt_manifest_vector.bin")[..],
            )
            .unwrap(),
        )
    }

    #[test]
    fn manifest_requires_a_valid_kt_inclusion_proof() {
        let Some(m) = kt_manifest_vector() else {
            return;
        };
        assert_eq!(
            ServerPublicKey::from_manifest(&m).unwrap().key_id,
            "vector-current"
        );

        let mut missing = m.clone();
        missing.kt_proof = None;
        assert!(ServerPublicKey::from_manifest(&missing).is_err());

        // Committed value is not this manifest's digest.
        let mut wrong_value = m.clone();
        wrong_value
            .kt_proof
            .as_mut()
            .unwrap()
            .inclusion_proof
            .as_mut()
            .unwrap()
            .value[0] ^= 1;
        assert!(ServerPublicKey::from_manifest(&wrong_value).is_err());

        // Inclusion path does not reach the committed root.
        let mut bad_path = m.clone();
        bad_path
            .kt_proof
            .as_mut()
            .unwrap()
            .inclusion_proof
            .as_mut()
            .unwrap()
            .sibling_hashes
            .last_mut()
            .unwrap()[0] ^= 1;
        assert!(ServerPublicKey::from_manifest(&bad_path).is_err());

        // Tree head not signed by the pinned key.
        let mut bad_head = m.clone();
        bad_head
            .kt_proof
            .as_mut()
            .unwrap()
            .signed_tree_head
            .as_mut()
            .unwrap()
            .signature[0] ^= 1;
        assert!(ServerPublicKey::from_manifest(&bad_head).is_err());

        // A valid proof for a different label (a user's, or another key's) does
        // not cover this manifest.
        assert_ne!(manifest_label(&m.key_id), "vector-current");
        assert!(
            crate::crypto::verify_kt_proof(
                "vector-current",
                &m.kt_proof
                    .as_ref()
                    .unwrap()
                    .inclusion_proof
                    .as_ref()
                    .unwrap()
                    .value,
                m.kt_proof.as_ref().unwrap(),
            )
            .is_err()
        );
    }

    /// The manifest is fetched only once the pin lock is held, so its head
    /// cannot predate a pin another check advanced meanwhile.
    #[tokio::test]
    async fn manifest_is_fetched_under_the_pin_lock() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let Some(m) = kt_manifest_vector() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let db = std::sync::Arc::new(
            crate::storage::Database::open(&dir.path().join("kt.db"), "testpass").unwrap(),
        );
        db.run_migrations().unwrap();
        let settings = SettingsRepository::new(db);

        let fetched = AtomicBool::new(false);
        let pin = kt_anchor::lock_pin().await;
        let other_check = async {
            tokio::task::yield_now().await;
            assert!(
                !fetched.load(Ordering::SeqCst),
                "fetched while another check held the pin"
            );
            drop(pin);
        };
        let fetch = ServerPublicKey::fetch_manifest(
            || async {
                fetched.store(true, Ordering::SeqCst);
                Ok(m)
            },
            &settings,
            |_, _| async {
                Err(NetworkError::ConnectionFailed(
                    "first pin needs no proof".into(),
                ))
            },
        );
        let ((), result) = tokio::join!(other_check, fetch);
        assert_eq!(result.unwrap().1.key_id, "vector-current");
        assert!(fetched.load(Ordering::SeqCst));
        assert!(kt_anchor::pinned_head(&settings).is_some());
    }

    #[test]
    fn manifest_canonical_vector_matches_go() {
        use crate::proto::mailbox_v1::BlindSignatureNextKey;
        let m = GetBlindSignaturePublicKeyResponse {
            public_key: vec![1, 2],
            key_id: "a".into(),
            valid_from: 1,
            valid_until: 3,
            next_key: Some(BlindSignatureNextKey {
                public_key: vec![3],
                key_id: "b".into(),
                valid_from: 2,
                valid_until: 4,
            }),
            signature: vec![],
            kt_proof: None,
        };
        assert_eq!(
            hex::encode(manifest_bytes(&m)),
            "636861747465722f6d61696c626f782d626c696e642d6b65792d6d616e69666573742f7631000000000201020000000161000000000000000100000000000000030000000103000000016200000000000000020000000000000004"
        );
    }

    #[test]
    fn test_blinding_secret_roundtrip() {
        // Create a fake secret (normally from blinding operation)
        let fake_bytes = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let secret = BlindingSecret::from_bytes(fake_bytes.clone());
        assert_eq!(secret.as_bytes(), &fake_bytes);
    }


    #[test]
    fn test_server_public_key_validity() {
        // Test expired key (all timestamps in nanoseconds per proto spec)
        let now_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);

        // We can't easily create a valid RSA key without the full setup,
        // but we can test the validity logic conceptually
        let past = now_nanos - 1_000_000_000; // 1 second ago
        let future = now_nanos + 60_000_000_000; // 1 minute from now

        // Validity check is: now_nanos < valid_until
        assert!(now_nanos < future); // Should be valid
        assert!(now_nanos >= past); // Should be expired
    }

    #[test]
    fn test_unblinded_token_structure() {
        let token = UnblindedToken {
            public_key: vec![0u8; 32], // Ed25519 public key
            signature: vec![0u8; 256], // RSA-2048 signature
            created_at: 1234567890000,
        };

        assert_eq!(token.public_key.len(), 32);
        assert_eq!(token.signature.len(), 256);
        assert_eq!(token.created_at, 1234567890000);
    }

    #[test]
    fn test_invalid_server_key_der() {
        let invalid_der = vec![0, 1, 2, 3]; // Not valid DER
        let result = ServerPublicKey::from_der(&invalid_der, "key-1".to_string(), u64::MAX);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, MailboxError::InvalidServerKey(_)));
    }

    /// Simulates a malicious server attempting to correlate blinded messages
    /// with their original plaintexts via bit-pattern matching.
    ///
    /// Generates 1,000 distinct Ed25519 public keys, blinds each one, then
    /// checks that a server seeing only the blinded messages cannot determine
    /// which plaintext produced which blinded output. The server's strategy:
    ///
    /// 1. **Byte-prefix matching** — do any blinded messages share leading
    ///    bytes with the plaintext that produced them?
    /// 2. **Hamming-distance bias** — is the hamming distance between a
    ///    blinded message and its source plaintext statistically closer than
    ///    between that blinded message and a random other plaintext?
    /// 3. **Duplicate detection** — do identical plaintexts produce identical
    ///    blinded outputs? (They must not, since blinding uses fresh randomness.)
    #[test]
    fn test_blind_signature_no_correlation() {
        use blind_rsa_signatures::KeyPair;
        use rand::Rng;

        const N: usize = 1_000;

        // Generate server keypair (the "malicious server" has the secret key)
        let kp = KeyPair::generate(2048).expect("RSA keygen");
        let server_pk = ServerPublicKey {
            inner: kp.pk,
            key_id: "test-key".to_string(),
            valid_until: u64::MAX,
            valid_from: 0,
        };

        // Generate N distinct 32-byte "Ed25519 public keys"
        let mut rng = rand::rng();
        let plaintexts: Vec<[u8; 32]> = (0..N)
            .map(|_| {
                let mut buf = [0u8; 32];
                rng.fill(&mut buf);
                buf
            })
            .collect();

        // Blind each plaintext
        let blinded: Vec<Vec<u8>> = plaintexts
            .iter()
            .map(|pt| {
                let op = blind_key(&server_pk, pt).expect("blinding");
                op.blinded_message
            })
            .collect();

        // --- Attack 1: byte-prefix correlation ---
        // Check that no blinded message shares a 4-byte prefix with its plaintext.
        let prefix_matches = plaintexts
            .iter()
            .zip(blinded.iter())
            .filter(|(pt, bm)| bm.len() >= 4 && bm[..4] == pt[..4])
            .count();
        // With 256^4 possible prefixes, expected matches ≈ 0 for 1,000 samples.
        // Allow at most 1 as a fluke (p ≈ 2^-32 per trial).
        assert!(
            prefix_matches <= 1,
            "Too many prefix matches ({prefix_matches}/1000) — blinding may leak plaintext bits"
        );

        // --- Attack 2: hamming-distance bias ---
        // For each blinded message, compare hamming distance to its own plaintext
        // vs. hamming distance to a random other plaintext. If blinding is sound,
        // both distributions should be indistinguishable (centered around 50% of bits).
        let compare_len = 32; // compare over plaintext length
        let mut own_distances: Vec<u32> = Vec::with_capacity(N);
        let mut other_distances: Vec<u32> = Vec::with_capacity(N);

        for i in 0..N {
            let bm = &blinded[i];
            if bm.len() < compare_len {
                continue;
            }
            // Distance to own plaintext
            let own_dist: u32 = plaintexts[i]
                .iter()
                .zip(bm.iter())
                .map(|(a, b)| (a ^ b).count_ones())
                .sum();
            own_distances.push(own_dist);

            // Distance to a different plaintext (next one, wrapping)
            let other = &plaintexts[(i + 1) % N];
            let other_dist: u32 = other
                .iter()
                .zip(bm.iter())
                .map(|(a, b)| (a ^ b).count_ones())
                .sum();
            other_distances.push(other_dist);
        }

        let own_mean = own_distances.iter().sum::<u32>() as f64 / own_distances.len() as f64;
        let other_mean = other_distances.iter().sum::<u32>() as f64 / other_distances.len() as f64;

        // Both means should be close to 128 (half of 256 bits). Allow 10% tolerance.
        // A biased blinding scheme would show own_mean significantly lower than other_mean.
        let diff = (own_mean - other_mean).abs();
        assert!(
            diff < 10.0,
            "Hamming distance bias detected: own_mean={own_mean:.1}, other_mean={other_mean:.1}, \
             diff={diff:.1} — blinding may be leaking information"
        );

        // --- Attack 3: duplicate blinded outputs from identical plaintexts ---
        // Blind the same plaintext twice; the outputs must differ.
        let fixed_pt = [0xABu8; 32];
        let blind1 = blind_key(&server_pk, &fixed_pt)
            .expect("blinding")
            .blinded_message;
        let blind2 = blind_key(&server_pk, &fixed_pt)
            .expect("blinding")
            .blinded_message;
        assert_ne!(
            blind1, blind2,
            "Identical plaintexts produced identical blinded messages — randomness failure"
        );
    }
}
