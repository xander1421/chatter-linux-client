//! Signal Protocol orchestration service.
//!
//! Bridges the gap between business logic (MessageService) and crypto primitives.
//! Handles session establishment, message encryption, and decryption.

use std::collections::HashMap;
use std::sync::Arc;
use rand::SeedableRng;
use libsignal_protocol::{
    ProtocolAddress, DeviceId,
    PreKeyBundle, PublicKey, IdentityKey, PreKeyId, SignedPreKeyId, KyberPreKeyId,
    SenderCertificate, SenderKeyDistributionMessage,
    CiphertextMessage, CiphertextMessageType, PreKeySignalMessage, SignalMessage, SessionRecord,
    IdentityKeyStore,
};
use tokio::sync::Mutex;
use uuid::Uuid;


use crate::crypto::{
    SealedSenderManager, SessionManager, SignalStore,
    signal::sender_key::SenderKeyManager,
    signal::sesame::{SesameManager, SesameStore, ConvergenceResult},
};
use crate::domain::ids::{ChannelId, UserId};
use crate::proto::content_v1::{GroupEncrypted, SenderKeyDistribution};
use crate::network::grpc::auth_client::AuthClient;
use crate::storage::repositories::settings_repo::{keys, SettingsRepository};
use crate::error::{CryptoError, NetworkError, ServiceError};

use crate::storage::KeyStore;
use crate::util::time::timestamp_nanos;

/// Signal Service for end-to-end encryption.
///
/// Uses per-recipient locks to prevent race conditions when multiple messages
/// are encrypted concurrently to the same recipient. Without this, rapid sends
/// (e.g., message + typing indicator) can both read the same session counter,
/// resulting in duplicate counters and "old counter" errors on the receiving side.
pub struct SignalService {
    key_store: Arc<KeyStore>,
    settings_repo: Arc<SettingsRepository>,
    trust_root: PublicKey,
    /// Per-recipient encryption locks.
    /// Ensures only one encryption to a given recipient at a time,
    /// preventing counter collisions in the Double Ratchet.
    encrypt_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl Clone for SignalService {
    fn clone(&self) -> Self {
        Self {
            key_store: self.key_store.clone(),
            settings_repo: self.settings_repo.clone(),
            trust_root: self.trust_root,
            encrypt_locks: self.encrypt_locks.clone(),
        }
    }
}

impl SignalService {
    /// Create a new SignalService.
    pub fn new(
        key_store: Arc<KeyStore>,
        settings_repo: Arc<SettingsRepository>,
    ) -> Self {
        let trust_root = *crate::config::TRUST_ROOT_PUBLIC_KEY;

        Self {
            key_store,
            settings_repo,
            trust_root,
            encrypt_locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Get or create a lock for encrypting to a specific recipient.
    ///
    /// This ensures only one encryption operation to a given recipient
    /// happens at a time, preventing Double Ratchet counter collisions.
    async fn get_encrypt_lock(&self, recipient_key: &str) -> Arc<Mutex<()>> {
        let mut locks = self.encrypt_locks.lock().await;
        locks
            .entry(recipient_key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Establish a session with a remote recipient if one doesn't exist.
    ///
    /// Fetches the recipient's prekey bundle from the server and processes it.
    pub async fn ensure_session(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
        device_id: u32,
    ) -> Result<(), ServiceError> {
        tracing::debug!(recipient_id, device_id, "Signal: ensure_session starting");
        let device_id_struct = DeviceId::try_from(device_id).map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
        let address = ProtocolAddress::new(recipient_id.to_string(), device_id_struct);

        tracing::debug!(recipient_id, device_id, "Signal: checking if session exists");
        if SessionManager::has_session(&SignalStore::new(self.key_store.clone()), &address).await.map_err(CryptoError::from)? {
            tracing::debug!(recipient_id, device_id, "Signal: session already exists");
            return Ok(());
        }

        tracing::debug!(recipient_id, device_id, "Signal: no session, fetching prekey bundle from server");
        // Fetch prekey bundle from server
        let bundle_response = auth_client
            .get_pre_key_bundle(recipient_id, &device_id.to_string())
            .await
            .map_err(|e| {
                tracing::error!(recipient_id, device_id, error = %e, "Signal: failed to fetch prekey bundle");
                NetworkError::from(e)
            })?;
        tracing::debug!(recipient_id, device_id, "Signal: prekey bundle received");

        let result = bundle_response.result.ok_or(ServiceError::Validation("Missing bundle in response".into()))?;
        let proto_bundle = match result {
            crate::proto::chat_v1::get_pre_key_bundle_response::Result::Bundle(b) => b,
            _ => return Err(ServiceError::Validation("Unexpected response type".into())),
        };
        let bundle = Self::convert_prekey_bundle(proto_bundle)?;
        tracing::debug!(recipient_id, device_id, "Signal: prekey bundle converted");

        // Process bundle to establish session
        let mut rng = rand::rngs::StdRng::from_os_rng();
        let mut session_store = SignalStore::new(self.key_store.clone());
        let mut identity_store = SignalStore::new(self.key_store.clone());

        tracing::debug!(recipient_id, device_id, "Signal: establishing session");
        SessionManager::establish_session(
            &address,
            &mut session_store,
            &mut identity_store,
            &bundle,
            &mut rng,
        )
        .await
        .map_err(|e| {
            tracing::error!(recipient_id, device_id, error = ?e, "Signal: failed to establish session");
            CryptoError::from(e)
        })?;

        tracing::debug!(recipient_id, device_id, "Signal: session established successfully");
        Ok(())
    }

    /// Encrypt a message for a specific recipient device using Sealed Sender.
    ///
    /// Returns the encrypted ciphertext bytes.

    pub async fn encrypt_message(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
        device_id: u32,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, ServiceError> {
        tracing::debug!(recipient_id, device_id, plaintext_len = plaintext.len(), "Signal: encrypt_message starting");

        // Acquire per-recipient lock to prevent counter collisions.
        // Without this, concurrent encryptions (e.g., message + typing indicator)
        // can both read the same session counter, causing "old counter" errors.
        let lock_key = format!("{}:{}", recipient_id, device_id);
        let recipient_lock = self.get_encrypt_lock(&lock_key).await;
        let _guard = recipient_lock.lock().await;

        tracing::debug!(recipient_id, device_id, "Signal: acquired encrypt lock");

        // Ensure session exists
        self.ensure_session(auth_client, recipient_id, device_id).await?;
        tracing::debug!(recipient_id, device_id, "Signal: session ensured");

        let device_id_struct = DeviceId::try_from(device_id).map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
        let address = ProtocolAddress::new(recipient_id.to_string(), device_id_struct);

        // Get local device ID for fetching sender certificate
        let local_device_id = self.settings_repo
             .get::<u32>(keys::CURRENT_DEVICE_ID)
             .map_err(ServiceError::Storage)?
             .ok_or(ServiceError::NotLoggedIn)?;
        tracing::debug!(recipient_id, device_id, local_device_id, "Signal: getting sender certificate");

        // Try to use cached certificate first
        let sender_cert = self.get_or_request_sender_certificate(auth_client, local_device_id).await?;
        let cert_expiration = sender_cert.expiration().map(|t| t.epoch_millis()).unwrap_or(0);
        tracing::debug!(
            recipient_id,
            device_id,
            cert_sender = ?sender_cert.sender_uuid(),
            cert_sender_device = ?sender_cert.sender_device_id(),
            cert_expiration,
            "Signal: sender certificate ready for encryption"
        );

        let mut session_store = SignalStore::new(self.key_store.clone());
        let mut identity_store = SignalStore::new(self.key_store.clone());

        tracing::debug!(recipient_id, device_id, "Signal: performing sealed sender encryption");
        // Encrypt with explicit types
        let ciphertext: Vec<u8> = SealedSenderManager::encrypt(
            plaintext,
            &sender_cert,
            &address,
            &mut session_store,
            &mut identity_store,
        )
        .await?;

        tracing::debug!(recipient_id, device_id, ciphertext_len = ciphertext.len(), "Signal: encryption complete");
        Ok(ciphertext)
    }

    /// Encrypt a message for a specific recipient device using basic Signal Protocol.
    ///
    /// This is "identified sender" mode - the server knows who is sending.
    /// Use this for DM bootstrap when we don't have the recipient's UAK.
    ///
    /// Returns a tuple of (ciphertext_bytes, message_type) where message_type is:
    /// - 2 = PreKeySignalMessage (first message, includes prekey info)
    /// - 3 = SignalMessage (subsequent messages)
    pub async fn encrypt_message_identified(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
        device_id: u32,
        plaintext: &[u8],
    ) -> Result<(Vec<u8>, u8), ServiceError> {
        tracing::debug!(recipient_id, device_id, plaintext_len = plaintext.len(),
            "Signal: encrypt_message_identified starting (basic Signal, not sealed sender)");

        // Acquire per-recipient lock to prevent counter collisions.
        let lock_key = format!("{}:{}", recipient_id, device_id);
        let recipient_lock = self.get_encrypt_lock(&lock_key).await;
        let _guard = recipient_lock.lock().await;

        tracing::debug!(recipient_id, device_id, "Signal: acquired encrypt lock for identified sender");

        // Ensure session exists
        self.ensure_session(auth_client, recipient_id, device_id).await?;
        tracing::debug!(recipient_id, device_id, "Signal: session ensured for identified sender");

        let device_id_struct = DeviceId::try_from(device_id)
            .map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
        let address = ProtocolAddress::new(recipient_id.to_string(), device_id_struct);

        let mut session_store = SignalStore::new(self.key_store.clone());
        let mut identity_store = SignalStore::new(self.key_store.clone());

        tracing::debug!(recipient_id, device_id, "Signal: performing basic message encryption");

        // Use message_encrypt for basic Signal Protocol (not sealed sender)
        let mut rng = rand::rngs::StdRng::from_os_rng();
        let ciphertext: CiphertextMessage = libsignal_protocol::message_encrypt(
            plaintext,
            &address,
            &mut session_store,
            &mut identity_store,
            std::time::SystemTime::now(),
            &mut rng,
        )
        .await
        .map_err(|e| {
            tracing::error!(recipient_id, device_id, error = %e, "Signal: message_encrypt failed");
            CryptoError::Protocol(e.to_string())
        })?;

        // Get message type for envelope routing
        let msg_type = match ciphertext.message_type() {
            CiphertextMessageType::PreKey => 2u8,    // PreKeySignalMessage
            CiphertextMessageType::Whisper => 3u8,   // SignalMessage (regular ciphertext)
            CiphertextMessageType::SenderKey => 4u8, // SenderKey (group messages)
            CiphertextMessageType::Plaintext => 1u8, // Plaintext content
        };

        let ciphertext_bytes = ciphertext.serialize().to_vec();

        tracing::debug!(
            recipient_id,
            device_id,
            ciphertext_len = ciphertext_bytes.len(),
            message_type = msg_type,
            "Signal: identified sender encryption complete"
        );

        Ok((ciphertext_bytes, msg_type))
    }

    /// Decrypt an incoming Sealed Sender message.
    ///
    /// Returns the sender's address and the decrypted plaintext.
    pub async fn decrypt_message(
        &self,
        ciphertext: &[u8],
    ) -> Result<(ProtocolAddress, Vec<u8>), ServiceError> {
        // We need the server's trust root public key to validate the sender certificate.
        let trust_root = &self.trust_root; 

        let local_uuid = self.settings_repo
             .get_string(keys::CURRENT_USER_ID)
             .map_err(ServiceError::Storage)?
             .ok_or(ServiceError::NotLoggedIn)?;
             
        let local_device_id = self.settings_repo
             .get::<u32>(keys::CURRENT_DEVICE_ID)
             .map_err(ServiceError::Storage)?
             .ok_or(ServiceError::NotLoggedIn)?;

        let mut identity_store = SignalStore::new(self.key_store.clone());
        let mut session_store = SignalStore::new(self.key_store.clone());
        let mut pre_key_store = SignalStore::new(self.key_store.clone());
        let signed_pre_key_store = SignalStore::new(self.key_store.clone());
        let mut kyber_pre_key_store = SignalStore::new(self.key_store.clone());

        let result: libsignal_protocol::SealedSenderDecryptionResult = SealedSenderManager::decrypt(
            ciphertext,
            &trust_root,
            libsignal_protocol::Timestamp::from_epoch_millis(std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64),
            None, // local_e164 not used
            local_uuid,
            DeviceId::new(local_device_id.try_into().map_err(|_| ServiceError::Validation("Local Device ID too large".into()))?)
                .map_err(|_| ServiceError::Validation("Invalid local device ID".into()))?,
            &mut identity_store,
            &mut session_store,
            &mut pre_key_store,
            &signed_pre_key_store,
            &mut kyber_pre_key_store,
        )
        .await?;

        let sender_address = ProtocolAddress::new(result.sender_uuid, result.device_id.into());
        Ok((sender_address, result.message))
    }

    /// Decrypt an incoming identified sender message (PreKey or Ciphertext).
    ///
    /// This is used when the server knows the sender (Basic auth fallback).
    /// Uses regular Signal Protocol message_decrypt instead of sealed_sender_decrypt.
    ///
    /// Returns the decrypted plaintext.
    pub async fn decrypt_message_identified(
        &self,
        ciphertext: &[u8],
        sender_id: &str,
        sender_device_id: u32,
    ) -> Result<Vec<u8>, ServiceError> {
        tracing::info!(
            sender_id,
            sender_device_id,
            ciphertext_len = ciphertext.len(),
            "Signal: decrypt_message_identified starting"
        );

        let device_id_struct = DeviceId::try_from(sender_device_id)
            .map_err(|_| ServiceError::Validation("Invalid sender device ID".into()))?;
        let sender_address = ProtocolAddress::new(sender_id.to_string(), device_id_struct);

        let mut identity_store = SignalStore::new(self.key_store.clone());
        let mut session_store = SignalStore::new(self.key_store.clone());
        let mut pre_key_store = SignalStore::new(self.key_store.clone());
        let signed_pre_key_store = SignalStore::new(self.key_store.clone());
        let mut kyber_pre_key_store = SignalStore::new(self.key_store.clone());

        // Try to deserialize as PreKeySignalMessage first (most common for new sessions)
        let mut rng = rand::rngs::StdRng::from_os_rng();

        let plaintext = if let Ok(prekey_msg) = PreKeySignalMessage::try_from(ciphertext) {
            tracing::debug!(sender_id, "Signal: decrypting as PreKeySignalMessage");
            libsignal_protocol::message_decrypt_prekey(
                &prekey_msg,
                &sender_address,
                &mut session_store,
                &mut identity_store,
                &mut pre_key_store,
                &signed_pre_key_store,
                &mut kyber_pre_key_store,
                &mut rng,
            )
            .await
            .map_err(|e| {
                tracing::error!(sender_id, sender_device_id, error = %e, "Signal: message_decrypt_prekey failed");
                CryptoError::Protocol(e.to_string())
            })?
        } else if let Ok(signal_msg) = SignalMessage::try_from(ciphertext) {
            tracing::debug!(sender_id, "Signal: decrypting as SignalMessage");
            libsignal_protocol::message_decrypt_signal(
                &signal_msg,
                &sender_address,
                &mut session_store,
                &mut identity_store,
                &mut rng,
            )
            .await
            .map_err(|e| {
                tracing::error!(sender_id, sender_device_id, error = %e, "Signal: message_decrypt_signal failed");
                CryptoError::Protocol(e.to_string())
            })?
        } else {
            tracing::error!(sender_id, "Signal: could not deserialize ciphertext as PreKey or Signal message");
            return Err(CryptoError::Protocol("Unknown ciphertext format".into()).into());
        };

        tracing::info!(
            sender_id,
            sender_device_id,
            plaintext_len = plaintext.len(),
            "Signal: identified sender decryption complete"
        );

        Ok(plaintext)
    }

    /// Decrypt with session convergence (Sesame algorithm).
    ///
    /// This method implements the Sesame receive path:
    /// 1. Try decryption with current session state
    /// 2. If that fails, try alternate sessions from SesameManager
    /// 3. On success with an inactive session, promote it to active
    /// 4. If all sessions fail, return an error indicating retry may be needed
    ///
    /// This is used for DM messages where session race conditions can occur
    /// during bootstrap.
    pub async fn decrypt_with_convergence<S: SesameStore>(
        &self,
        ciphertext: &[u8],
        sender_id: &str,
        sender_device_id: u32,
        sesame_manager: &SesameManager<S>,
    ) -> Result<ConvergenceResult, ServiceError> {
        tracing::info!(
            sender_id,
            sender_device_id,
            ciphertext_len = ciphertext.len(),
            "Signal: decrypt_with_convergence starting (Sesame)"
        );

        let sender_user_id: UserId = sender_id.parse()
            .map_err(|_| ServiceError::Validation("Invalid sender ID".into()))?;

        // Get all sessions for this sender/device from SesameManager
        let sessions = sesame_manager
            .get_all_sessions_for_convergence(&sender_user_id, sender_device_id)
            .map_err(ServiceError::Storage)?;

        // Backup current session state from KeyStore
        let device_id_struct = DeviceId::try_from(sender_device_id)
            .map_err(|_| ServiceError::Validation("Invalid sender device ID".into()))?;
        let sender_address = ProtocolAddress::new(sender_id.to_string(), device_id_struct);
        let address_str = sender_address.to_string();

        let original_session = self.key_store.load_session(&address_str)
            .map_err(ServiceError::Storage)?;

        let mut errors: Vec<String> = Vec::new();

        // First, try with current session state in KeyStore
        match self.try_decrypt_identified(ciphertext, &sender_address).await {
            Ok(plaintext) => {
                tracing::info!(
                    sender_id,
                    sender_device_id,
                    "Signal: decryption succeeded with current session"
                );

                // Get current session ID for tracking
                let current_session_id = if let Some(ref record) = original_session {
                    use sha2::{Digest, Sha256};
                    let mut hasher = Sha256::new();
                    hasher.update(record);
                    hasher.finalize().to_vec()
                } else {
                    Vec::new()
                };

                // Load updated session after decryption for SesameManager
                if let Ok(Some(updated_record)) = self.key_store.load_session(&address_str) {
                    if let Ok(session_record) = SessionRecord::deserialize(&updated_record) {
                        let _ = sesame_manager.record_successful_decrypt(
                            &sender_user_id,
                            sender_device_id,
                            &current_session_id,
                            &session_record,
                        );
                    }
                }

                return Ok(ConvergenceResult::ActiveSession {
                    plaintext,
                    session_id: current_session_id,
                });
            }
            Err(e) => {
                tracing::debug!(
                    sender_id,
                    sender_device_id,
                    error = %e,
                    "Signal: current session decryption failed, trying alternates"
                );
                errors.push(format!("current: {}", e));
            }
        }

        // Try each alternate session from SesameManager
        for (session_id, session_record) in sessions {
            // Skip if this matches what we already tried
            let session_bytes = session_record.serialize()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?;

            if Some(&session_bytes) == original_session.as_ref() {
                continue;
            }

            tracing::debug!(
                sender_id,
                sender_device_id,
                session_id_hex = %hex::encode(&session_id[..8.min(session_id.len())]),
                "Signal: trying alternate session"
            );

            // Temporarily store this session
            self.key_store.store_session(&address_str, &session_bytes)
                .map_err(ServiceError::Storage)?;

            match self.try_decrypt_identified(ciphertext, &sender_address).await {
                Ok(plaintext) => {
                    tracing::info!(
                        sender_id,
                        sender_device_id,
                        session_id_hex = %hex::encode(&session_id[..8.min(session_id.len())]),
                        "Signal: decryption succeeded with alternate session, promoting"
                    );

                    // Load the updated session after decryption
                    if let Ok(Some(updated_record)) = self.key_store.load_session(&address_str) {
                        if let Ok(updated_session_record) = SessionRecord::deserialize(&updated_record) {
                            // Record success and promote this session in SesameManager
                            let _ = sesame_manager.record_successful_decrypt(
                                &sender_user_id,
                                sender_device_id,
                                &session_id,
                                &updated_session_record,
                            );
                        }
                    }

                    return Ok(ConvergenceResult::PromotedSession {
                        plaintext,
                        session_id,
                    });
                }
                Err(e) => {
                    errors.push(format!("session {}: {}",
                        hex::encode(&session_id[..8.min(session_id.len())]), e));
                }
            }
        }

        // All sessions failed - restore original state
        if let Some(original) = original_session {
            let _ = self.key_store.store_session(&address_str, &original);
        }

        tracing::warn!(
            sender_id,
            sender_device_id,
            error_count = errors.len(),
            "Signal: all session convergence attempts failed"
        );

        Ok(ConvergenceResult::AllFailed { errors })
    }

    /// Internal helper to try decryption without modifying SesameManager state.
    async fn try_decrypt_identified(
        &self,
        ciphertext: &[u8],
        sender_address: &ProtocolAddress,
    ) -> Result<Vec<u8>, ServiceError> {
        let mut identity_store = SignalStore::new(self.key_store.clone());
        let mut session_store = SignalStore::new(self.key_store.clone());
        let mut pre_key_store = SignalStore::new(self.key_store.clone());
        let signed_pre_key_store = SignalStore::new(self.key_store.clone());
        let mut kyber_pre_key_store = SignalStore::new(self.key_store.clone());

        let mut rng = rand::rngs::StdRng::from_os_rng();

        let plaintext = if let Ok(prekey_msg) = PreKeySignalMessage::try_from(ciphertext) {
            libsignal_protocol::message_decrypt_prekey(
                &prekey_msg,
                sender_address,
                &mut session_store,
                &mut identity_store,
                &mut pre_key_store,
                &signed_pre_key_store,
                &mut kyber_pre_key_store,
                &mut rng,
            )
            .await
            .map_err(|e| CryptoError::Protocol(e.to_string()))?
        } else if let Ok(signal_msg) = SignalMessage::try_from(ciphertext) {
            libsignal_protocol::message_decrypt_signal(
                &signal_msg,
                sender_address,
                &mut session_store,
                &mut identity_store,
                &mut rng,
            )
            .await
            .map_err(|e| CryptoError::Protocol(e.to_string()))?
        } else {
            return Err(CryptoError::Protocol("Unknown ciphertext format".into()).into());
        };

        Ok(plaintext)
    }

    /// Get sender certificate from cache or request a new one from server.
    ///
    /// Certificates are cached locally and only refreshed when:
    /// - No cached certificate exists
    /// - The cached certificate has passed its refresh_after timestamp
    /// - The cached certificate fails to deserialize
    async fn get_or_request_sender_certificate(
        &self,
        auth_client: &mut AuthClient,
        device_id: u32,
    ) -> Result<SenderCertificate, ServiceError> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        // Check for cached certificate
        let cached_cert_b64: Option<String> = self.settings_repo
            .get_string(keys::SENDER_CERTIFICATE)
            .map_err(ServiceError::Storage)?;
        let cached_refresh_after: Option<u64> = self.settings_repo
            .get::<u64>(keys::SENDER_CERTIFICATE_REFRESH_AFTER)
            .map_err(ServiceError::Storage)?;

        tracing::info!(
            has_cached_cert = cached_cert_b64.is_some(),
            cached_refresh_after = ?cached_refresh_after,
            now_ms,
            "Signal: checking cached sender certificate"
        );

        // Try to use cached certificate if not expired
        if let (Some(cert_b64), Some(refresh_after)) = (&cached_cert_b64, cached_refresh_after) {
            if now_ms < refresh_after {
                tracing::info!(
                    refresh_after,
                    time_until_refresh_ms = refresh_after.saturating_sub(now_ms),
                    "Signal: using cached sender certificate (still valid)"
                );

                // Decode and deserialize cached certificate
                use base64::Engine;
                match base64::engine::general_purpose::STANDARD.decode(cert_b64) {
                    Ok(cert_bytes) => {
                        match SenderCertificate::deserialize(&cert_bytes) {
                            Ok(cert) => {
                                let expiration = cert.expiration().map(|t| t.epoch_millis()).unwrap_or(0);
                                tracing::info!(
                                    sender_uuid = ?cert.sender_uuid(),
                                    sender_device = ?cert.sender_device_id(),
                                    expiration,
                                    "Signal: cached certificate deserialized successfully"
                                );
                                return Ok(cert);
                            }
                            Err(e) => {
                                tracing::warn!(error = ?e, "Signal: cached certificate failed to deserialize, requesting new one");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = ?e, "Signal: cached certificate failed to decode from base64, requesting new one");
                    }
                }
            } else {
                tracing::info!(
                    refresh_after,
                    expired_by_ms = now_ms.saturating_sub(refresh_after),
                    "Signal: cached certificate needs refresh"
                );
            }
        }

        // Request new certificate from server
        tracing::info!(device_id, "Signal: requesting new sender certificate from server");
        let cert_response = auth_client
            .request_sender_certificate(device_id, None)
            .await
            .map_err(|e| {
                tracing::error!(device_id, error = %e, "Signal: failed to request sender certificate");
                NetworkError::from(e)
            })?;

        let refresh_after = cert_response.refresh_after;
        tracing::info!(
            device_id,
            refresh_after,
            has_certificate = cert_response.certificate.is_some(),
            "Signal: received sender certificate response from server"
        );

        let proto_cert = cert_response.certificate.ok_or_else(|| {
            tracing::error!("Signal: server returned empty certificate");
            ServiceError::Validation("Missing certificate in response".into())
        })?;

        tracing::info!(
            cert_bytes_len = proto_cert.certificate.len(),
            "Signal: deserializing sender certificate"
        );

        let sender_cert = SenderCertificate::deserialize(&proto_cert.certificate)
            .map_err(|e| {
                tracing::error!(error = ?e, "Signal: failed to deserialize sender certificate");
                CryptoError::InvalidSenderCertificate("Invalid certificate from server".into())
            })?;

        let expiration = sender_cert.expiration().map(|t| t.epoch_millis()).unwrap_or(0);
        tracing::info!(
            sender_uuid = ?sender_cert.sender_uuid(),
            sender_device = ?sender_cert.sender_device_id(),
            expiration,
            "Signal: sender certificate deserialized successfully"
        );

        // Cache the certificate
        use base64::Engine;
        let cert_b64 = base64::engine::general_purpose::STANDARD.encode(&proto_cert.certificate);
        if let Err(e) = self.settings_repo.set(keys::SENDER_CERTIFICATE, &cert_b64) {
            tracing::warn!(error = ?e, "Signal: failed to cache sender certificate");
        }
        if let Err(e) = self.settings_repo.set(keys::SENDER_CERTIFICATE_REFRESH_AFTER, &refresh_after) {
            tracing::warn!(error = ?e, "Signal: failed to cache certificate refresh_after");
        }

        tracing::info!(refresh_after, "Signal: sender certificate cached for future use");

        Ok(sender_cert)
    }

    /// Convert Protobuf PreKeyBundle to libsignal PreKeyBundle.
    fn convert_prekey_bundle(proto: crate::proto::chat_v1::SignalPreKeyBundle) -> Result<PreKeyBundle, ServiceError> {
        let registration_id = proto.registration_id;
        let device_id = DeviceId::try_from(proto.device_id).map_err(|_| ServiceError::Validation("Invalid device ID in bundle".into()))?;
        
        // Identity Key
        let identity_key = IdentityKey::decode(&proto.identity_key)
            .map_err(|_| ServiceError::Validation("Invalid identity key".into()))?;

        // Signed PreKey
        let signed_prekey = proto.signed_prekey.ok_or(ServiceError::Validation("Missing signed prekey".into()))?;
        let signed_prekey_id = SignedPreKeyId::from(signed_prekey.id);
        let signed_prekey_public = PublicKey::deserialize(&signed_prekey.public_key)
            .map_err(|_| ServiceError::Validation("Invalid signed prekey public key".into()))?;
        let signed_prekey_signature = signed_prekey.signature;

        // One-time PreKey (Optional)
        let pre_key_option = if let Some(pk) = proto.one_time_prekey {
            let id = PreKeyId::from(pk.id);
            let key = PublicKey::deserialize(&pk.public_key)
                .map_err(|_| ServiceError::Validation("Invalid one-time prekey public key".into()))?;
            Some((id, key))
        } else {
            None
        };

        // Kyber PreKey (Optional/Required for PQXDH)
        let kyber_prekey = proto.kyber_prekey.ok_or(ServiceError::Validation("Missing Kyber prekey".into()))?;
        let kyber_id = KyberPreKeyId::from(kyber_prekey.id);
        let kyber_public = libsignal_protocol::kem::Key::try_from(kyber_prekey.public_key.as_slice())
             .map_err(|_| ServiceError::Validation("Invalid Kyber prekey public key".into()))?;
        let kyber_signature = kyber_prekey.signature;

        PreKeyBundle::new(
            registration_id,
            device_id,
            pre_key_option,
            signed_prekey_id,
            signed_prekey_public,
            signed_prekey_signature,
            kyber_id,
            kyber_public,
            kyber_signature,
            identity_key,
        ).map_err(|e| ServiceError::Validation(format!("Invalid prekey bundle format: {}", e).into()))
    }

    pub fn set_trust_root(&mut self, key: PublicKey) {
        self.trust_root = key;
    }

    // =========================================================================
    // Sender Key Methods (Group Encryption)
    // =========================================================================

    /// Get the current user's protocol address.
    fn get_our_address(&self) -> Result<ProtocolAddress, ServiceError> {
        let user_id = self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .map_err(ServiceError::Storage)?
            .ok_or(ServiceError::NotLoggedIn)?;

        let device_id = self
            .settings_repo
            .get::<u32>(keys::CURRENT_DEVICE_ID)
            .map_err(ServiceError::Storage)?
            .unwrap_or(1);

        let device_id_struct = DeviceId::try_from(device_id)
            .map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;

        Ok(ProtocolAddress::new(user_id, device_id_struct))
    }

    /// Create a sender key distribution message for a channel.
    ///
    /// Call this when:
    /// - Creating an encrypted channel
    /// - Joining an encrypted channel
    /// - Rotating sender keys (e.g., after member leaves)
    ///
    /// The returned SKDM must be distributed to all channel members.
    pub async fn create_sender_key(
        &self,
        channel_id: &ChannelId,
    ) -> Result<SenderKeyDistribution, ServiceError> {
        let our_address = self.get_our_address()?;

        // Use channel ID as distribution ID (UUID)
        let distribution_id: Uuid = channel_id
            .to_string()
            .parse()
            .map_err(|_| ServiceError::Validation("Invalid channel ID format".into()))?;

        tracing::info!(
            channel_id = %channel_id,
            our_address = %our_address,
            distribution_id = %distribution_id,
            "[GROUP_MSG] Creating sender key for channel"
        );

        let mut store = SignalStore::new(self.key_store.clone());

        let skdm = SenderKeyManager::create_distribution_message(
            &our_address,
            distribution_id,
            &mut store,
        )
        .await
        .map_err(|e| {
            tracing::error!(
                channel_id = %channel_id,
                error = %e,
                "[GROUP_MSG] Failed to create sender key"
            );
            ServiceError::Crypto(e)
        })?;

        tracing::info!(
            channel_id = %channel_id,
            skdm_len = skdm.serialized().len(),
            "[GROUP_MSG] Created sender key for channel successfully"
        );

        Ok(SenderKeyDistribution {
            channel_id: channel_id.to_string(),
            distribution_message: skdm.serialized().to_vec(),
            timestamp: timestamp_nanos(),
        })
    }

    /// Process a received sender key distribution message.
    ///
    /// Call this when receiving an SKDM from another channel member.
    /// After processing, you can decrypt messages from that sender.
    pub async fn process_sender_key(
        &self,
        sender_id: &UserId,
        sender_device_id: u32,
        skdm: &SenderKeyDistribution,
    ) -> Result<(), ServiceError> {
        tracing::info!(
            sender_id = %sender_id,
            sender_device_id = sender_device_id,
            channel_id = %skdm.channel_id,
            skdm_len = skdm.distribution_message.len(),
            "[GROUP_MSG] Processing sender key distribution from member"
        );

        let device_id = DeviceId::try_from(sender_device_id)
            .map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
        let sender_address = ProtocolAddress::new(sender_id.to_string(), device_id);

        tracing::debug!(
            sender_address = %sender_address,
            "[GROUP_MSG] Constructed sender address for SKDM processing"
        );

        let skdm_msg = SenderKeyDistributionMessage::try_from(skdm.distribution_message.as_slice())
            .map_err(|e| {
                tracing::error!(
                    sender_id = %sender_id,
                    channel_id = %skdm.channel_id,
                    error = %e,
                    "[GROUP_MSG] Failed to deserialize SKDM"
                );
                CryptoError::Protocol(e.to_string())
            })?;

        let mut store = SignalStore::new(self.key_store.clone());

        SenderKeyManager::process_distribution_message(&sender_address, &skdm_msg, &mut store)
            .await
            .map_err(|e| {
                tracing::error!(
                    sender_id = %sender_id,
                    channel_id = %skdm.channel_id,
                    error = %e,
                    "[GROUP_MSG] Failed to process SKDM"
                );
                ServiceError::Crypto(e)
            })?;

        tracing::info!(
            sender_id = %sender_id,
            sender_device_id = sender_device_id,
            channel_id = %skdm.channel_id,
            "[GROUP_MSG] Successfully stored sender key from member"
        );

        Ok(())
    }

    /// Encrypt a message for a group channel using sender keys.
    ///
    /// All members who have our sender key can decrypt.
    /// Automatically creates a sender key if one doesn't exist.
    ///
    /// # Returns
    ///
    /// A tuple of (encrypted_message, optional_skdm). The SKDM is returned
    /// when a new sender key was created and must be distributed to all
    /// channel members so they can decrypt future messages.
    pub async fn encrypt_for_group(
        &self,
        channel_id: &ChannelId,
        plaintext: &[u8],
    ) -> Result<(GroupEncrypted, Option<SenderKeyDistribution>), ServiceError> {
        let our_address = self.get_our_address()?;

        let distribution_id: Uuid = channel_id
            .to_string()
            .parse()
            .map_err(|_| ServiceError::Validation("Invalid channel ID format".into()))?;

        tracing::info!(
            channel_id = %channel_id,
            our_address = %our_address,
            plaintext_len = plaintext.len(),
            "[GROUP_MSG] Encrypting message for group"
        );

        let mut store = SignalStore::new(self.key_store.clone());

        // Check if rotation was requested (e.g., member left the channel)
        // If so, clear the existing key to force creation of a new one
        if self.check_and_clear_rotation_needed(channel_id)? {
            tracing::info!(
                channel_id = %channel_id,
                "[GROUP_MSG] Rotation needed - clearing existing sender key"
            );
            // Clear existing key - this will force creation of a new one below
            let _ = self.clear_channel_sender_keys(channel_id);
        }

        // Check if we need to create a sender key and return SKDM for distribution
        let has_key = self.has_sender_key(channel_id)?;
        tracing::debug!(
            channel_id = %channel_id,
            has_sender_key = has_key,
            "[GROUP_MSG] Sender key check"
        );

        let skdm_to_distribute = if !has_key {
            tracing::info!(
                channel_id = %channel_id,
                "[GROUP_MSG] No sender key found, creating new SKDM"
            );
            let skdm = SenderKeyManager::create_distribution_message(
                &our_address,
                distribution_id,
                &mut store,
            )
            .await
            .map_err(|e| {
                tracing::error!(
                    channel_id = %channel_id,
                    error = %e,
                    "[GROUP_MSG] Failed to create SKDM for encryption"
                );
                ServiceError::Crypto(e)
            })?;

            tracing::info!(
                channel_id = %channel_id,
                skdm_len = skdm.serialized().len(),
                "[GROUP_MSG] Created new SKDM to distribute"
            );

            Some(SenderKeyDistribution {
                channel_id: channel_id.to_string(),
                distribution_message: skdm.serialized().to_vec(),
                timestamp: timestamp_nanos(),
            })
        } else {
            tracing::debug!(
                channel_id = %channel_id,
                "[GROUP_MSG] Using existing sender key"
            );
            None
        };

        let ciphertext =
            SenderKeyManager::encrypt(&our_address, distribution_id, plaintext, &mut store)
                .await
                .map_err(|e| {
                    tracing::error!(
                        channel_id = %channel_id,
                        error = %e,
                        "[GROUP_MSG] Sender key encryption failed"
                    );
                    ServiceError::Crypto(e)
                })?;

        tracing::info!(
            channel_id = %channel_id,
            plaintext_len = plaintext.len(),
            ciphertext_len = ciphertext.serialized().len(),
            skdm_created = skdm_to_distribute.is_some(),
            "[GROUP_MSG] Encrypted message for group successfully"
        );

        let group_encrypted = GroupEncrypted {
            channel_id: channel_id.to_string(),
            ciphertext: ciphertext.serialized().to_vec(),
            timestamp: timestamp_nanos(),
        };

        Ok((group_encrypted, skdm_to_distribute))
    }

    /// Decrypt a group message from a sender.
    ///
    /// Requires having processed that sender's SKDM first.
    pub async fn decrypt_from_group(
        &self,
        sender_id: &UserId,
        sender_device_id: u32,
        encrypted: &GroupEncrypted,
    ) -> Result<Vec<u8>, ServiceError> {
        tracing::info!(
            sender_id = %sender_id,
            sender_device_id = sender_device_id,
            channel_id = %encrypted.channel_id,
            ciphertext_len = encrypted.ciphertext.len(),
            "[GROUP_MSG] Decrypting message from group"
        );

        let device_id = DeviceId::try_from(sender_device_id)
            .map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
        let sender_address = ProtocolAddress::new(sender_id.to_string(), device_id);

        tracing::debug!(
            sender_address = %sender_address,
            "[GROUP_MSG] Constructed sender address for decryption"
        );

        let mut store = SignalStore::new(self.key_store.clone());

        let plaintext =
            SenderKeyManager::decrypt(&encrypted.ciphertext, &sender_address, &mut store)
                .await
                .map_err(|e| {
                    tracing::error!(
                        sender_id = %sender_id,
                        sender_device_id = sender_device_id,
                        channel_id = %encrypted.channel_id,
                        error = %e,
                        "[GROUP_MSG] Group decryption failed - sender key may be missing"
                    );
                    ServiceError::Crypto(e)
                })?;

        tracing::info!(
            sender_id = %sender_id,
            channel_id = %encrypted.channel_id,
            plaintext_len = plaintext.len(),
            "[GROUP_MSG] Decrypted message from group successfully"
        );

        Ok(plaintext)
    }

    /// Check if we have a sender key for a channel.
    pub fn has_sender_key(&self, channel_id: &ChannelId) -> Result<bool, ServiceError> {
        let our_address = self.get_our_address()?;

        let distribution_id: Uuid = channel_id
            .to_string()
            .parse()
            .map_err(|_| ServiceError::Validation("Invalid channel ID format".into()))?;

        let key = self
            .key_store
            .load_sender_key(&our_address.to_string(), &distribution_id.to_string())
            .map_err(ServiceError::Storage)?;

        let has_key = key.is_some();
        tracing::debug!(
            channel_id = %channel_id,
            our_address = %our_address,
            distribution_id = %distribution_id,
            has_key = has_key,
            "[GROUP_MSG] Checked sender key existence"
        );

        Ok(has_key)
    }

    /// Rotate our sender key for a channel.
    ///
    /// Call this when a member leaves an encrypted channel to maintain
    /// forward secrecy. The old key is deleted and a new SKDM is returned
    /// for distribution to remaining members.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - The channel to rotate keys for
    ///
    /// # Returns
    ///
    /// A new SenderKeyDistribution to send to remaining channel members.
    pub async fn rotate_sender_key(
        &self,
        channel_id: &ChannelId,
    ) -> Result<SenderKeyDistribution, ServiceError> {
        let our_address = self.get_our_address()?;

        let distribution_id: Uuid = channel_id
            .to_string()
            .parse()
            .map_err(|_| ServiceError::Validation("Invalid channel ID format".into()))?;

        // Delete existing sender key
        self.key_store
            .delete_sender_key(&our_address.to_string(), &distribution_id.to_string())
            .map_err(ServiceError::Storage)?;

        tracing::info!(
            channel_id = %channel_id,
            "Deleted old sender key for rotation"
        );

        // Create new sender key and SKDM
        let skdm = self.create_sender_key(channel_id).await?;

        tracing::info!(
            channel_id = %channel_id,
            "Created new sender key after rotation"
        );

        Ok(skdm)
    }

    /// Clear all sender keys for a channel.
    ///
    /// Call this when leaving a channel to clean up stored keys.
    pub fn clear_channel_sender_keys(&self, channel_id: &ChannelId) -> Result<(), ServiceError> {
        let our_address = self.get_our_address()?;

        let distribution_id: Uuid = channel_id
            .to_string()
            .parse()
            .map_err(|_| ServiceError::Validation("Invalid channel ID format".into()))?;

        // Delete our own sender key
        self.key_store
            .delete_sender_key(&our_address.to_string(), &distribution_id.to_string())
            .map_err(ServiceError::Storage)?;

        tracing::debug!(
            channel_id = %channel_id,
            "Cleared sender keys for channel"
        );

        Ok(())
    }

    /// Mark a channel as needing sender key rotation.
    ///
    /// Call this when a member leaves the channel to ensure forward secrecy.
    /// The rotation will happen automatically on the next message send.
    pub fn mark_channel_for_rotation(&self, channel_id: &ChannelId) -> Result<(), ServiceError> {
        let key = format!("sender_key_rotation_needed:{}", channel_id);
        self.settings_repo
            .set_string(&key, "1")
            .map_err(ServiceError::Storage)?;
        tracing::info!(
            channel_id = %channel_id,
            "Marked channel for sender key rotation"
        );
        Ok(())
    }

    /// Check if a channel needs sender key rotation and clear the flag.
    ///
    /// Returns true if rotation was needed (and clears the flag).
    fn check_and_clear_rotation_needed(&self, channel_id: &ChannelId) -> Result<bool, ServiceError> {
        let key = format!("sender_key_rotation_needed:{}", channel_id);
        let needs_rotation = self
            .settings_repo
            .get_string(&key)
            .map_err(ServiceError::Storage)?
            .map(|v| v == "1")
            .unwrap_or(false);

        if needs_rotation {
            // Clear the flag
            self.settings_repo
                .delete(&key)
                .map_err(ServiceError::Storage)?;
            tracing::debug!(
                channel_id = %channel_id,
                "Cleared sender key rotation flag"
            );
        }

        Ok(needs_rotation)
    }

    // ==================== Identity & PreKey Management ====================

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

    // ==================== Call Encryption ====================

    /// Derive a shared secret for call encryption with a peer.
    ///
    /// Uses ECDH between our identity key and peer's identity key,
    /// then derives a call-specific key. This is separate from the
    /// Double Ratchet session - it's a one-time derivation for calls.
    ///
    /// # Arguments
    /// * `peer_address` - The peer's Signal protocol address
    /// * `call_id` - Unique call identifier
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;
    use crate::storage::{Database, KeyStore};
    use crate::storage::repositories::{SettingsRepository, settings_keys};

    async fn setup_service(path: &std::path::Path) -> (SignalService, Arc<KeyStore>, Arc<SettingsRepository>) {
        let db = Arc::new(Database::open(path, "test_passphrase").unwrap());
        db.run_migrations().unwrap();

        let key_store = Arc::new(KeyStore::new(db.clone()));
        let settings_repo = Arc::new(SettingsRepository::new(db.clone()));

        // Initialize settings
        settings_repo.set(settings_keys::CURRENT_USER_ID, &"bob_uuid".to_string()).unwrap();
        settings_repo.set(settings_keys::CURRENT_DEVICE_ID, &1u32).unwrap();

        let service = SignalService::new(key_store.clone(), settings_repo.clone());
        (service, key_store, settings_repo)
    }

    #[tokio::test]
    async fn test_signal_service_creation() {
        let db_file = NamedTempFile::new().unwrap();
        let (service, _key_store, _settings) = setup_service(db_file.path()).await;

        // Basic test that service can be created
        assert!(service.key_store.get_local_identity().is_err() ||
                service.key_store.get_local_identity().unwrap().is_none());
    }

    #[tokio::test]
    async fn test_signal_store_creation() {
        let db_file = NamedTempFile::new().unwrap();
        let (_service, key_store, _settings) = setup_service(db_file.path()).await;

        let store = SignalStore::new(key_store);
        // Store should be created successfully
        drop(store);
    }

    // TODO: Full sealed sender encryption/decryption test requires updated
    // libsignal-protocol API usage. The libsignal API has changed significantly:
    // - SealedSenderManager is now in our crate, not libsignal
    // - DeviceId construction changed
    // - SignedPreKeyRecord/KyberPreKeyRecord use generate() not new()
    // - CertificateGenerator API differs
    //
    // See integration_sealed_sender.rs for working sealed sender tests
    // that use our SealedSenderManager wrapper.
}
