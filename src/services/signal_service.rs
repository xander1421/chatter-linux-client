//! Signal Protocol orchestration service.
//!
//! Bridges the gap between business logic (MessageService) and crypto primitives.
//! Handles session establishment, message encryption, and decryption.

use std::sync::Arc;
use rand::SeedableRng;
use libsignal_protocol::{
    ProtocolAddress, DeviceId,
    PreKeyBundle, PublicKey, IdentityKey, PreKeyId, SignedPreKeyId, KyberPreKeyId,
    SenderCertificate, SenderKeyDistributionMessage,
};
use uuid::Uuid;


use crate::crypto::{
    SealedSenderManager, SessionManager, SignalStore,
    signal::sender_key::SenderKeyManager,
};
use crate::domain::ids::{ChannelId, UserId};
use crate::proto::content_v1::{GroupEncrypted, SenderKeyDistribution};
use crate::network::grpc::auth_client::AuthClient;
use crate::storage::repositories::settings_repo::{keys, SettingsRepository};
use crate::error::{CryptoError, NetworkError, ServiceError};

use crate::storage::KeyStore;

/// Signal Service for end-to-end encryption.
#[derive(Clone)]
pub struct SignalService {
    key_store: Arc<KeyStore>,
    settings_repo: Arc<SettingsRepository>,
    trust_root: PublicKey,
    // Managers are stateless but might hold KeyStore too
    // Stateless managers are used directly in methods
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
        }
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
        let device_id_struct = DeviceId::try_from(device_id).map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
        let address = ProtocolAddress::new(recipient_id.to_string(), device_id_struct);


        if SessionManager::has_session(&SignalStore::new(self.key_store.clone()), &address).await.map_err(CryptoError::from)? {
            return Ok(());
        }

        // Fetch prekey bundle from server
        let bundle_response = auth_client
            .get_pre_key_bundle(recipient_id, &device_id.to_string())
            .await
            .map_err(NetworkError::from)?;
        
        let result = bundle_response.result.ok_or(ServiceError::Validation("Missing bundle in response".into()))?;
        let proto_bundle = match result {
            crate::proto::chat_v1::get_pre_key_bundle_response::Result::Bundle(b) => b,
            _ => return Err(ServiceError::Validation("Unexpected response type".into())),
        };
        let bundle = Self::convert_prekey_bundle(proto_bundle)?;

        // Process bundle to establish session
        let mut rng = rand::rngs::StdRng::from_os_rng();
        let mut session_store = SignalStore::new(self.key_store.clone());
        let mut identity_store = SignalStore::new(self.key_store.clone());

        SessionManager::establish_session(
            &address,
            &mut session_store,
            &mut identity_store,
            &bundle,
            &mut rng,
        )
        .await
        .map_err(CryptoError::from)?;

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
        // Ensure session exists
        self.ensure_session(auth_client, recipient_id, device_id).await?;

        let device_id_struct = DeviceId::try_from(device_id).map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
        let address = ProtocolAddress::new(recipient_id.to_string(), device_id_struct);


        // Get local device ID for fetching sender certificate
        let local_device_id = self.settings_repo
             .get::<u32>(keys::CURRENT_DEVICE_ID)
             .map_err(ServiceError::Storage)?
             .ok_or(ServiceError::NotLoggedIn)?;

        // Get our sender certificate
        let cert_response = auth_client
            .get_sender_certificate(local_device_id)
            .await
            .map_err(NetworkError::from)?;
        
        let proto_cert = cert_response.certificate.ok_or(ServiceError::Validation("Missing certificate".into()))?;
        let sender_cert = SenderCertificate::deserialize(&proto_cert.certificate)
            .map_err(|_| CryptoError::InvalidSenderCertificate("Invalid certificate from server".into()))?;

        let mut session_store = SignalStore::new(self.key_store.clone());
        let mut identity_store = SignalStore::new(self.key_store.clone());

        // Encrypt with explicit types
        let ciphertext: Vec<u8> = SealedSenderManager::encrypt(
            plaintext,
            &sender_cert,
            &address,
            &mut session_store,
            &mut identity_store,
        )
        .await?;

        Ok(ciphertext)
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

        let mut store = SignalStore::new(self.key_store.clone());

        let skdm = SenderKeyManager::create_distribution_message(
            &our_address,
            distribution_id,
            &mut store,
        )
        .await
        .map_err(ServiceError::Crypto)?;

        tracing::debug!(
            channel_id = %channel_id,
            skdm_len = skdm.serialized().len(),
            "Created sender key for channel"
        );

        Ok(SenderKeyDistribution {
            channel_id: channel_id.to_string(),
            distribution_message: skdm.serialized().to_vec(),
            timestamp: chrono::Utc::now().timestamp_millis() as u64,
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
        let device_id = DeviceId::try_from(sender_device_id)
            .map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
        let sender_address = ProtocolAddress::new(sender_id.to_string(), device_id);

        let skdm_msg = SenderKeyDistributionMessage::try_from(skdm.distribution_message.as_slice())
            .map_err(|e| CryptoError::Protocol(e.to_string()))?;

        let mut store = SignalStore::new(self.key_store.clone());

        SenderKeyManager::process_distribution_message(&sender_address, &skdm_msg, &mut store)
            .await
            .map_err(ServiceError::Crypto)?;

        tracing::debug!(
            sender_id = %sender_id,
            channel_id = %skdm.channel_id,
            "Processed sender key from member"
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

        let mut store = SignalStore::new(self.key_store.clone());

        // Check if we need to create a sender key and return SKDM for distribution
        let skdm_to_distribute = if !self.has_sender_key(channel_id)? {
            tracing::debug!(channel_id = %channel_id, "Creating sender key for channel");
            let skdm = SenderKeyManager::create_distribution_message(
                &our_address,
                distribution_id,
                &mut store,
            )
            .await
            .map_err(ServiceError::Crypto)?;

            Some(SenderKeyDistribution {
                channel_id: channel_id.to_string(),
                distribution_message: skdm.serialized().to_vec(),
                timestamp: chrono::Utc::now().timestamp_millis() as u64,
            })
        } else {
            None
        };

        let ciphertext =
            SenderKeyManager::encrypt(&our_address, distribution_id, plaintext, &mut store)
                .await
                .map_err(ServiceError::Crypto)?;

        tracing::trace!(
            channel_id = %channel_id,
            plaintext_len = plaintext.len(),
            ciphertext_len = ciphertext.serialized().len(),
            skdm_created = skdm_to_distribute.is_some(),
            "Encrypted message for group"
        );

        let group_encrypted = GroupEncrypted {
            channel_id: channel_id.to_string(),
            ciphertext: ciphertext.serialized().to_vec(),
            timestamp: chrono::Utc::now().timestamp_millis() as u64,
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
        let device_id = DeviceId::try_from(sender_device_id)
            .map_err(|_| ServiceError::Validation("Invalid device ID".into()))?;
        let sender_address = ProtocolAddress::new(sender_id.to_string(), device_id);

        let mut store = SignalStore::new(self.key_store.clone());

        let plaintext =
            SenderKeyManager::decrypt(&encrypted.ciphertext, &sender_address, &mut store)
                .await
                .map_err(ServiceError::Crypto)?;

        tracing::trace!(
            sender_id = %sender_id,
            channel_id = %encrypted.channel_id,
            plaintext_len = plaintext.len(),
            "Decrypted message from group"
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

        Ok(key.is_some())
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
