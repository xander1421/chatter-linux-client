//! Mock Signal Protocol services.
//!
//! Provides mockable traits for Signal Protocol operations.
//! Note: Uses owned types instead of references for mockall compatibility.

use mockall::automock;
use crate::framework::fixtures::TestSession;
use super::services::{MockError, MockResult};

// ============================================================================
// Signal Service Trait
// ============================================================================

/// Mockable Signal service trait.
#[automock]
pub trait SignalServiceTrait: Send + Sync {
    /// Encrypt a message for a recipient.
    fn encrypt(
        &self,
        recipient_id: String,
        device_id: u32,
        plaintext: Vec<u8>,
    ) -> impl std::future::Future<Output = MockResult<Vec<u8>>> + Send;

    /// Decrypt a message.
    fn decrypt(
        &self,
        sender_id: String,
        device_id: u32,
        ciphertext: Vec<u8>,
    ) -> impl std::future::Future<Output = MockResult<Vec<u8>>> + Send;

    /// Check if session exists with recipient.
    fn has_session(
        &self,
        recipient_id: String,
        device_id: u32,
    ) -> impl std::future::Future<Output = MockResult<bool>> + Send;

    /// Establish session using prekey bundle.
    fn establish_session(
        &self,
        recipient_id: String,
        device_id: u32,
        prekey_bundle: Vec<u8>,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;

    /// Get local identity key.
    fn get_identity_key(
        &self,
    ) -> impl std::future::Future<Output = MockResult<Vec<u8>>> + Send;

    /// Get local registration ID.
    fn get_registration_id(
        &self,
    ) -> impl std::future::Future<Output = MockResult<u32>> + Send;
}

// ============================================================================
// Signal Store Traits
// ============================================================================

/// Mockable identity key store.
#[automock]
pub trait IdentityKeyStoreTrait: Send + Sync {
    /// Get identity key pair.
    fn get_identity_key_pair(
        &self,
    ) -> impl std::future::Future<Output = MockResult<(Vec<u8>, Vec<u8>)>> + Send;

    /// Get local registration ID.
    fn get_local_registration_id(
        &self,
    ) -> impl std::future::Future<Output = MockResult<u32>> + Send;

    /// Save identity.
    fn save_identity(
        &mut self,
        address: String,
        identity: Vec<u8>,
    ) -> impl std::future::Future<Output = MockResult<bool>> + Send;

    /// Check if identity is trusted.
    fn is_trusted_identity(
        &self,
        address: String,
        identity: Vec<u8>,
    ) -> impl std::future::Future<Output = MockResult<bool>> + Send;

    /// Get identity.
    fn get_identity(
        &self,
        address: String,
    ) -> impl std::future::Future<Output = MockResult<Option<Vec<u8>>>> + Send;
}

/// Mockable session store.
#[automock]
pub trait SessionStoreTrait: Send + Sync {
    /// Load session.
    fn load_session(
        &self,
        address: String,
    ) -> impl std::future::Future<Output = MockResult<Option<Vec<u8>>>> + Send;

    /// Store session.
    fn store_session(
        &mut self,
        address: String,
        record: Vec<u8>,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;
}

/// Mockable prekey store.
#[automock]
pub trait PreKeyStoreTrait: Send + Sync {
    /// Get prekey.
    fn get_pre_key(
        &self,
        prekey_id: u32,
    ) -> impl std::future::Future<Output = MockResult<Vec<u8>>> + Send;

    /// Save prekey.
    fn save_pre_key(
        &mut self,
        prekey_id: u32,
        record: Vec<u8>,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;

    /// Remove prekey.
    fn remove_pre_key(
        &mut self,
        prekey_id: u32,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;
}

/// Mockable signed prekey store.
#[automock]
pub trait SignedPreKeyStoreTrait: Send + Sync {
    /// Get signed prekey.
    fn get_signed_pre_key(
        &self,
        signed_prekey_id: u32,
    ) -> impl std::future::Future<Output = MockResult<Vec<u8>>> + Send;

    /// Save signed prekey.
    fn save_signed_pre_key(
        &mut self,
        signed_prekey_id: u32,
        record: Vec<u8>,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;
}

/// Mockable sender key store.
#[automock]
pub trait SenderKeyStoreTrait: Send + Sync {
    /// Store sender key.
    fn store_sender_key(
        &mut self,
        sender: String,
        distribution_id: Vec<u8>,
        record: Vec<u8>,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;

    /// Load sender key.
    fn load_sender_key(
        &self,
        sender: String,
        distribution_id: Vec<u8>,
    ) -> impl std::future::Future<Output = MockResult<Option<Vec<u8>>>> + Send;
}

// ============================================================================
// Sealed Sender Trait
// ============================================================================

/// Mockable sealed sender service.
#[automock]
pub trait SealedSenderServiceTrait: Send + Sync {
    /// Create sealed sender message.
    fn seal_message(
        &self,
        recipient_id: String,
        device_id: u32,
        plaintext: Vec<u8>,
    ) -> impl std::future::Future<Output = MockResult<Vec<u8>>> + Send;

    /// Open sealed sender message.
    fn unseal_message(
        &self,
        ciphertext: Vec<u8>,
    ) -> impl std::future::Future<Output = MockResult<(String, u32, Vec<u8>)>> + Send; // (sender_id, device_id, plaintext)

}

// ============================================================================
// In-Memory Signal Stores for Testing
// ============================================================================

use std::collections::HashMap;
use std::sync::RwLock;

/// In-memory implementation of all Signal stores for testing.
pub struct InMemorySignalStores {
    identity_key_pair: (Vec<u8>, Vec<u8>),
    registration_id: u32,
    identities: RwLock<HashMap<String, Vec<u8>>>,
    sessions: RwLock<HashMap<String, Vec<u8>>>,
    prekeys: RwLock<HashMap<u32, Vec<u8>>>,
    signed_prekeys: RwLock<HashMap<u32, Vec<u8>>>,
    sender_keys: RwLock<HashMap<(String, Vec<u8>), Vec<u8>>>,
}

impl InMemorySignalStores {
    /// Create new in-memory stores with generated identity.
    pub fn new() -> Self {
        let mut private_key = vec![0u8; 32];
        let mut public_key = vec![0u8; 33];
        rand::RngCore::fill_bytes(&mut rand::rng(), &mut private_key);
        public_key[0] = 0x05; // DJB key type
        rand::RngCore::fill_bytes(&mut rand::rng(), &mut public_key[1..]);

        Self {
            identity_key_pair: (private_key, public_key),
            registration_id: rand::random::<u32>() % 16380 + 1,
            identities: RwLock::new(HashMap::new()),
            sessions: RwLock::new(HashMap::new()),
            prekeys: RwLock::new(HashMap::new()),
            signed_prekeys: RwLock::new(HashMap::new()),
            sender_keys: RwLock::new(HashMap::new()),
        }
    }

    /// Create with deterministic identity for reproducible tests.
    pub fn deterministic(seed: u64) -> Self {
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::seed_from_u64(seed);

        let mut private_key = vec![0u8; 32];
        let mut public_key = vec![0u8; 33];
        rand::RngCore::fill_bytes(&mut rng, &mut private_key);
        public_key[0] = 0x05;
        rand::RngCore::fill_bytes(&mut rng, &mut public_key[1..]);

        Self {
            identity_key_pair: (private_key, public_key),
            registration_id: (seed % 16380 + 1) as u32,
            identities: RwLock::new(HashMap::new()),
            sessions: RwLock::new(HashMap::new()),
            prekeys: RwLock::new(HashMap::new()),
            signed_prekeys: RwLock::new(HashMap::new()),
            sender_keys: RwLock::new(HashMap::new()),
        }
    }

    /// Get identity key pair.
    pub fn identity_key_pair(&self) -> &(Vec<u8>, Vec<u8>) {
        &self.identity_key_pair
    }

    /// Get registration ID.
    pub fn registration_id(&self) -> u32 {
        self.registration_id
    }

    /// Store a session.
    pub fn store_session(&self, address: &str, record: Vec<u8>) {
        self.sessions.write().unwrap().insert(address.to_string(), record);
    }

    /// Load a session.
    pub fn load_session(&self, address: &str) -> Option<Vec<u8>> {
        self.sessions.read().unwrap().get(address).cloned()
    }

    /// Check if session exists.
    pub fn has_session(&self, address: &str) -> bool {
        self.sessions.read().unwrap().contains_key(address)
    }

    /// Clear all sessions.
    pub fn clear_sessions(&self) {
        self.sessions.write().unwrap().clear();
    }

    /// Store identity.
    pub fn store_identity(&self, address: &str, identity: Vec<u8>) {
        self.identities.write().unwrap().insert(address.to_string(), identity);
    }

    /// Load identity.
    pub fn load_identity(&self, address: &str) -> Option<Vec<u8>> {
        self.identities.read().unwrap().get(address).cloned()
    }
}

impl Default for InMemorySignalStores {
    fn default() -> Self {
        Self::new()
    }
}
