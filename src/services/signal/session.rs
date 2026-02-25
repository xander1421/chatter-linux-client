//! Session management and encryption/decryption operations.
//!
//! Handles establishing Signal Protocol sessions and encrypting/decrypting
//! messages using both sealed sender and identified sender modes.

use std::sync::Arc;
use rand::SeedableRng;
use libsignal_protocol::{
    ProtocolAddress, DeviceId,
    PreKeyBundle, PublicKey, IdentityKey, PreKeyId, SignedPreKeyId, KyberPreKeyId,
    SenderCertificate,
    CiphertextMessage, CiphertextMessageType, PreKeySignalMessage, SignalMessage, SessionRecord,
};
use tokio::sync::Mutex;

use crate::crypto::{
    SealedSenderManager, SessionManager, SignalStore,
    signal::sesame::{SesameManager, SesameStore, ConvergenceResult},
};
use crate::domain::ids::UserId;
use crate::network::grpc::auth_client::AuthClient;
use crate::storage::repositories::settings_repo::keys;
use crate::error::{CryptoError, ServiceError};

use super::SignalService;

/// Maximum number of encrypt locks to keep in cache.
const MAX_ENCRYPT_LOCKS: usize = 500;

impl SignalService {
    /// Get or create a lock for encrypting to a specific recipient.
    ///
    /// This ensures only one encryption operation to a given recipient
    /// happens at a time, preventing Double Ratchet counter collisions.
    /// Uses LRU eviction to bound memory usage.
    pub(crate) async fn get_encrypt_lock(&self, recipient_key: &str) -> Arc<Mutex<()>> {
        let mut locks = self.encrypt_locks.lock().await;
        let mut lru = self.encrypt_lock_lru.lock().await;

        // Touch recipient in LRU
        lru.retain(|key| key != recipient_key);
        lru.push_back(recipient_key.to_string());

        // Evict oldest locks if over limit (only if not currently in use)
        while lru.len() > MAX_ENCRYPT_LOCKS {
            if let Some(oldest) = lru.pop_front() {
                // Only remove if the lock is not currently held
                // Arc::strong_count == 1 means only the HashMap holds it
                if let Some(lock) = locks.get(&oldest) {
                    if Arc::strong_count(lock) == 1 {
                        locks.remove(&oldest);
                    } else {
                        // Lock is in use, put it back at front (will try again later)
                        lru.push_front(oldest);
                        break;
                    }
                }
            }
        }

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
        if SessionManager::has_session(&SignalStore::new(self.key_store.clone()), &address).await? {
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
                e
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
            e
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
        let trust_root = self.trust_root;

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

        let sender_address = ProtocolAddress::new(result.sender_uuid, result.device_id);
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
    pub(crate) async fn try_decrypt_identified(
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
    pub(crate) async fn get_or_request_sender_certificate(
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
                e
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
    pub(crate) fn convert_prekey_bundle(proto: crate::proto::chat_v1::SignalPreKeyBundle) -> Result<PreKeyBundle, ServiceError> {
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
        ).map_err(|e| ServiceError::Validation(format!("Invalid prekey bundle format: {}", e)))
    }

    pub fn set_trust_root(&mut self, key: PublicKey) {
        self.trust_root = key;
    }
}
