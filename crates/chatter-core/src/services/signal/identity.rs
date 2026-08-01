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

    /// Generate and upload new one-time prekeys (classical OPK + PQOPK in
    /// lockstep). Per Signal PQXDH §3.2 a PQOPK is consumed every time a
    /// classical OPK is, so the two pools must refill together — failing to
    /// upload PQOPKs would force the server to fall back to the medium-term
    /// last-resort PQSPK and weaken PQ forward secrecy.
    pub async fn refresh_prekeys(
        &self,
        auth_client: &mut AuthClient,
        count: u32,
    ) -> Result<u32, ServiceError> {
        let proto_prekeys = self.actor.refresh_prekeys(count).await?;
        let proto_kyber_otpks = self.actor.refresh_kyber_prekeys(count).await?;
        let uploaded_count = proto_prekeys.len() as u32;
        let kyber_count = proto_kyber_otpks.len() as u32;

        auth_client
            .refresh_pre_keys_with_kyber_otpk(proto_prekeys, proto_kyber_otpks)
            .await
            .map_err(ServiceError::Network)?;

        tracing::info!(opk = uploaded_count, pq_opk = kyber_count, "Refreshed prekeys via Actor");

        Ok(uploaded_count)
    }

    /// Rotate signed prekey + Kyber prekey and upload to server.
    ///
    /// Generates a new signed prekey and Kyber prekey (PQXDH pairing),
    /// uploads them via the RefreshPreKeysRequest, and prunes expired
    /// signed prekeys older than 30 days.
    pub async fn rotate_signed_prekey(
        &self,
        auth_client: &mut AuthClient,
    ) -> Result<(), ServiceError> {
        let (spk_proto, kpk_proto) = self.actor.rotate_signed_prekey().await?;

        auth_client
            .refresh_pre_keys_with_rotation(Vec::new(), Some(spk_proto), Some(kpk_proto), Vec::new())
            .await
            .map_err(ServiceError::Network)?;

        // Prune signed + Kyber prekeys older than 30 days (retain for in-flight
        // sessions). Both are rotated together as the PQXDH pair; both must be
        // pruned together to avoid unbounded Kyber key-store growth.
        use crate::crypto::signal::PreKeyManager;
        let prekey_manager = PreKeyManager::new(self.key_store.clone());
        let pruned_spk = prekey_manager.cleanup_expired_signed_prekeys(30)
            .map_err(ServiceError::Crypto)?;
        if pruned_spk > 0 {
            tracing::info!(count = pruned_spk, "Pruned expired signed prekeys");
        }
        let pruned_kpk = prekey_manager.cleanup_expired_kyber_prekeys(30)
            .map_err(ServiceError::Crypto)?;
        if pruned_kpk > 0 {
            tracing::info!(count = pruned_kpk, "Pruned expired Kyber prekeys");
        }

        tracing::info!("Rotated signed prekey + Kyber prekey (PQXDH pair)");
        Ok(())
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

    /// Accept a peer's NEW identity key after their key changed, and unwedge the
    /// conversation.
    ///
    /// Reached from [`crate::error::CryptoError::UntrustedIdentity`], which is
    /// raised whenever a peer presents a key other than the one pinned for their
    /// account — normally a reinstall or device wipe. Until this runs, the pinned
    /// key stays the old one: every inbound PreKey message from the peer fails
    /// libsignal's `is_trusted_identity` and every fresh session with them is
    /// refused, and nothing in the protocol can clear that by itself.
    ///
    /// **This is a trust decision and must only be reached from an explicit user
    /// action.** An identity change is also exactly what an active MITM looks like;
    /// the client cannot tell the two apart, which is why it is not automatic.
    /// Accordingly this drops the peer's verified flag — the safety number the user
    /// checked belongs to the old key and says nothing about this one, so the peer
    /// must be re-verified.
    ///
    /// Every session with the peer is then deleted (not archived): the old ratchet
    /// states were negotiated against the old identity key, so nothing encrypted
    /// under them can be trusted, and archived states would only keep failing
    /// `is_trusted_identity` on the way in. With no session left,
    /// [`super::SignalService::ensure_session`] fetches a fresh prekey bundle and
    /// the next send re-runs PQXDH against the accepted key.
    ///
    /// Returns the number of sessions dropped. A no-op (`Ok(0)`) when the key is
    /// already the pinned one, so a duplicate accept cannot silently clear the
    /// peer's verification.
    pub fn accept_peer_identity_change(
        &self,
        peer_user_id: &str,
        new_identity_key: &[u8],
    ) -> Result<u32, ServiceError> {
        // Reject anything that is not a well-formed Signal identity key BEFORE it
        // is pinned. A malformed key stored here fails to parse in
        // `is_trusted_identity` and turns every later message from the peer into a
        // decode error — a worse wedge than the one being cleared.
        libsignal_protocol::IdentityKey::decode(new_identity_key).map_err(|e| {
            ServiceError::Crypto(crate::error::CryptoError::InvalidKey(format!(
                "not a Signal identity key: {e}"
            )))
        })?;

        // Identity keys are pinned per ACCOUNT, so the store address is the bare
        // user id (see `crypto::signal::store::SignalStore::save_identity`).
        if self.key_store.get_identity(peer_user_id).map_err(ServiceError::Storage)?
            .as_deref()
            == Some(new_identity_key)
        {
            tracing::debug!(
                peer = %peer_user_id,
                "Identity change accepted for a key that is already pinned — nothing to do"
            );
            return Ok(0);
        }

        self.key_store
            .save_identity(peer_user_id, new_identity_key)
            .map_err(ServiceError::Storage)?;
        self.key_store
            .unverify_peer(peer_user_id)
            .map_err(ServiceError::Storage)?;
        let dropped = self
            .key_store
            .delete_sessions_for_user(peer_user_id)
            .map_err(ServiceError::Storage)?;

        tracing::warn!(
            peer = %peer_user_id,
            sessions_dropped = dropped,
            "ACCEPTED a peer identity key change: new key pinned, verification cleared, \
             all sessions dropped — next send re-establishes PQXDH against the new key"
        );

        Ok(dropped)
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
