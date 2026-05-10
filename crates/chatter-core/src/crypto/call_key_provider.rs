//! Call encryption key derivation.
//!
//! Derives AES-256 keys for LiveKit E2EE from Signal Protocol sessions.
//! This provides Signal-level key establishment with AES-GCM media encryption.
//!
//! # Design
//!
//! ## DM Calls (Ephemeral Key Exchange)
//!
//! For 1:1 calls, we use ephemeral X25519 keys for Perfect Forward Secrecy (PFS):
//! 1. Caller generates ephemeral keypair, sends public key in offer
//! 2. Callee generates ephemeral keypair, sends public key in answer
//! 3. Both sides compute: ECDH(our_ephemeral, peer_ephemeral)
//! 4. Derive SRTP key: HKDF(shared || caller_identity || callee_identity)
//! 5. Delete ephemeral private keys immediately after derivation
//!
//! This mirrors Signal's 1:1 call setup (per Signal Protocol spec).
//!
//! ## Group Calls (Distributed Key)
//!
//! For group calls, the initiator generates a random symmetric key and
//! distributes it to all participants via sender key encrypted messages.
//! Key rotation occurs when members leave.
//!
//! # Usage
//!
//! ```ignore
//! // DM call with ephemeral keys (PFS)
//! let ephemeral = EphemeralCallKey::generate();
//! let offer_bytes = ephemeral.public_key_bytes();
//! // ... send offer, receive answer ...
//! let shared = ephemeral.diffie_hellman(&peer_ephemeral_pk)?;
//! let call_key = derive_call_key_ephemeral(&shared, &our_identity, &peer_identity, call_id, true)?;
//! // ephemeral is automatically zeroized on drop
//!
//! // Group call key
//! let group_key = generate_group_call_key();
//! // ... distribute to members via sender key messages ...
//! ```

use hkdf::Hkdf;
use libsignal_protocol::{IdentityKey, IdentityKeyPair};
use sha2::Sha256;
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret as X25519PrivateKey};
use zeroize::ZeroizeOnDrop;

use crate::error::CryptoError;

/// HKDF salt for call key derivation.
const CALL_KEY_SALT: &[u8] = b"ChatterCallKey-v1";

/// Info prefix for call key derivation.
const CALL_KEY_INFO_PREFIX: &[u8] = b"call-encryption:";

/// Info string for ephemeral DH-based call key derivation (Signal-style).
const EPHEMERAL_CALL_INFO: &[u8] = b"Chatter_Calling_EphemeralDH_SRTPKey_KDF";

/// Derive a shared secret from identity keys using X25519 ECDH.
///
/// Signal identity keys use Curve25519 (X25519) keys with a 0x05 type prefix.
/// The public key bytes are already in Montgomery form, so no conversion is needed.
///
/// # Arguments
/// * `our_identity` - Our identity key pair
/// * `peer_identity` - Peer's identity public key
///
/// # Returns
/// 32-byte ECDH shared secret.
pub fn derive_call_shared_secret(
    our_identity: &IdentityKeyPair,
    peer_identity: &IdentityKey,
) -> Result<[u8; 32], CryptoError> {
    // Get our private key bytes and peer's public key bytes
    let our_private_bytes = our_identity.private_key().serialize();
    let peer_public_bytes = peer_identity.serialize();

    // The identity key serialization includes a type byte prefix (0x05 for DJB/Curve25519)
    // Skip the first byte to get the actual 32-byte X25519 public key
    if peer_public_bytes.len() != 33 || peer_public_bytes[0] != 0x05 {
        return Err(CryptoError::InvalidKeyFormat);
    }
    let peer_x25519_bytes: [u8; 32] = peer_public_bytes[1..33]
        .try_into()
        .map_err(|_| CryptoError::InvalidKeyFormat)?;

    // Signal public keys are already X25519 (Curve25519 in Montgomery form)
    // No conversion needed - use them directly
    let peer_x25519_public = X25519PublicKey::from(peer_x25519_bytes);

    // Signal private keys are already clamped scalars suitable for X25519
    let our_x25519_private: [u8; 32] = our_private_bytes[..32]
        .try_into()
        .map_err(|_| CryptoError::InvalidKeyFormat)?;
    let our_private = X25519PrivateKey::from(our_x25519_private);

    // Compute ECDH shared secret
    let shared_secret = our_private.diffie_hellman(&peer_x25519_public);

    Ok(*shared_secret.as_bytes())
}

/// Derives a 32-byte call encryption key from a shared secret.
///
/// For DM calls, the shared_secret comes from ECDH(our_identity, peer_identity).
/// For group calls, the shared_secret comes from the sender key chain.
///
/// # Arguments
/// * `shared_secret` - 32-byte ECDH shared secret or sender key
/// * `call_id` - Unique call identifier (used as info)
///
/// # Returns
/// 32-byte AES-256 key suitable for LiveKit E2EE.
pub fn derive_call_key(
    shared_secret: &[u8; 32],
    call_id: &str,
) -> Result<[u8; 32], CryptoError> {
    let hkdf = Hkdf::<Sha256>::new(Some(CALL_KEY_SALT), shared_secret);

    // Combine prefix with call_id for info parameter
    let mut info = CALL_KEY_INFO_PREFIX.to_vec();
    info.extend_from_slice(call_id.as_bytes());

    let mut call_key = [0u8; 32];
    hkdf.expand(&info, &mut call_key)
        .map_err(|_| CryptoError::KeyDerivationFailed("HKDF expand failed".into()))?;

    Ok(call_key)
}

// =============================================================================
// Ephemeral Key Exchange (Signal-style PFS for DM calls)
// =============================================================================

/// Ephemeral X25519 keypair for call key exchange.
///
/// This provides Perfect Forward Secrecy (PFS) for DM calls:
/// - Private key is zeroized on drop
/// - Generate fresh keypair for each call
/// - Exchange public keys in offer/answer
/// - Delete after deriving shared secret
///
/// Per Signal Protocol: ephemeral keys provide PFS - even if identity keys
/// are later compromised, past call keys cannot be derived.
#[derive(ZeroizeOnDrop)]
pub struct EphemeralCallKey {
    /// X25519 private key (32 bytes, zeroized on drop).
    #[zeroize(skip)] // X25519PrivateKey handles its own zeroization
    secret: X25519PrivateKey,
    /// X25519 public key (32 bytes).
    public: X25519PublicKey,
}

impl EphemeralCallKey {
    /// Generate a new ephemeral X25519 keypair.
    ///
    /// The private key is automatically zeroized when this struct is dropped.
    pub fn generate() -> Self {
        // SECURITY: ephemeral private key must come from a draw that hits
        // the OS entropy pool, not a userspace ChaCha PRG seeded once
        // (`rand::rng()` returns ThreadRng, which is exactly that pattern).
        // ringrtc uses `rand::rngs::OsRng` directly for the same reason.
        // Memory disclosure of a single seed otherwise reproduces every
        // ephemeral key generated by this thread - i.e. every PFS call key.
        use rand::TryRngCore;
        let mut rng = rand::rngs::OsRng.unwrap_err();

        let mut secret_bytes = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rng, &mut secret_bytes);

        let secret = X25519PrivateKey::from(secret_bytes);
        let public = X25519PublicKey::from(&secret);
        Self { secret, public }
    }

    /// Reconstruct from raw private key bytes (for service-layer use).
    ///
    /// The private key is automatically zeroized when this struct is dropped.
    pub fn from_private_bytes(bytes: [u8; 32]) -> Self {
        let secret = X25519PrivateKey::from(bytes);
        let public = X25519PublicKey::from(&secret);
        Self { secret, public }
    }

    /// Get the private key bytes for passing to the service layer.
    ///
    /// The caller is responsible for ensuring these bytes are handled securely.
    pub fn private_key_bytes(&self) -> [u8; 32] {
        self.secret.to_bytes()
    }

    /// Get the public key bytes to send in offer/answer.
    pub fn public_key_bytes(&self) -> [u8; 32] {
        *self.public.as_bytes()
    }

    /// Compute ECDH with peer's ephemeral public key.
    ///
    /// # Arguments
    /// * `peer_public` - Peer's ephemeral public key (32 bytes from offer/answer)
    ///
    /// # Returns
    /// 32-byte ECDH shared secret.
    pub fn diffie_hellman(&self, peer_public: &[u8; 32]) -> [u8; 32] {
        let peer_pk = X25519PublicKey::from(*peer_public);
        let shared = self.secret.diffie_hellman(&peer_pk);
        *shared.as_bytes()
    }
}

/// Derive call encryption key using ephemeral ECDH + identity binding.
///
/// Signal-style derivation that binds both identity keys into the KDF,
/// providing MITM protection while using ephemeral keys for PFS.
///
/// Key = HKDF(
///     salt: "ChatterCallKey-v1",
///     ikm: ephemeral_shared_secret,
///     info: "Chatter_Calling_EphemeralDH_SRTPKey_KDF" || call_id || caller_identity || callee_identity
/// )
///
/// # Arguments
/// * `ephemeral_shared` - ECDH(our_ephemeral, peer_ephemeral)
/// * `our_identity` - Our Signal identity keypair
/// * `peer_identity` - Peer's Signal identity public key
/// * `call_id` - Unique call identifier
/// * `we_are_caller` - True if we initiated the call (determines identity ordering)
///
/// # Returns
/// 32-byte AES-256 key suitable for LiveKit E2EE.
pub fn derive_call_key_ephemeral(
    ephemeral_shared: &[u8; 32],
    our_identity: &IdentityKeyPair,
    peer_identity: &IdentityKey,
    call_id: &str,
    we_are_caller: bool,
) -> Result<[u8; 32], CryptoError> {
    let hkdf = Hkdf::<Sha256>::new(Some(CALL_KEY_SALT), ephemeral_shared);

    // Build info: prefix || call_id || caller_identity || callee_identity
    // Order is deterministic based on who initiated
    let our_id_bytes = our_identity.identity_key().serialize();
    let peer_id_bytes = peer_identity.serialize();

    let mut info = Vec::with_capacity(
        EPHEMERAL_CALL_INFO.len() + call_id.len() + our_id_bytes.len() + peer_id_bytes.len(),
    );
    info.extend_from_slice(EPHEMERAL_CALL_INFO);
    info.extend_from_slice(call_id.as_bytes());

    // Caller identity always comes first for deterministic derivation
    if we_are_caller {
        info.extend_from_slice(&our_id_bytes);
        info.extend_from_slice(&peer_id_bytes);
    } else {
        info.extend_from_slice(&peer_id_bytes);
        info.extend_from_slice(&our_id_bytes);
    }

    let mut call_key = [0u8; 32];
    hkdf.expand(&info, &mut call_key)
        .map_err(|_| CryptoError::KeyDerivationFailed("Ephemeral HKDF expand failed".into()))?;

    Ok(call_key)
}

// =============================================================================
// Anonymous DM Call Key Derivation (Hybrid)
// =============================================================================

/// HKDF salt for anonymous call key derivation.
const ANON_CALL_KEY_SALT: &[u8] = b"ChatterAnonCallKey-v1";

/// Derive the final call encryption key for anonymous DM calls.
///
/// Mixes the Signal session ECDH shared secret with a per-call random
/// contribution, producing a key that is:
/// - Bound to the Signal identity keys (MITM protection)
/// - Unique per call (random contribution)
/// - Deterministic given the same inputs (both sides derive same key)
///
/// Final key = HKDF(
///     salt: "ChatterAnonCallKey-v1",
///     ikm: signal_shared_secret || random_contribution,
///     info: "anon-call:" || room_name
/// )
///
/// # Arguments
/// * `signal_shared` - 32-byte ECDH shared secret from Signal identity keys
/// * `random_contribution` - 32-byte random value from the call offer
/// * `room_name` - The call room name (deterministic context binding)
///
/// # Returns
/// 32-byte AES-256 key for LiveKit E2EE.
pub fn derive_anon_call_key(
    signal_shared: &[u8; 32],
    random_contribution: &[u8; 32],
    room_name: &str,
) -> Result<[u8; 32], CryptoError> {
    // Concatenate both key materials
    let mut ikm = [0u8; 64];
    ikm[..32].copy_from_slice(signal_shared);
    ikm[32..].copy_from_slice(random_contribution);

    let hkdf = Hkdf::<Sha256>::new(Some(ANON_CALL_KEY_SALT), &ikm);

    let info = format!("anon-call:{}", room_name);
    let mut call_key = [0u8; 32];
    hkdf.expand(info.as_bytes(), &mut call_key)
        .map_err(|_| CryptoError::KeyDerivationFailed("Anon call HKDF expand failed".into()))?;

    Ok(call_key)
}

/// Derive the final call encryption key with optional post-quantum contribution.
///
/// Extends `derive_anon_call_key` with an optional Kyber shared secret.
/// When PQ is available: HKDF(signal || random || kyber, room_name)
/// When PQ is not available: falls back to HKDF(signal || random, room_name)
pub fn derive_anon_call_key_hybrid(
    signal_shared: &[u8; 32],
    random_contribution: &[u8; 32],
    pq_shared: Option<&[u8]>,
    room_name: &str,
) -> Result<[u8; 32], CryptoError> {
    // Concatenate all key materials
    let mut ikm = Vec::with_capacity(64 + pq_shared.map(|s| s.len()).unwrap_or(0));
    ikm.extend_from_slice(signal_shared);
    ikm.extend_from_slice(random_contribution);
    if let Some(pq) = pq_shared {
        ikm.extend_from_slice(pq);
    }

    let hkdf = Hkdf::<Sha256>::new(Some(ANON_CALL_KEY_SALT), &ikm);

    let info = format!("anon-call:{}", room_name);
    let mut call_key = [0u8; 32];
    hkdf.expand(info.as_bytes(), &mut call_key)
        .map_err(|_| CryptoError::KeyDerivationFailed("Hybrid anon call HKDF expand failed".into()))?;

    Ok(call_key)
}

// =============================================================================
// Group Call Key Generation
// =============================================================================

/// Generate a random 32-byte group call encryption key.
///
/// This key is distributed to all group members via **pairwise sealed sender**
/// messages (client-side fanout). Each member receives an independent encrypted
/// message - the server cannot correlate these to a single group.
///
/// Per Signal Protocol & NDSS '21:
/// - Server-side fanout (Sender Keys) is prohibited - reveals group membership
/// - Client-side pairwise encryption preserves metadata privacy
/// - Each GroupCallKey message is sent via sealed sender like a DM
///
/// Key rotation:
/// - When a member leaves, generate new key and redistribute via pairwise
/// - Use 3-second delay before using new key (allow propagation)
pub fn generate_group_call_key() -> [u8; 32] {
    // SECURITY: see comment in `EphemeralCallKey::generate` - draw from OS
    // each time, never from a userspace ChaCha PRG seeded once.
    use rand::TryRngCore;
    let mut rng = rand::rngs::OsRng.unwrap_err();
    let mut key = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rng, &mut key);
    key
}

/// Key provider for LiveKit E2EE integration.
///
/// Wraps the derived key and provides methods for key ratcheting
/// during long calls. Each ratchet produces a new key derived from
/// the current key, providing forward secrecy within the call.
///
/// # Security
///
/// Automatically zeroizes the encryption key when dropped to prevent
/// key material from lingering in memory after the call ends.
#[derive(ZeroizeOnDrop)]
pub struct CallKeyState {
    /// Current encryption key (zeroized on drop).
    #[zeroize(drop)]
    current_key: [u8; 32],
    /// Call ID this key is for.
    call_id: String,
    /// Key epoch (increments on ratchet).
    epoch: u32,
}

impl CallKeyState {
    /// Create a new key state for a call.
    ///
    /// # Arguments
    /// * `shared_secret` - 32-byte ECDH shared secret
    /// * `call_id` - Unique call identifier
    pub fn new(shared_secret: &[u8; 32], call_id: &str) -> Result<Self, CryptoError> {
        let current_key = derive_call_key(shared_secret, call_id)?;
        Ok(Self {
            current_key,
            call_id: call_id.to_string(),
            epoch: 0,
        })
    }

    /// Get the current encryption key.
    pub fn current_key(&self) -> &[u8; 32] {
        &self.current_key
    }

    /// Get the current key as a Vec (for LiveKit KeyProvider).
    pub fn current_key_vec(&self) -> Vec<u8> {
        self.current_key.to_vec()
    }

    /// Get the current epoch.
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    /// Get the call ID.
    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    /// Replace the current key with a new random key (for member-leave rotation).
    ///
    /// Unlike `ratchet()` which derives deterministically from the current key,
    /// this replaces the key entirely. The departed member cannot derive the
    /// new key since it's randomly generated. The caller must redistribute
    /// the new key to remaining participants.
    pub fn reset(&mut self, new_key: [u8; 32]) {
        self.current_key = new_key;
        self.epoch += 1;
    }

    /// Ratchet the key forward (for long-running calls).
    ///
    /// Derives a new key from the current key + epoch, providing
    /// forward secrecy within the call. Even if the current key
    /// is compromised, previous keys cannot be derived.
    pub fn ratchet(&mut self) -> Result<(), CryptoError> {
        self.epoch += 1;

        let info = format!("{}:epoch:{}", self.call_id, self.epoch);
        let hkdf = Hkdf::<Sha256>::new(Some(CALL_KEY_SALT), &self.current_key);

        hkdf.expand(info.as_bytes(), &mut self.current_key)
            .map_err(|_| CryptoError::KeyDerivationFailed("Key ratchet failed".into()))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-3 #4: ensure ephemeral call keys come from the OS each draw,
    /// not a userspace ChaCha PRG. Two consecutive draws must produce
    /// distinct outputs with overwhelming probability.
    ///
    /// This guards against a future regression that swaps `OsRng.unwrap_err()`
    /// back to `rand::rng()` (ThreadRng): under either RNG the test would
    /// pass for "different output", but ThreadRng caches a seed in process
    /// memory which is the actual hazard. The companion regression we rely on
    /// is the reading of the source file by `karpathy-guidelines` style
    /// review — not pure runtime detection.
    #[test]
    fn ephemeral_call_keys_are_random() {
        let k1 = EphemeralCallKey::generate();
        let k2 = EphemeralCallKey::generate();
        assert_ne!(k1.public_key_bytes(), k2.public_key_bytes());
        assert_ne!(k1.public_key_bytes(), [0u8; 32]);
    }

    #[test]
    fn group_call_keys_are_random() {
        let g1 = generate_group_call_key();
        let g2 = generate_group_call_key();
        assert_ne!(g1, g2);
        assert_ne!(g1, [0u8; 32]);
    }

    #[test]
    fn test_derive_call_key_deterministic() {
        let shared_secret = [1u8; 32];
        let call_id = "test-call-123";

        let key1 = derive_call_key(&shared_secret, call_id).unwrap();
        let key2 = derive_call_key(&shared_secret, call_id).unwrap();

        // Same inputs = same output
        assert_eq!(key1, key2);
    }

    #[test]
    fn test_derive_call_key_different_call_ids() {
        let shared_secret = [1u8; 32];

        let key1 = derive_call_key(&shared_secret, "call-1").unwrap();
        let key2 = derive_call_key(&shared_secret, "call-2").unwrap();

        // Different call_id = different key
        assert_ne!(key1, key2);
    }

    #[test]
    fn test_derive_call_key_different_secrets() {
        let secret1 = [1u8; 32];
        let secret2 = [2u8; 32];

        let key1 = derive_call_key(&secret1, "call-1").unwrap();
        let key2 = derive_call_key(&secret2, "call-1").unwrap();

        // Different shared_secret = different key
        assert_ne!(key1, key2);
    }

    #[test]
    fn test_key_state_initialization() {
        let shared_secret = [1u8; 32];
        let state = CallKeyState::new(&shared_secret, "test-call").unwrap();

        assert_eq!(state.epoch(), 0);
        assert_eq!(state.call_id(), "test-call");
        assert_eq!(state.current_key().len(), 32);
    }

    #[test]
    fn test_key_state_ratchet() {
        let shared_secret = [1u8; 32];
        let mut state = CallKeyState::new(&shared_secret, "test-call").unwrap();

        let initial_key = *state.current_key();
        assert_eq!(state.epoch(), 0);

        state.ratchet().unwrap();
        assert_eq!(state.epoch(), 1);
        assert_ne!(*state.current_key(), initial_key);
    }

    #[test]
    fn test_key_state_multiple_ratchets() {
        let shared_secret = [1u8; 32];
        let mut state = CallKeyState::new(&shared_secret, "test-call").unwrap();

        let key0 = *state.current_key();
        state.ratchet().unwrap();
        let key1 = *state.current_key();
        state.ratchet().unwrap();
        let key2 = *state.current_key();

        // All keys should be different
        assert_ne!(key0, key1);
        assert_ne!(key1, key2);
        assert_ne!(key0, key2);

        assert_eq!(state.epoch(), 2);
    }

    #[test]
    fn test_key_state_vec_output() {
        let shared_secret = [1u8; 32];
        let state = CallKeyState::new(&shared_secret, "test-call").unwrap();

        let key_vec = state.current_key_vec();
        assert_eq!(key_vec.len(), 32);
        assert_eq!(key_vec.as_slice(), state.current_key());
    }

    #[test]
    fn test_derive_call_shared_secret() {
        // Generate two identity key pairs
        let mut rng = rand::rng();
        let alice_identity = IdentityKeyPair::generate(&mut rng);
        let bob_identity = IdentityKeyPair::generate(&mut rng);

        // Debug: print the key format
        let alice_public = alice_identity.identity_key();
        let alice_serialized = alice_public.serialize();
        println!("Alice identity key serialized: {:?}", alice_serialized);
        println!("Length: {}, First byte: 0x{:02x}", alice_serialized.len(), alice_serialized[0]);

        // Try to derive shared secret
        let result = derive_call_shared_secret(&alice_identity, bob_identity.identity_key());

        match &result {
            Ok(secret) => println!("Success! Secret: {:?}", &secret[..8]),
            Err(e) => println!("Error: {:?}", e),
        }

        assert!(result.is_ok(), "derive_call_shared_secret should succeed");

        // Verify both parties derive the same secret
        let alice_to_bob = derive_call_shared_secret(&alice_identity, bob_identity.identity_key()).unwrap();
        let bob_to_alice = derive_call_shared_secret(&bob_identity, alice_identity.identity_key()).unwrap();

        assert_eq!(alice_to_bob, bob_to_alice, "Both parties should derive the same shared secret");
    }

    // =========================================================================
    // Ephemeral Key Exchange Tests (Signal-style PFS)
    // =========================================================================

    #[test]
    fn test_ephemeral_key_generation() {
        let key1 = EphemeralCallKey::generate();
        let key2 = EphemeralCallKey::generate();

        // Each key should be unique
        assert_ne!(key1.public_key_bytes(), key2.public_key_bytes());

        // Public key should be 32 bytes
        assert_eq!(key1.public_key_bytes().len(), 32);
    }

    #[test]
    fn test_ephemeral_ecdh_symmetric() {
        // Simulate offer/answer exchange
        let caller_ephemeral = EphemeralCallKey::generate();
        let callee_ephemeral = EphemeralCallKey::generate();

        // Exchange public keys
        let caller_pk = caller_ephemeral.public_key_bytes();
        let callee_pk = callee_ephemeral.public_key_bytes();

        // Both sides compute ECDH
        let caller_shared = caller_ephemeral.diffie_hellman(&callee_pk);
        let callee_shared = callee_ephemeral.diffie_hellman(&caller_pk);

        // ECDH should produce same result on both sides
        assert_eq!(caller_shared, callee_shared, "ECDH should be symmetric");
    }

    #[test]
    fn test_derive_call_key_ephemeral_symmetric() {
        let mut rng = rand::rng();
        let caller_identity = IdentityKeyPair::generate(&mut rng);
        let callee_identity = IdentityKeyPair::generate(&mut rng);

        // Generate ephemeral keys
        let caller_ephemeral = EphemeralCallKey::generate();
        let callee_ephemeral = EphemeralCallKey::generate();

        // Exchange public keys
        let caller_eph_pk = caller_ephemeral.public_key_bytes();
        let callee_eph_pk = callee_ephemeral.public_key_bytes();

        // Both compute ECDH
        let caller_shared = caller_ephemeral.diffie_hellman(&callee_eph_pk);
        let callee_shared = callee_ephemeral.diffie_hellman(&caller_eph_pk);

        let call_id = "test-call-123";

        // Caller derives key (we_are_caller = true)
        let caller_key = derive_call_key_ephemeral(
            &caller_shared,
            &caller_identity,
            callee_identity.identity_key(),
            call_id,
            true,
        ).unwrap();

        // Callee derives key (we_are_caller = false)
        let callee_key = derive_call_key_ephemeral(
            &callee_shared,
            &callee_identity,
            caller_identity.identity_key(),
            call_id,
            false,
        ).unwrap();

        // Both should derive the same call key
        assert_eq!(caller_key, callee_key, "Both parties should derive same call key");
    }

    #[test]
    fn test_ephemeral_keys_unique_per_call() {
        let mut rng = rand::rng();
        let caller_identity = IdentityKeyPair::generate(&mut rng);
        let callee_identity = IdentityKeyPair::generate(&mut rng);

        // Call 1
        let caller_eph1 = EphemeralCallKey::generate();
        let callee_eph1 = EphemeralCallKey::generate();
        let shared1 = caller_eph1.diffie_hellman(&callee_eph1.public_key_bytes());
        let key1 = derive_call_key_ephemeral(
            &shared1,
            &caller_identity,
            callee_identity.identity_key(),
            "call-1",
            true,
        ).unwrap();

        // Call 2 (different ephemeral keys)
        let caller_eph2 = EphemeralCallKey::generate();
        let callee_eph2 = EphemeralCallKey::generate();
        let shared2 = caller_eph2.diffie_hellman(&callee_eph2.public_key_bytes());
        let key2 = derive_call_key_ephemeral(
            &shared2,
            &caller_identity,
            callee_identity.identity_key(),
            "call-2",
            true,
        ).unwrap();

        // Different calls should have different keys (PFS)
        assert_ne!(key1, key2, "Different calls should have different keys");
    }

    // =========================================================================
    // Group Call Key Tests
    // =========================================================================

    #[test]
    fn test_generate_group_call_key() {
        let key1 = generate_group_call_key();
        let key2 = generate_group_call_key();

        // Each key should be 32 bytes
        assert_eq!(key1.len(), 32);
        assert_eq!(key2.len(), 32);

        // Keys should be unique
        assert_ne!(key1, key2, "Each group call key should be unique");
    }

    #[test]
    fn test_key_state_reset() {
        let shared_secret = [1u8; 32];
        let mut state = CallKeyState::new(&shared_secret, "test-call").unwrap();

        let initial_key = *state.current_key();
        assert_eq!(state.epoch(), 0);

        // Ratchet once so we're at epoch 1
        state.ratchet().unwrap();
        let ratcheted_key = *state.current_key();
        assert_eq!(state.epoch(), 1);

        // Reset with a new random key
        let new_key = [42u8; 32];
        state.reset(new_key);
        assert_eq!(state.epoch(), 2);
        assert_ne!(*state.current_key(), initial_key);
        assert_ne!(*state.current_key(), ratcheted_key);

        // The new key is derived from new_key (not the old chain)
        // Subsequent ratchets should work from the new key
        state.ratchet().unwrap();
        assert_eq!(state.epoch(), 3);
        assert_ne!(*state.current_key(), new_key);
    }

    #[test]
    fn test_key_state_reset_breaks_chain() {
        // Verify that reset() produces a key the old chain can't derive
        let shared_secret = [1u8; 32];
        let mut state_a = CallKeyState::new(&shared_secret, "test-call").unwrap();
        let mut state_b = CallKeyState::new(&shared_secret, "test-call").unwrap();

        // Both at epoch 0, same key
        assert_eq!(state_a.current_key(), state_b.current_key());

        // A resets with new key (member left), B continues ratcheting (departed member)
        let new_key = generate_group_call_key();
        state_a.reset(new_key);
        state_b.ratchet().unwrap();

        // Keys must differ — departed member can't derive the rotated key
        assert_ne!(state_a.current_key(), state_b.current_key());
    }

    #[test]
    fn test_group_call_key_not_all_zeros() {
        let key = generate_group_call_key();

        // Key should not be all zeros (cryptographic randomness)
        assert_ne!(key, [0u8; 32], "Group call key should not be all zeros");
    }
}
