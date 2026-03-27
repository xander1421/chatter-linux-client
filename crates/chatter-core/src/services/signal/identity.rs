//! Identity and key management operations.
//!
//! Handles identity fingerprints, prekey refresh, and call encryption key derivation.
//! All operations are delegated to the SignalActor for thread-safety.

use libsignal_protocol::ProtocolAddress;
use crate::network::grpc::auth_client::AuthClient;
use crate::error::ServiceError;
use super::SignalService;

impl SignalService {
    /// Get the local identity key fingerprint.
    pub async fn get_identity_fingerprint(&self) -> Result<String, ServiceError> {
        self.actor.get_identity_fingerprint().await
    }

    /// Get the local identity public key (33 bytes - 0x05 + 32-byte Curve25519).
    ///
    /// This is the Signal Protocol format identity key with the type byte prefix.
    pub async fn get_identity_public_key(&self) -> Result<[u8; 33], ServiceError> {
        self.actor.get_identity_public_key().await
    }

    /// Get the local identity private key (32 bytes X25519 scalar).
    ///
    /// This is used for ECIES decryption of NDSS'21 initial contact messages.
    pub async fn get_identity_private_key(&self) -> Result<[u8; 32], ServiceError> {
        self.actor.get_identity_private_key().await
    }

    /// Check if we have a trusted identity key for a peer.
    ///
    /// Returns `Some(key_bytes)` if trusted, `None` if unknown peer.
    pub fn get_peer_identity(&self, peer_id: &str) -> Result<Option<Vec<u8>>, crate::error::StorageError> {
        self.key_store.get_identity_by_uuid(peer_id)
    }

    /// Generate and upload new one-time prekeys.
    pub async fn refresh_prekeys(
        &self,
        auth_client: &mut AuthClient,
        count: u32,
    ) -> Result<u32, ServiceError> {
        let proto_prekeys = self.actor.refresh_prekeys(count).await?;
        let uploaded_count = proto_prekeys.len() as u32;

        // Upload to server
        auth_client
            .refresh_pre_keys(proto_prekeys)
            .await
            .map_err(ServiceError::Network)?;

        tracing::info!(count = uploaded_count, "Refreshed prekeys via Actor");

        Ok(uploaded_count)
    }

    /// Derive a shared secret for call encryption with a peer.
    pub async fn derive_call_encryption_key(
        &self,
        peer_address: &ProtocolAddress,
    ) -> Result<[u8; 32], ServiceError> {
        let peer_id = peer_address.name().parse().map_err(|_| ServiceError::Validation("Invalid peer ID".into()))?;
        let device_id = peer_address.device_id().into();
        
        self.actor.derive_call_encryption_key(peer_id, device_id).await
    }

    /// Generate a 60-digit safety number for a peer.
    ///
    /// Returns a numeric string that both parties should compare to verify
    /// they share the same identity keys and are not subject to MITM.
    pub async fn generate_safety_number(
        &self,
        local_user_id: &str,
        peer_user_id: &str,
    ) -> Result<String, ServiceError> {
        self.actor
            .generate_safety_number(local_user_id.to_string(), peer_user_id.to_string())
            .await
    }

    /// Generate scannable fingerprint bytes for QR code verification.
    pub async fn generate_scannable_fingerprint(
        &self,
        local_user_id: &str,
        peer_user_id: &str,
    ) -> Result<Vec<u8>, ServiceError> {
        self.actor
            .generate_scannable_fingerprint(local_user_id.to_string(), peer_user_id.to_string())
            .await
    }

    /// Check if we have previously verified a peer's safety number.
    pub fn is_peer_verified(&self, peer_user_id: &str) -> Result<bool, ServiceError> {
        self.key_store
            .is_peer_verified(peer_user_id)
            .map_err(ServiceError::Storage)
    }

    /// Mark a peer as verified (or unverified) with their current safety number.
    pub fn set_peer_verified(
        &self,
        peer_user_id: &str,
        safety_number: &str,
        verified: bool,
    ) -> Result<(), ServiceError> {
        self.key_store
            .set_peer_verified(peer_user_id, safety_number, verified)
            .map_err(ServiceError::Storage)
    }

    /// Derive a call key for a specific call with a peer.
    pub async fn derive_call_key(
        &self,
        peer_address: &ProtocolAddress,
        call_id: &str,
    ) -> Result<[u8; 32], ServiceError> {
        use crate::crypto::call_key_provider;

        let shared_secret = self.derive_call_encryption_key(peer_address).await?;
        let call_key = call_key_provider::derive_call_key(&shared_secret, call_id)
            .map_err(ServiceError::Crypto)?;

        Ok(call_key)
    }
}
