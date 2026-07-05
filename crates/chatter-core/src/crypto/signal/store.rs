//! Signal Protocol store.
//!
//! SignalProtocolStore implementation for identity, prekey, session, and sender key storage.
//! Bridges our SQLite-based KeyStore to libsignal's async store traits.

use async_trait::async_trait;
use libsignal_protocol::{
    CiphertextMessageType, Direction, GenericSignedPreKey, IdentityChange, IdentityKey,
    IdentityKeyPair, IdentityKeyStore, KyberPreKeyId, KyberPreKeyRecord, KyberPreKeyStore,
    PreKeyId, PreKeyRecord, PreKeyStore, ProtocolAddress, ProtocolStore, PublicKey,
    SenderKeyRecord, SenderKeyStore, SessionRecord, SessionStore, SignalProtocolError,
    SignedPreKeyId, SignedPreKeyRecord, SignedPreKeyStore,
};
use std::sync::Arc;
use uuid::Uuid;

use crate::error::StorageError;
use crate::storage::KeyStore;

/// Result type for Signal Protocol operations.
type Result<T> = std::result::Result<T, SignalProtocolError>;

/// A simple error wrapper that implements Send + Sync + UnwindSafe.
///
/// libsignal's ApplicationCallbackError requires UnwindSafe, but rusqlite::Error
/// does not implement it. We wrap the error message as a string to satisfy this.
#[derive(Debug)]
struct StorageCallbackError(String);

impl std::fmt::Display for StorageCallbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for StorageCallbackError {}

/// Signal Protocol store wrapping all key storage.
///
/// This struct implements all 6 libsignal storage traits by delegating to our
/// SQLite-based KeyStore. It bridges the async trait interfaces that libsignal
/// expects with our synchronous database operations.
#[derive(Clone)]
pub struct SignalStore {
    key_store: Arc<KeyStore>,
}

impl SignalStore {
    /// Create a new SignalStore wrapping the given KeyStore.
    pub fn new(key_store: Arc<KeyStore>) -> Self {
        Self { key_store }
    }

    /// Get the underlying KeyStore.
    pub fn key_store(&self) -> Arc<KeyStore> {
        self.key_store.clone()
    }
}

/// Convert a ProtocolAddress to the string format used by KeyStore.
///
/// Format: "name:device_id"
fn address_to_string(address: &ProtocolAddress) -> String {
    format!("{}:{}", address.name(), address.device_id())
}

/// Convert a StorageError to a SignalProtocolError.
fn storage_to_signal_error(e: StorageError) -> SignalProtocolError {
    SignalProtocolError::ApplicationCallbackError(
        "storage",
        Box::new(StorageCallbackError(e.to_string())),
    )
}

// =============================================================================
// IdentityKeyStore
// =============================================================================

#[async_trait(?Send)]
impl IdentityKeyStore for SignalStore {
    async fn get_identity_key_pair(&self) -> Result<IdentityKeyPair> {
        let (_, key_pair_bytes) = self
            .key_store
            .get_local_identity()
            .map_err(storage_to_signal_error)?
            .ok_or(SignalProtocolError::InvalidState(
                "get_identity_key_pair",
                "no local identity".to_string(),
            ))?;

        IdentityKeyPair::try_from(key_pair_bytes.as_slice())
            .map_err(|_| SignalProtocolError::InvalidProtobufEncoding)
    }

    async fn get_local_registration_id(&self) -> Result<u32> {
        let (registration_id, _) = self
            .key_store
            .get_local_identity()
            .map_err(storage_to_signal_error)?
            .ok_or(SignalProtocolError::InvalidState(
                "get_local_registration_id",
                "no local identity".to_string(),
            ))?;

        Ok(registration_id)
    }

    async fn save_identity(
        &mut self,
        address: &ProtocolAddress,
        identity: &IdentityKey,
    ) -> Result<IdentityChange> {
        let address_str = address_to_string(address);
        let key_changed = self
            .key_store
            .save_identity(&address_str, &identity.serialize())
            .map_err(storage_to_signal_error)?;

        // If identity key changed, archive the old session (not delete).
        // archive_current_state() moves the current session to previous_sessions,
        // preserving it so in-transit messages under the old key can still decrypt.
        // This matches Signal-Desktop behavior and forces new session establishment.
        if key_changed {
            tracing::warn!(
                address = %address_str,
                "Identity key changed - archiving session for re-establishment"
            );
            // The peer's safety number no longer matches the new key, so any
            // prior manual verification is stale. Clear it so the peer must be
            // re-verified — this is the MITM/re-registration signal the
            // verification system exists to surface. Keyed on the bare user-id
            // (address.name()), not the "user.device" address_str.
            self.key_store
                .unverify_peer(address.name())
                .map_err(storage_to_signal_error)?;
            if let Some(record_bytes) = self
                .key_store
                .load_session(&address_str)
                .map_err(storage_to_signal_error)?
            {
                let mut record = SessionRecord::deserialize(&record_bytes)?;
                record.archive_current_state()?;
                self.key_store
                    .store_session(&address_str, &record.serialize()?)
                    .map_err(storage_to_signal_error)?;
            }
        }

        Ok(IdentityChange::from_changed(key_changed))
    }

    async fn is_trusted_identity(
        &self,
        address: &ProtocolAddress,
        identity: &IdentityKey,
        _direction: Direction,
    ) -> Result<bool> {
        let address_str = address_to_string(address);

        // TOFU (Trust On First Use): if no stored key, trust it
        let stored_key = self
            .key_store
            .get_identity(&address_str)
            .map_err(storage_to_signal_error)?;

        match stored_key {
            None => Ok(true), // First use, trust it
            Some(bytes) => {
                let stored_identity = IdentityKey::try_from(bytes.as_slice())
                    .map_err(|_| SignalProtocolError::InvalidProtobufEncoding)?;
                Ok(&stored_identity == identity)
            }
        }
    }

    async fn get_identity(&self, address: &ProtocolAddress) -> Result<Option<IdentityKey>> {
        let address_str = address_to_string(address);

        let stored_key = self
            .key_store
            .get_identity(&address_str)
            .map_err(storage_to_signal_error)?;

        match stored_key {
            None => Ok(None),
            Some(bytes) => {
                let identity = IdentityKey::try_from(bytes.as_slice())
                    .map_err(|_| SignalProtocolError::InvalidProtobufEncoding)?;
                Ok(Some(identity))
            }
        }
    }
}

// =============================================================================
// PreKeyStore
// =============================================================================

#[async_trait(?Send)]
impl PreKeyStore for SignalStore {
    async fn get_pre_key(&self, prekey_id: PreKeyId) -> Result<PreKeyRecord> {
        let id: u32 = prekey_id.into();
        let record_bytes = self
            .key_store
            .get_prekey(id)
            .map_err(storage_to_signal_error)?
            .ok_or(SignalProtocolError::InvalidPreKeyId)?;

        PreKeyRecord::deserialize(&record_bytes)
    }

    async fn save_pre_key(&mut self, prekey_id: PreKeyId, record: &PreKeyRecord) -> Result<()> {
        let id: u32 = prekey_id.into();
        self.key_store
            .save_prekey(id, &record.serialize()?)
            .map_err(storage_to_signal_error)?;
        Ok(())
    }

    async fn remove_pre_key(&mut self, prekey_id: PreKeyId) -> Result<()> {
        let id: u32 = prekey_id.into();
        self.key_store
            .remove_prekey(id)
            .map_err(storage_to_signal_error)?;
        Ok(())
    }
}

// =============================================================================
// SignedPreKeyStore
// =============================================================================

#[async_trait(?Send)]
impl SignedPreKeyStore for SignalStore {
    async fn get_signed_pre_key(&self, signed_prekey_id: SignedPreKeyId) -> Result<SignedPreKeyRecord> {
        let id: u32 = signed_prekey_id.into();
        let record_bytes = self
            .key_store
            .get_signed_prekey(id)
            .map_err(storage_to_signal_error)?
            .ok_or(SignalProtocolError::InvalidSignedPreKeyId)?;

        SignedPreKeyRecord::deserialize(&record_bytes)
    }

    async fn save_signed_pre_key(
        &mut self,
        signed_prekey_id: SignedPreKeyId,
        record: &SignedPreKeyRecord,
    ) -> Result<()> {
        let id: u32 = signed_prekey_id.into();
        self.key_store
            .save_signed_prekey(id, &record.serialize()?)
            .map_err(storage_to_signal_error)?;
        Ok(())
    }
}

// =============================================================================
// KyberPreKeyStore
// =============================================================================

#[async_trait(?Send)]
impl KyberPreKeyStore for SignalStore {
    async fn get_kyber_pre_key(&self, kyber_prekey_id: KyberPreKeyId) -> Result<KyberPreKeyRecord> {
        let id: u32 = kyber_prekey_id.into();
        let record_bytes = self
            .key_store
            .get_kyber_prekey(id)
            .map_err(storage_to_signal_error)?
            .ok_or(SignalProtocolError::InvalidKyberPreKeyId)?;

        KyberPreKeyRecord::deserialize(&record_bytes)
    }

    async fn save_kyber_pre_key(
        &mut self,
        kyber_prekey_id: KyberPreKeyId,
        record: &KyberPreKeyRecord,
    ) -> Result<()> {
        let id: u32 = kyber_prekey_id.into();
        self.key_store
            .save_kyber_prekey(id, &record.serialize()?)
            .map_err(storage_to_signal_error)?;
        Ok(())
    }

    async fn mark_kyber_pre_key_used(
        &mut self,
        kyber_prekey_id: KyberPreKeyId,
        ec_prekey_id: SignedPreKeyId,
        base_key: &PublicKey,
    ) -> Result<()> {
        let kyber_id: u32 = kyber_prekey_id.into();
        let ec_id: u32 = ec_prekey_id.into();

        self.key_store
            .mark_kyber_prekey_used(kyber_id, ec_id, &base_key.serialize())
            .map_err(|e| {
                // Check if this is a key reuse error
                if let StorageError::InvalidData(_) = e {
                    SignalProtocolError::InvalidMessage(
                        CiphertextMessageType::PreKey,
                        "reused base key",
                    )
                } else {
                    storage_to_signal_error(e)
                }
            })?;
        Ok(())
    }
}

// =============================================================================
// SessionStore
// =============================================================================

#[async_trait(?Send)]
impl SessionStore for SignalStore {
    async fn load_session(&self, address: &ProtocolAddress) -> Result<Option<SessionRecord>> {
        let address_str = address_to_string(address);

        let record_bytes = self
            .key_store
            .load_session(&address_str)
            .map_err(storage_to_signal_error)?;

        match record_bytes {
            None => Ok(None),
            Some(bytes) => {
                let record = SessionRecord::deserialize(&bytes)?;
                Ok(Some(record))
            }
        }
    }

    async fn store_session(
        &mut self,
        address: &ProtocolAddress,
        record: &SessionRecord,
    ) -> Result<()> {
        let address_str = address_to_string(address);
        self.key_store
            .store_session(&address_str, &record.serialize()?)
            .map_err(storage_to_signal_error)?;
        Ok(())
    }
}

// =============================================================================
// SenderKeyStore
// =============================================================================

#[async_trait(?Send)]
impl SenderKeyStore for SignalStore {
    async fn store_sender_key(
        &mut self,
        sender: &ProtocolAddress,
        distribution_id: Uuid,
        record: &SenderKeyRecord,
    ) -> Result<()> {
        let sender_str = address_to_string(sender);
        let distribution_str = distribution_id.to_string();

        self.key_store
            .store_sender_key(&sender_str, &distribution_str, &record.serialize()?)
            .map_err(storage_to_signal_error)?;
        Ok(())
    }

    async fn load_sender_key(
        &mut self,
        sender: &ProtocolAddress,
        distribution_id: Uuid,
    ) -> Result<Option<SenderKeyRecord>> {
        let sender_str = address_to_string(sender);
        let distribution_str = distribution_id.to_string();

        let record_bytes = self
            .key_store
            .load_sender_key(&sender_str, &distribution_str)
            .map_err(storage_to_signal_error)?;

        match record_bytes {
            None => Ok(None),
            Some(bytes) => {
                let record = SenderKeyRecord::deserialize(&bytes)?;
                Ok(Some(record))
            }
        }
    }
}

// =============================================================================
// ProtocolStore (marker trait)
// =============================================================================

impl ProtocolStore for SignalStore {}
