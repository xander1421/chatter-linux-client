//! PreKey bundle management.
//!
//! PreKey generation, storage, and bundle creation for PQXDH key exchange.

use libsignal_protocol::{
    kem, DeviceId, GenericSignedPreKey, IdentityKey, IdentityKeyPair, KeyPair, KyberPreKeyId,
    KyberPreKeyRecord, PreKeyBundle, PreKeyId, PreKeyRecord, SignedPreKeyId, SignedPreKeyRecord,
    Timestamp,
};
use rand::{CryptoRng, Rng};
use std::sync::Arc;
use std::time::SystemTime;

use crate::error::CryptoError;
use crate::storage::KeyStore;

/// PreKey manager for PQXDH key agreement.
///
/// Handles generation, storage, and bundle creation of prekeys for
/// Signal Protocol session establishment. Supports:
/// - One-time prekeys (consumed on first use)
/// - Signed prekeys (rotated periodically)
/// - Kyber prekeys (post-quantum resistance)
#[derive(Clone)]
pub struct PreKeyManager {
    key_store: Arc<KeyStore>,
}

impl PreKeyManager {
    /// Create a new PreKeyManager with the given key store.
    pub fn new(key_store: Arc<KeyStore>) -> Self {
        Self { key_store }
    }

    /// Generate a one-time prekey.
    ///
    /// One-time prekeys are consumed when a session is established.
    /// Returns the generated PreKeyRecord which should also be uploaded
    /// to the server.
    pub fn generate_prekey<R: CryptoRng + Rng>(
        &self,
        rng: &mut R,
        id: u32,
    ) -> Result<PreKeyRecord, CryptoError> {
        // Generate the EC key pair
        let key_pair = KeyPair::generate(rng);

        // Create the record
        let record = PreKeyRecord::new(PreKeyId::from(id), &key_pair);

        // Store in database
        self.key_store
            .save_prekey(id, &record.serialize().map_err(|e| CryptoError::Protocol(e.to_string()))?)
            .map_err(CryptoError::Storage)?;

        Ok(record)
    }

    /// Generate multiple one-time prekeys.
    ///
    /// Generates `count` prekeys starting from `start_id`.
    /// Returns all generated records for server upload.
    pub fn generate_prekeys<R: CryptoRng + Rng>(
        &self,
        rng: &mut R,
        start_id: u32,
        count: u32,
    ) -> Result<Vec<PreKeyRecord>, CryptoError> {
        let mut records = Vec::with_capacity(count as usize);

        for i in 0..count {
            let record = self.generate_prekey(rng, start_id + i)?;
            records.push(record);
        }

        Ok(records)
    }

    /// Generate a signed prekey.
    ///
    /// Signed prekeys are long-lived and signed with the identity key.
    /// They should be rotated periodically (e.g., every 30 days).
    pub fn generate_signed_prekey<R: CryptoRng + Rng>(
        &self,
        rng: &mut R,
        id: u32,
        identity_key_pair: &IdentityKeyPair,
    ) -> Result<SignedPreKeyRecord, CryptoError> {
        // Generate EC key pair for the signed prekey
        let key_pair = KeyPair::generate(rng);

        // Sign the public key with our identity key
        let signature = identity_key_pair
            .private_key()
            .calculate_signature(&key_pair.public_key.serialize(), rng)
            .map_err(|e| CryptoError::Protocol(e.to_string()))?;

        // Calculate timestamp
        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("Time should move forward")
            .as_millis();

        // Create the record using the GenericSignedPreKey trait
        let record = SignedPreKeyRecord::new(
            SignedPreKeyId::from(id),
            Timestamp::from_epoch_millis(timestamp as u64),
            &key_pair,
            &signature.into_vec(),
        );

        // Store in database
        self.key_store
            .save_signed_prekey(
                id,
                &record.serialize().map_err(|e| CryptoError::Protocol(e.to_string()))?,
            )
            .map_err(CryptoError::Storage)?;

        Ok(record)
    }

    /// Generate a Kyber prekey (post-quantum).
    ///
    /// Kyber prekeys provide post-quantum resistance using ML-KEM (Kyber1024).
    /// They are signed with the identity key and are required for PQXDH.
    pub fn generate_kyber_prekey(
        &self,
        id: u32,
        identity_key_pair: &IdentityKeyPair,
    ) -> Result<KyberPreKeyRecord, CryptoError> {
        // Generate Kyber key pair using libsignal's built-in method
        // This uses OsRng internally and signs with the identity key
        let record =
            KyberPreKeyRecord::generate(kem::KeyType::Kyber1024, KyberPreKeyId::from(id), identity_key_pair.private_key())
                .map_err(|e| CryptoError::Protocol(e.to_string()))?;

        // Store in database
        self.key_store
            .save_kyber_prekey(
                id,
                &record.serialize().map_err(|e| CryptoError::Protocol(e.to_string()))?,
            )
            .map_err(CryptoError::Storage)?;

        Ok(record)
    }

    /// Create a prekey bundle for upload to the server.
    ///
    /// A prekey bundle contains all the public key material needed for
    /// another device to establish a session with us using PQXDH.
    ///
    /// # Arguments
    /// * `registration_id` - Our device's registration ID (14-bit, 1-16383)
    /// * `device_id` - Our device ID
    /// * `prekey` - One-time prekey (optional but recommended)
    /// * `signed_prekey` - Signed prekey (required)
    /// * `kyber_prekey` - Kyber prekey (required for PQXDH)
    /// * `identity_key` - Our identity public key
    pub fn create_prekey_bundle(
        &self,
        registration_id: u32,
        device_id: u32,
        prekey: Option<&PreKeyRecord>,
        signed_prekey: &SignedPreKeyRecord,
        kyber_prekey: &KyberPreKeyRecord,
        identity_key: IdentityKey,
    ) -> Result<PreKeyBundle, CryptoError> {
        // Extract prekey info if present
        let pre_key_option = match prekey {
            Some(pk) => {
                let id = pk.id().map_err(|e| CryptoError::Protocol(e.to_string()))?;
                let public_key = pk.public_key().map_err(|e| CryptoError::Protocol(e.to_string()))?;
                Some((id, public_key))
            }
            None => None,
        };

        // Extract signed prekey info
        let signed_id = signed_prekey
            .id()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?;
        let signed_public = signed_prekey
            .public_key()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?;
        let signed_signature = signed_prekey
            .signature()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?;

        // Extract Kyber prekey info
        let kyber_id = kyber_prekey
            .id()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?;
        let kyber_public = kyber_prekey
            .public_key()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?;
        let kyber_signature = kyber_prekey
            .signature()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?;

        // Create DeviceId (1-127 range)
        let device = DeviceId::try_from(device_id)
            .map_err(|e| CryptoError::Protocol(format!("invalid device_id: {}", e)))?;

        // Create the bundle
        PreKeyBundle::new(
            registration_id,
            device,
            pre_key_option,
            signed_id,
            signed_public,
            signed_signature,
            kyber_id,
            kyber_public,
            kyber_signature,
            identity_key,
        )
        .map_err(|e| CryptoError::Protocol(e.to_string()))
    }

    /// Get the count of available one-time prekeys.
    pub fn get_prekey_count(&self) -> Result<u32, CryptoError> {
        self.key_store
            .count_prekeys()
            .map_err(CryptoError::Storage)
    }

    /// Get the next available prekey ID.
    ///
    /// Returns the maximum existing prekey ID + 1, or 1 if no prekeys exist.
    pub fn get_next_prekey_id(&self) -> Result<u32, CryptoError> {
        let max_id = self
            .key_store
            .max_prekey_id()
            .map_err(CryptoError::Storage)?;
        Ok(max_id + 1)
    }

    /// Check if prekey refresh is needed.
    ///
    /// Returns `true` if the number of available prekeys is below the threshold.
    /// Signal recommends maintaining at least 100 prekeys.
    pub fn needs_prekey_refresh(&self, threshold: u32) -> Result<bool, CryptoError> {
        let count = self.get_prekey_count()?;
        Ok(count < threshold)
    }

    /// Get a signed prekey by ID.
    pub fn get_signed_prekey(&self, id: u32) -> Result<Option<SignedPreKeyRecord>, CryptoError> {
        let bytes = self
            .key_store
            .get_signed_prekey(id)
            .map_err(CryptoError::Storage)?;

        match bytes {
            Some(data) => {
                let record = SignedPreKeyRecord::deserialize(&data)
                    .map_err(|e| CryptoError::Protocol(e.to_string()))?;
                Ok(Some(record))
            }
            None => Ok(None),
        }
    }

    /// Get a Kyber prekey by ID.
    pub fn get_kyber_prekey(&self, id: u32) -> Result<Option<KyberPreKeyRecord>, CryptoError> {
        let bytes = self
            .key_store
            .get_kyber_prekey(id)
            .map_err(CryptoError::Storage)?;

        match bytes {
            Some(data) => {
                let record = KyberPreKeyRecord::deserialize(&data)
                    .map_err(|e| CryptoError::Protocol(e.to_string()))?;
                Ok(Some(record))
            }
            None => Ok(None),
        }
    }

    /// Get a one-time prekey by ID.
    pub fn get_prekey(&self, id: u32) -> Result<Option<PreKeyRecord>, CryptoError> {
        let bytes = self
            .key_store
            .get_prekey(id)
            .map_err(CryptoError::Storage)?;

        match bytes {
            Some(data) => {
                let record = PreKeyRecord::deserialize(&data)
                    .map_err(|e| CryptoError::Protocol(e.to_string()))?;
                Ok(Some(record))
            }
            None => Ok(None),
        }
    }
}
