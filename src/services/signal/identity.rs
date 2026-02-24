//! Identity and key management operations.
//!
//! Handles identity fingerprints, prekey refresh, and call encryption key derivation.

use rand::SeedableRng;
use libsignal_protocol::{ProtocolAddress, IdentityKeyStore};

use crate::crypto::SignalStore;
use crate::network::grpc::auth_client::AuthClient;
use crate::error::{CryptoError, ServiceError};

use super::SignalService;

impl SignalService {
    /// Get the local identity key fingerprint.
    ///
    /// Returns the public key formatted as a hex fingerprint string
    /// with 4-byte chunks separated by spaces (e.g., "6BB8D7AC EF0B47A2 ...").
    pub fn get_identity_fingerprint(&self) -> Result<String, ServiceError> {
        use crate::crypto::signal::IdentityKeyManager;

        let identity_manager = IdentityKeyManager::new(self.key_store.clone());
        let public_key = identity_manager
            .get_public_key()
            .map_err(ServiceError::Crypto)?;

        let bytes = public_key.serialize();
        let fingerprint = bytes
            .chunks(4)
            .map(|chunk| chunk.iter().map(|b| format!("{:02X}", b)).collect::<String>())
            .collect::<Vec<_>>()
            .join(" ");

        Ok(fingerprint)
    }

    /// Generate and upload new one-time prekeys.
    ///
    /// Generates `count` new prekeys and uploads them to the server.
    /// Returns the number of keys successfully uploaded.
    pub async fn refresh_prekeys(
        &self,
        auth_client: &mut AuthClient,
        count: u32,
    ) -> Result<u32, ServiceError> {
        use crate::crypto::signal::PreKeyManager;
        use crate::proto::chat_v1::signal_pre_key_bundle::PreKeyRecord as ProtoPreKeyRecord;

        let prekey_manager = PreKeyManager::new(self.key_store.clone());
        let mut rng = rand::rngs::StdRng::from_os_rng();

        // Get the next prekey ID to start from
        let start_id = prekey_manager
            .get_next_prekey_id()
            .map_err(ServiceError::Crypto)?;

        // Generate prekeys
        let prekeys = prekey_manager
            .generate_prekeys(&mut rng, start_id, count)
            .map_err(ServiceError::Crypto)?;

        // Convert to proto format
        let proto_prekeys: Vec<ProtoPreKeyRecord> = prekeys
            .iter()
            .filter_map(|pk| {
                Some(ProtoPreKeyRecord {
                    id: u32::from(pk.id().ok()?),
                    public_key: pk.public_key().ok()?.serialize().to_vec(),
                })
            })
            .collect();

        let uploaded_count = proto_prekeys.len() as u32;

        // Upload to server
        auth_client
            .refresh_pre_keys(proto_prekeys)
            .await
            .map_err(ServiceError::Network)?;

        tracing::info!(
            count = uploaded_count,
            start_id = start_id,
            "Refreshed prekeys"
        );

        Ok(uploaded_count)
    }

    /// Derive a shared secret for call encryption with a peer.
    ///
    /// Uses ECDH between our identity key and peer's identity key,
    /// then derives a call-specific key. This is separate from the
    /// Double Ratchet session - it's a one-time derivation for calls.
    ///
    /// # Arguments
    /// * `peer_address` - The peer's Signal protocol address
    ///
    /// # Returns
    /// 32-byte key for LiveKit E2EE.
    pub async fn derive_call_encryption_key(
        &self,
        peer_address: &ProtocolAddress,
    ) -> Result<[u8; 32], ServiceError> {
        use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret as X25519PrivateKey};

        // Get our identity key pair
        let identity_store = SignalStore::new(self.key_store.clone());
        let our_identity = identity_store
            .get_identity_key_pair()
            .await
            .map_err(|e| CryptoError::Protocol(e.to_string()))?;

        // Get peer's identity key
        let peer_identity = identity_store
            .get_identity(peer_address)
            .await
            .map_err(|e| CryptoError::Protocol(e.to_string()))?
            .ok_or_else(|| CryptoError::SessionNotFound(peer_address.to_string()))?;

        // Extract raw key bytes
        // libsignal PublicKey.serialize() = 0x05 || 32-byte Curve25519 point
        // libsignal PrivateKey.serialize() = 32-byte Curve25519 scalar
        let our_private_bytes = our_identity.private_key().serialize();
        let peer_public_bytes = peer_identity.public_key().serialize();

        // Validate key sizes
        if our_private_bytes.len() != 32 {
            return Err(CryptoError::InvalidKeyLength {
                expected: 32,
                got: our_private_bytes.len(),
            }.into());
        }
        if peer_public_bytes.len() != 33 {
            return Err(CryptoError::InvalidKeyLength {
                expected: 33,
                got: peer_public_bytes.len(),
            }.into());
        }

        // Convert to x25519-dalek types
        let mut our_scalar = [0u8; 32];
        our_scalar.copy_from_slice(&our_private_bytes);
        let our_x25519_private = X25519PrivateKey::from(our_scalar);

        // Skip the 0x05 type prefix from peer's public key
        let mut peer_point = [0u8; 32];
        peer_point.copy_from_slice(&peer_public_bytes[1..33]);
        let peer_x25519_public = X25519PublicKey::from(peer_point);

        // Perform ECDH
        let shared_secret = our_x25519_private.diffie_hellman(&peer_x25519_public);

        tracing::debug!(
            peer = %peer_address,
            "Derived call encryption shared secret"
        );

        Ok(*shared_secret.as_bytes())
    }

    /// Derive a call key for a specific call with a peer.
    ///
    /// Combines `derive_call_encryption_key` with call-specific key derivation.
    ///
    /// # Arguments
    /// * `peer_address` - The peer's Signal protocol address
    /// * `call_id` - Unique call identifier
    ///
    /// # Returns
    /// 32-byte AES-256 key for LiveKit E2EE.
    pub async fn derive_call_key(
        &self,
        peer_address: &ProtocolAddress,
        call_id: &str,
    ) -> Result<[u8; 32], ServiceError> {
        use crate::crypto::call_key_provider;

        let shared_secret = self.derive_call_encryption_key(peer_address).await?;
        let call_key = call_key_provider::derive_call_key(&shared_secret, call_id)
            .map_err(ServiceError::Crypto)?;

        tracing::info!(
            peer = %peer_address,
            call_id = call_id,
            "Derived call encryption key"
        );

        Ok(call_key)
    }
}
