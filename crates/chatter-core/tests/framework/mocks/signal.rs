//! In-memory Signal Protocol stores.

use std::collections::HashMap;
use std::sync::RwLock;

/// In-memory implementation of all Signal stores for testing.
pub struct InMemorySignalStores {
    identity_key_pair: (Vec<u8>, Vec<u8>),
    registration_id: u32,
    identities: RwLock<HashMap<String, Vec<u8>>>,
    sessions: RwLock<HashMap<String, Vec<u8>>>,
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
