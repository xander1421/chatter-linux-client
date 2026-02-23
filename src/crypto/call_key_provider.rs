//! Call encryption key derivation.
//!
//! Derives AES-256 keys for LiveKit E2EE from Signal Protocol sessions.
//! This provides Signal-level key establishment with AES-GCM media encryption.
//!
//! # Design
//!
//! For calls, we use ECDH between identity keys + HKDF to derive a shared
//! symmetric key. This is separate from the Double Ratchet session:
//! - Double Ratchet is too slow for 30fps media encryption
//! - We use the identity keys for a one-time key agreement
//! - The derived key is used with LiveKit's E2EE (AES-GCM)
//!
//! # Usage
//!
//! ```ignore
//! // Derive call key from ECDH shared secret
//! let call_key = derive_call_key(&shared_secret, "call-id-123")?;
//!
//! // Or use CallKeyState for key lifecycle management
//! let mut state = CallKeyState::new(&shared_secret, "call-id-123")?;
//! let key = state.current_key_vec(); // For LiveKit KeyProvider
//! state.ratchet()?; // Rotate key during long calls
//! ```

use hkdf::Hkdf;
use sha2::Sha256;

use crate::error::CryptoError;

/// HKDF salt for call key derivation.
const CALL_KEY_SALT: &[u8] = b"ChatterCallKey-v1";

/// Info prefix for call key derivation.
const CALL_KEY_INFO_PREFIX: &[u8] = b"call-encryption:";

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

/// Key provider for LiveKit E2EE integration.
///
/// Wraps the derived key and provides methods for key ratcheting
/// during long calls. Each ratchet produces a new key derived from
/// the current key, providing forward secrecy within the call.
pub struct CallKeyState {
    /// Current encryption key.
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
}
