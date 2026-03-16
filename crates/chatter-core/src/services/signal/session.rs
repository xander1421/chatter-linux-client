//! Session management and encryption/decryption operations.
//!
//! Handles establishing Signal Protocol sessions and encrypting/decrypting
//! messages using both sealed sender and identified sender modes.
//! All operations are delegated to the SignalActor for thread-safety.

use std::collections::HashMap;
use std::sync::Arc;
use libsignal_protocol::{ProtocolAddress, DeviceId, SenderCertificate, PublicKey};
use tokio::sync::Mutex;

use crate::crypto::signal::sesame::ConvergenceResult;
use crate::network::grpc::auth_client::AuthClient;
use crate::storage::repositories::settings_repo::keys;
use crate::error::ServiceError;
use super::SignalService;

/// Maximum number of encrypt locks to keep in cache.
const MAX_ENCRYPT_LOCKS: usize = 500;

impl SignalService {
    /// Get or create a lock for encrypting to a specific recipient.
    pub(crate) async fn get_encrypt_lock(&self, recipient_key: &str) -> Arc<Mutex<()>> {
        let mut locks = self.encrypt_locks.lock().await;
        let mut lru = self.encrypt_lock_lru.lock().await;

        lru.retain(|key| key != recipient_key);
        lru.push_back(recipient_key.to_string());

        while lru.len() > MAX_ENCRYPT_LOCKS {
            if let Some(oldest) = lru.pop_front() {
                if let Some(lock) = locks.get(&oldest) {
                    if Arc::strong_count(lock) == 1 {
                        locks.remove(&oldest);
                    } else {
                        lru.push_front(oldest);
                        break;
                    }
                }
            }
        }

        locks.entry(recipient_key.to_string()).or_insert_with(|| Arc::new(Mutex::new(()))).clone()
    }

    /// Establish a session with a remote recipient if one doesn't exist.
    ///
    /// **Important**: This checks if a session already exists before fetching prekeys.
    /// If we already have a session (e.g., from receiving a message from this peer),
    /// we reuse it rather than creating a new one. This is critical for the bootstrap
    /// protocol where one party receives a PreKey message and should use that session
    /// for the response, not fetch new prekeys.
    pub async fn ensure_session(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
        device_id: u32,
    ) -> Result<(), ServiceError> {
        let device_id_struct = DeviceId::try_from(device_id).map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
        let address = ProtocolAddress::new(recipient_id.to_string(), device_id_struct);

        // Check if we already have a session with this peer
        // This happens when we've received a message from them (e.g., bootstrap MESSAGE)
        if self.actor.has_session(address.clone()).await? {
            tracing::debug!(
                recipient = %recipient_id,
                device_id,
                "Session already exists, skipping prekey fetch"
            );
            return Ok(());
        }

        // No existing session - fetch prekey bundle from server
        tracing::debug!(
            recipient = %recipient_id,
            device_id,
            "No session exists, fetching prekey bundle"
        );

        let bundle_response = auth_client
            .get_pre_key_bundle(recipient_id, &device_id.to_string())
            .await?;

        let result = bundle_response.result.ok_or(ServiceError::Validation("Missing bundle".into()))?;
        let proto_bundle = match result {
            crate::proto::chat_v1::get_pre_key_bundle_response::Result::Bundle(b) => b,
            _ => return Err(ServiceError::Validation("Unexpected response type".into())),
        };

        let bundle = Self::convert_prekey_bundle(proto_bundle)?;
        self.actor.establish_session(address, bundle).await
    }

    /// Encrypt a message using Sealed Sender.
    pub async fn encrypt_message(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
        device_id: u32,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, ServiceError> {
        let lock_key = format!("{}:{}", recipient_id, device_id);
        let lock = self.get_encrypt_lock(&lock_key).await;
        let _guard = lock.lock().await;

        self.ensure_session(auth_client, recipient_id, device_id).await?;

        let device_id_struct = DeviceId::try_from(device_id).map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
        let address = ProtocolAddress::new(recipient_id.to_string(), device_id_struct);

        let local_device_id = self.settings_repo.get::<u32>(keys::CURRENT_DEVICE_ID)?.ok_or(ServiceError::NotLoggedIn)?;
        let sender_cert = self.get_or_request_sender_certificate(auth_client, local_device_id).await?;

        self.actor.encrypt_sealed(plaintext.to_vec(), sender_cert, address).await
    }

    /// Encrypt a message using basic Signal Protocol (Identified).
    pub async fn encrypt_message_identified(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
        device_id: u32,
        plaintext: &[u8],
    ) -> Result<(Vec<u8>, u8), ServiceError> {
        let lock_key = format!("{}:{}", recipient_id, device_id);
        let lock = self.get_encrypt_lock(&lock_key).await;
        let _guard = lock.lock().await;

        self.ensure_session(auth_client, recipient_id, device_id).await?;

        let device_id_struct = DeviceId::try_from(device_id).map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
        let address = ProtocolAddress::new(recipient_id.to_string(), device_id_struct);

        self.actor.encrypt_identified(plaintext.to_vec(), address).await
    }

    /// Decrypt an incoming Sealed Sender message.
    pub async fn decrypt_message(
        &self,
        ciphertext: &[u8],
    ) -> Result<(ProtocolAddress, Vec<u8>), ServiceError> {
        let local_uuid = self.settings_repo.get_string(keys::CURRENT_USER_ID)?.ok_or(ServiceError::NotLoggedIn)?;
        let local_device_id = self.settings_repo.get::<u32>(keys::CURRENT_DEVICE_ID)?.ok_or(ServiceError::NotLoggedIn)?;
        let device_id_struct = DeviceId::new(local_device_id.try_into().map_err(|_| ServiceError::Validation("ID too large".into()))?).map_err(|_| ServiceError::Validation("Invalid ID".into()))?;

        self.actor.decrypt_sealed(ciphertext.to_vec(), self.trust_root, local_uuid, device_id_struct).await
    }

    /// Decrypt an incoming identified sender message.
    pub async fn decrypt_message_identified(
        &self,
        ciphertext: &[u8],
        sender_id: &str,
        sender_device_id: u32,
    ) -> Result<Vec<u8>, ServiceError> {
        let device_id_struct = DeviceId::try_from(sender_device_id).map_err(|_| ServiceError::Validation("Invalid ID".into()))?;
        let sender_address = ProtocolAddress::new(sender_id.to_string(), device_id_struct);

        self.actor.decrypt_identified(ciphertext.to_vec(), sender_address).await
    }

    /// Decrypt a message with session convergence (Sesame).
    pub async fn decrypt_with_convergence(
        &self,
        envelope: &[u8],
        sender_id: &str,
        sender_device_id: u32,
        sessions: Vec<(Vec<u8>, libsignal_protocol::SessionRecord)>,
    ) -> Result<ConvergenceResult, ServiceError> {
        self.actor.decrypt_with_convergence(
            envelope.to_vec(),
            sender_id.to_string(),
            sender_device_id,
            sessions,
        ).await
    }

    /// Get sender certificate from cache or request a new one.
    pub(crate) async fn get_or_request_sender_certificate(
        &self,
        auth_client: &mut AuthClient,
        device_id: u32,
    ) -> Result<SenderCertificate, ServiceError> {
        let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;

        let cached_cert_b64: Option<String> = self.settings_repo.get_string(keys::SENDER_CERTIFICATE)?;
        let cached_refresh_after: Option<u64> = self.settings_repo.get::<u64>(keys::SENDER_CERTIFICATE_REFRESH_AFTER)?;

        if let (Some(cert_b64), Some(refresh_after)) = (&cached_cert_b64, cached_refresh_after) {
            if now_ms < refresh_after {
                use base64::Engine;
                if let Ok(cert_bytes) = base64::engine::general_purpose::STANDARD.decode(cert_b64) {
                    if let Ok(cert) = SenderCertificate::deserialize(&cert_bytes) {
                        return Ok(cert);
                    }
                }
            }
        }

        let cert_response = auth_client.request_sender_certificate(device_id, None).await?;
        let proto_cert = cert_response.certificate.ok_or(ServiceError::Validation("Missing certificate".into()))?;
        let sender_cert = SenderCertificate::deserialize(&proto_cert.certificate).map_err(|_| ServiceError::Validation("Invalid cert".into()))?;

        use base64::Engine;
        let cert_b64 = base64::engine::general_purpose::STANDARD.encode(&proto_cert.certificate);
        let _ = self.settings_repo.set(keys::SENDER_CERTIFICATE, &cert_b64);
        let _ = self.settings_repo.set(keys::SENDER_CERTIFICATE_REFRESH_AFTER, &cert_response.refresh_after);

        Ok(sender_cert)
    }

    /// Convert Protobuf PreKeyBundle to libsignal PreKeyBundle.
    pub(crate) fn convert_prekey_bundle(proto: crate::proto::chat_v1::SignalPreKeyBundle) -> Result<libsignal_protocol::PreKeyBundle, ServiceError> {
        let registration_id = proto.registration_id;
        let device_id = DeviceId::try_from(proto.device_id).map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
        let identity_key = libsignal_protocol::IdentityKey::decode(&proto.identity_key).map_err(|_| ServiceError::Validation("Invalid identity key".into()))?;
        let signed_prekey = proto.signed_prekey.ok_or(ServiceError::Validation("Missing signed prekey".into()))?;
        let signed_prekey_id = libsignal_protocol::SignedPreKeyId::from(signed_prekey.id);
        let signed_prekey_public = PublicKey::deserialize(&signed_prekey.public_key).map_err(|_| ServiceError::Validation("Invalid signed prekey public key".into()))?;
        let signed_prekey_signature = signed_prekey.signature;

        let pre_key_option = if let Some(pk) = proto.one_time_prekey {
            Some((libsignal_protocol::PreKeyId::from(pk.id), PublicKey::deserialize(&pk.public_key).map_err(|_| ServiceError::Validation("Invalid one-time prekey".into()))?))
        } else {
            None
        };

        let kyber_prekey = proto.kyber_prekey.ok_or(ServiceError::Validation("Missing Kyber prekey".into()))?;
        let kyber_id = libsignal_protocol::KyberPreKeyId::from(kyber_prekey.id);
        let kyber_public = libsignal_protocol::kem::Key::try_from(kyber_prekey.public_key.as_slice()).map_err(|_| ServiceError::Validation("Invalid Kyber key".into()))?;
        let kyber_signature = kyber_prekey.signature;

        libsignal_protocol::PreKeyBundle::new(
            registration_id, device_id, pre_key_option,
            signed_prekey_id, signed_prekey_public, signed_prekey_signature,
            kyber_id, kyber_public, kyber_signature, identity_key,
        ).map_err(|e| ServiceError::Validation(format!("Invalid bundle: {}", e)))
    }

    pub fn set_trust_root(&mut self, key: PublicKey) {
        self.trust_root = key;
    }

    // =========================================================================
    // Multi-Device Operations
    // =========================================================================

    /// Ensure sessions exist for ALL devices of a recipient.
    ///
    /// Fetches prekey bundles for all devices and establishes sessions.
    /// Returns the list of device IDs for which sessions were established.
    ///
    /// This is used to send messages to all of a recipient's devices,
    /// ensuring they receive the message on any device they're using.
    pub async fn ensure_sessions_all_devices(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
    ) -> Result<Vec<u32>, ServiceError> {
        use crate::proto::chat_v1::get_pre_key_bundle_response;

        // Fetch prekey bundles for ALL devices using wildcard
        let response = auth_client.get_pre_key_bundles_all_devices(recipient_id).await?;

        let result = response.result.ok_or(ServiceError::Validation(
            "No device bundles returned for recipient".into(),
        ))?;

        match result {
            // Multiple devices response
            get_pre_key_bundle_response::Result::Devices(device_list) => {
                if device_list.bundles.is_empty() {
                    return Err(ServiceError::Validation(
                        "No devices registered for recipient".into(),
                    ));
                }

                let mut established_device_ids = Vec::with_capacity(device_list.bundles.len());

                for proto_bundle in device_list.bundles {
                    let device_id = proto_bundle.device_id;
                    let device_id_struct = match DeviceId::try_from(device_id) {
                        Ok(d) => d,
                        Err(_) => {
                            tracing::warn!(
                                recipient = %recipient_id,
                                device_id,
                                "Invalid device ID, skipping"
                            );
                            continue;
                        }
                    };
                    let address = ProtocolAddress::new(recipient_id.to_string(), device_id_struct);

                    // Check if session already exists
                    if self.actor.has_session(address.clone()).await? {
                        tracing::debug!(
                            recipient = %recipient_id,
                            device_id,
                            "Session already exists for device"
                        );
                        established_device_ids.push(device_id);
                        continue;
                    }

                    // Establish new session from bundle
                    match Self::convert_prekey_bundle(proto_bundle) {
                        Ok(bundle) => {
                            match self.actor.establish_session(address, bundle).await {
                                Ok(_) => {
                                    tracing::debug!(
                                        recipient = %recipient_id,
                                        device_id,
                                        "Established session with device"
                                    );
                                    established_device_ids.push(device_id);
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        recipient = %recipient_id,
                                        device_id,
                                        error = %e,
                                        "Failed to establish session with device"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                recipient = %recipient_id,
                                device_id,
                                error = %e,
                                "Failed to convert prekey bundle for device"
                            );
                        }
                    }
                }

                if established_device_ids.is_empty() {
                    return Err(ServiceError::Validation(
                        "Failed to establish session with any device".into(),
                    ));
                }

                tracing::info!(
                    recipient = %recipient_id,
                    device_count = established_device_ids.len(),
                    device_ids = ?established_device_ids,
                    "Established sessions with all devices"
                );

                Ok(established_device_ids)
            }

            // Single device fallback (server doesn't support multi-device query)
            get_pre_key_bundle_response::Result::Bundle(single_bundle) => {
                let device_id = single_bundle.device_id;
                self.ensure_session(auth_client, recipient_id, device_id).await?;
                Ok(vec![device_id])
            }
        }
    }

    /// Encrypt a message for ALL devices of a recipient.
    ///
    /// Returns a map of device_id -> ciphertext for each device that was
    /// successfully encrypted to. If encryption fails for some devices,
    /// those devices are skipped and a warning is logged.
    ///
    /// # Errors
    ///
    /// Returns an error only if ALL devices fail to encrypt. Partial
    /// success is considered acceptable (the message will reach some devices).
    pub async fn encrypt_message_all_devices(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
        plaintext: &[u8],
    ) -> Result<HashMap<u32, Vec<u8>>, ServiceError> {
        // First ensure we have sessions with all devices
        let device_ids = self.ensure_sessions_all_devices(auth_client, recipient_id).await?;

        // Get sender certificate once (used for all encryptions)
        let local_device_id = self.settings_repo
            .get::<u32>(keys::CURRENT_DEVICE_ID)?
            .ok_or(ServiceError::NotLoggedIn)?;
        let sender_cert = self.get_or_request_sender_certificate(auth_client, local_device_id).await?;

        // Encrypt to each device
        let mut ciphertexts: HashMap<u32, Vec<u8>> = HashMap::with_capacity(device_ids.len());
        let mut failure_count = 0;

        for device_id in device_ids {
            let lock_key = format!("{}:{}", recipient_id, device_id);
            let lock = self.get_encrypt_lock(&lock_key).await;
            let _guard = lock.lock().await;

            let device_id_struct = match DeviceId::try_from(device_id) {
                Ok(d) => d,
                Err(_) => {
                    tracing::warn!(device_id, "Invalid device ID during encryption");
                    failure_count += 1;
                    continue;
                }
            };
            let address = ProtocolAddress::new(recipient_id.to_string(), device_id_struct);

            match self.actor.encrypt_sealed(plaintext.to_vec(), sender_cert.clone(), address).await {
                Ok(ciphertext) => {
                    ciphertexts.insert(device_id, ciphertext);
                }
                Err(e) => {
                    tracing::warn!(
                        recipient = %recipient_id,
                        device_id,
                        error = %e,
                        "Failed to encrypt for device"
                    );
                    failure_count += 1;
                }
            }
        }

        if ciphertexts.is_empty() {
            return Err(ServiceError::Validation(format!(
                "Failed to encrypt for any device ({} failures)",
                failure_count
            )));
        }

        tracing::info!(
            recipient = %recipient_id,
            success_count = ciphertexts.len(),
            failure_count,
            "Encrypted message for recipient's devices"
        );

        Ok(ciphertexts)
    }

    // =========================================================================
    // Anonymous Operations (for AnonDM - no authenticated requests)
    // =========================================================================

    /// Establish a session using a pre-fetched prekey bundle.
    ///
    /// This allows establishing sessions without making an authenticated
    /// prekey fetch request (the bundle was fetched anonymously).
    pub async fn ensure_session_with_bundle(
        &self,
        recipient_id: &str,
        device_id: u32,
        bundle_response: crate::proto::chat_v1::GetPreKeyBundleResponse,
    ) -> Result<(), ServiceError> {
        let device_id_struct = DeviceId::try_from(device_id)
            .map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
        let address = ProtocolAddress::new(recipient_id.to_string(), device_id_struct);

        // Check if we already have a session
        if self.actor.has_session(address.clone()).await? {
            tracing::debug!(
                recipient = %recipient_id,
                device_id,
                "Session already exists, skipping bundle processing"
            );
            return Ok(());
        }

        let result = bundle_response
            .result
            .ok_or(ServiceError::Validation("Missing bundle".into()))?;
        let proto_bundle = match result {
            crate::proto::chat_v1::get_pre_key_bundle_response::Result::Bundle(b) => b,
            _ => return Err(ServiceError::Validation("Unexpected response type".into())),
        };

        let bundle = Self::convert_prekey_bundle(proto_bundle)?;
        self.actor.establish_session(address, bundle).await
    }

    /// Get cached sender certificate without making auth request.
    ///
    /// Returns None if no valid cached certificate exists.
    pub fn get_cached_sender_certificate(&self) -> Result<Option<SenderCertificate>, ServiceError> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let cached_cert_b64: Option<String> =
            self.settings_repo.get_string(keys::SENDER_CERTIFICATE)?;
        let cached_refresh_after: Option<u64> =
            self.settings_repo.get::<u64>(keys::SENDER_CERTIFICATE_REFRESH_AFTER)?;

        if let (Some(cert_b64), Some(refresh_after)) = (&cached_cert_b64, cached_refresh_after) {
            if now_ms < refresh_after {
                use base64::Engine;
                if let Ok(cert_bytes) =
                    base64::engine::general_purpose::STANDARD.decode(cert_b64)
                {
                    if let Ok(cert) = SenderCertificate::deserialize(&cert_bytes) {
                        return Ok(Some(cert));
                    }
                }
            }
        }

        Ok(None)
    }

    /// Encrypt a message anonymously (using cached sender cert, no auth requests).
    ///
    /// Pre-conditions:
    /// - Session must already be established (call ensure_session_with_bundle first)
    /// - Sender certificate must be cached (pre-fetched during login)
    pub async fn encrypt_message_anonymous(
        &self,
        recipient_id: &str,
        device_id: u32,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, ServiceError> {
        let lock_key = format!("{}:{}", recipient_id, device_id);
        let lock = self.get_encrypt_lock(&lock_key).await;
        let _guard = lock.lock().await;

        let device_id_struct = DeviceId::try_from(device_id)
            .map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
        let address = ProtocolAddress::new(recipient_id.to_string(), device_id_struct);

        // Verify session exists (should have been established with anonymous bundle)
        if !self.actor.has_session(address.clone()).await? {
            return Err(ServiceError::Validation(
                "No session exists - call ensure_session_with_bundle first".into(),
            ));
        }

        // Use cached sender certificate (must be pre-fetched during login)
        let sender_cert = self
            .get_cached_sender_certificate()?
            .ok_or_else(|| ServiceError::Validation(
                "No cached sender certificate - ensure pre-fetch during login".into(),
            ))?;

        self.actor
            .encrypt_sealed(plaintext.to_vec(), sender_cert, address)
            .await
    }
}
