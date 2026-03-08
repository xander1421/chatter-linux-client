//! Pairwise encryption service.
//!
//! Provides reusable Signal Protocol pairwise encryption that can be used by:
//! - DMs (with additional mailbox encryption layer)
//! - Anonymous groups (direct pairwise to anonymous mailboxes)
//! - Any future pairwise messaging context
//!
//! # Separation of Concerns
//!
//! This service handles ONLY encryption/decryption using Signal Protocol.
//! Delivery and mailbox management are handled by the calling context:
//! - DM: `DmConversationManager` + two-layer encryption
//! - AnonGroup: Anonymous mailbox per group
//! - ServerGroup: Uses sender keys (different service)

use std::collections::HashMap;
use std::sync::Arc;

use crate::domain::UserId;
use crate::error::NetworkError;
use crate::mailbox::token_manager::MailboxToken;
use crate::network::grpc::anonymous_client::AnonymousAuthClient;
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::services::signal_service::SignalService;

/// Errors from pairwise encryption operations.
#[derive(Debug, thiserror::Error)]
pub enum PairwiseError {
    /// Session establishment failed.
    #[error("session error: {0}")]
    SessionError(String),

    /// Encryption failed.
    #[error("encryption failed: {0}")]
    EncryptionFailed(String),

    /// Decryption failed.
    #[error("decryption failed: {0}")]
    DecryptionFailed(String),

    /// Network error during key fetch.
    #[error("network error: {0}")]
    Network(#[from] NetworkError),

    /// No cached sender certificate (required for anonymous encryption).
    #[error("no cached sender certificate - ensure pre-fetch during login")]
    NoCachedCertificate,
}

/// Reusable pairwise encryption service using Signal Protocol.
///
/// Handles Signal Protocol session management and Double Ratchet
/// encryption without any delivery-specific logic.
#[derive(Clone)]
pub struct PairwiseService {
    signal_service: Arc<SignalService>,
    grpc_pool: Arc<GrpcClientPool>,
}

impl PairwiseService {
    /// Create a new PairwiseService.
    pub fn new(signal_service: Arc<SignalService>, grpc_pool: Arc<GrpcClientPool>) -> Self {
        Self {
            signal_service,
            grpc_pool,
        }
    }

    /// Create an auth client for key fetching.
    pub fn auth_client(&self) -> AuthClient {
        AuthClient::new(self.grpc_pool.channel(), self.grpc_pool.auth_clone())
    }

    /// Encrypt plaintext for a peer using Signal Protocol (Sealed Sender).
    ///
    /// Establishes a session if one doesn't exist (fetches prekey bundle).
    /// Uses Double Ratchet for forward secrecy and Sealed Sender for
    /// sender anonymity from the server.
    ///
    /// # Arguments
    /// * `peer_id` - The recipient's user ID
    /// * `plaintext` - The data to encrypt
    ///
    /// # Returns
    /// The sealed sender ciphertext (Signal Protocol encrypted).
    pub async fn encrypt(&self, peer_id: &UserId, plaintext: &[u8]) -> Result<Vec<u8>, PairwiseError> {
        let mut auth_client = self.auth_client();
        self.encrypt_with_client(&mut auth_client, peer_id, plaintext).await
    }

    /// Encrypt with a provided auth client (for batching).
    pub async fn encrypt_with_client(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, PairwiseError> {
        let peer_id_str = peer_id.to_string();
        let device_id = 1u32; // Primary device

        self.signal_service
            .encrypt_message(auth_client, &peer_id_str, device_id, plaintext)
            .await
            .map_err(|e| PairwiseError::EncryptionFailed(e.to_string()))
    }

    /// Decrypt sealed sender ciphertext from any peer.
    ///
    /// # Arguments
    /// * `ciphertext` - The sealed sender ciphertext
    ///
    /// # Returns
    /// Tuple of (sender_id, decrypted_plaintext).
    pub async fn decrypt_sealed_sender(
        &self,
        ciphertext: &[u8],
    ) -> Result<(String, Vec<u8>), PairwiseError> {
        let (address, plaintext) = self.signal_service
            .decrypt_message(ciphertext)
            .await
            .map_err(|e| PairwiseError::DecryptionFailed(e.to_string()))?;

        Ok((address.name().to_string(), plaintext))
    }

    /// Ensure a session exists for a peer (pre-fetch keys).
    ///
    /// Call this before encryption if you want to warm up sessions
    /// (e.g., when a user opens a conversation).
    pub async fn ensure_session(&self, peer_id: &UserId) -> Result<(), PairwiseError> {
        let mut auth_client = self.auth_client();
        let peer_id_str = peer_id.to_string();
        let device_id = 1u32;

        self.signal_service
            .ensure_session(&mut auth_client, &peer_id_str, device_id)
            .await
            .map_err(|e| PairwiseError::SessionError(e.to_string()))
    }

    // =========================================================================
    // Multi-Device Operations
    // =========================================================================

    /// Encrypt plaintext for ALL devices of a peer.
    ///
    /// Establishes sessions with all devices if needed and encrypts the
    /// plaintext for each device. Returns a map of device_id -> ciphertext.
    ///
    /// Use this method when you need to ensure the message reaches all of
    /// the recipient's devices (e.g., DM messages).
    ///
    /// # Arguments
    /// * `peer_id` - The recipient's user ID
    /// * `plaintext` - The data to encrypt
    ///
    /// # Returns
    /// A map of device_id -> sealed sender ciphertext for each device.
    /// Returns error only if ALL devices fail.
    pub async fn encrypt_all_devices(
        &self,
        peer_id: &UserId,
        plaintext: &[u8],
    ) -> Result<HashMap<u32, Vec<u8>>, PairwiseError> {
        let mut auth_client = self.auth_client();
        self.encrypt_all_devices_with_client(&mut auth_client, peer_id, plaintext).await
    }

    /// Encrypt for all devices with a provided auth client (for batching).
    pub async fn encrypt_all_devices_with_client(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
        plaintext: &[u8],
    ) -> Result<HashMap<u32, Vec<u8>>, PairwiseError> {
        let peer_id_str = peer_id.to_string();

        self.signal_service
            .encrypt_message_all_devices(auth_client, &peer_id_str, plaintext)
            .await
            .map_err(|e| PairwiseError::EncryptionFailed(e.to_string()))
    }

    /// Ensure sessions exist for all devices of a peer.
    ///
    /// Use this to pre-warm sessions before encryption.
    pub async fn ensure_sessions_all_devices(&self, peer_id: &UserId) -> Result<Vec<u32>, PairwiseError> {
        let mut auth_client = self.auth_client();
        let peer_id_str = peer_id.to_string();

        self.signal_service
            .ensure_sessions_all_devices(&mut auth_client, &peer_id_str)
            .await
            .map_err(|e| PairwiseError::SessionError(e.to_string()))
    }

    // =========================================================================
    // Anonymous Operations (for AnonDM - no authenticated requests)
    // =========================================================================

    /// Encrypt plaintext anonymously for a peer.
    ///
    /// This method fetches prekeys using an anonymous credential (blind signature)
    /// and encrypts using a cached sender certificate. NO authenticated requests
    /// are made, so the server cannot link this encryption to the sender.
    ///
    /// # Pre-conditions
    ///
    /// - Sender certificate must be cached (pre-fetched during login)
    /// - A valid blind signature token must be provided
    ///
    /// # Privacy Guarantees
    ///
    /// - Server knows: which user's prekeys are being fetched (peer_id)
    /// - Server does NOT know: who is fetching the prekeys (uses anonymous credential)
    /// - No authenticated requests are made during this operation
    pub async fn encrypt_anonymous(
        &self,
        peer_id: &UserId,
        plaintext: &[u8],
        token: &MailboxToken,
    ) -> Result<Vec<u8>, PairwiseError> {
        let peer_id_str = peer_id.to_string();
        let device_id = 1u32;

        // Fetch prekey bundle ANONYMOUSLY using blind signature token
        let mut anon_auth = AnonymousAuthClient::new(self.grpc_pool.channel());
        let bundle_response = anon_auth
            .get_pre_key_bundle_anonymous(
                &peer_id_str,
                &device_id.to_string(),
                &token.public_key,
                &token.signature,
                token.randomizer.as_deref(),
            )
            .await
            .map_err(|e| PairwiseError::SessionError(format!("Anonymous prekey fetch failed: {}", e)))?;

        // Establish session using the anonymously-fetched bundle
        self.signal_service
            .ensure_session_with_bundle(&peer_id_str, device_id, bundle_response)
            .await
            .map_err(|e| PairwiseError::SessionError(e.to_string()))?;

        // Encrypt using cached sender certificate (no auth request)
        self.signal_service
            .encrypt_message_anonymous(&peer_id_str, device_id, plaintext)
            .await
            .map_err(|e| PairwiseError::EncryptionFailed(e.to_string()))
    }
}
