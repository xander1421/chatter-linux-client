//! Signal Actor - Dedicated thread for libsignal-protocol operations.
//!
//! Encapsulates all non-Send/Sync libsignal operations into a single-threaded
//! actor, providing an async handle for the rest of the application.

use libsignal_protocol::{
    CiphertextMessageType, DeviceId, PreKeyBundle, ProtocolAddress, PublicKey, SenderCertificate,
    SessionRecord,
};
use rand::TryRngCore;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::crypto::{
    SealedSenderManager, SessionManager, SignalStore, signal::sender_key::SenderKeyManager,
};
use crate::domain::ids::{ChannelId, UserId};
use crate::error::{CryptoError, ServiceError};
use crate::storage::KeyStore;
use crate::storage::repositories::settings_repo::SettingsRepository;

/// Result is returned only after receive state is durably committed.
#[derive(Debug)]
pub struct DurableReceive {
    pub address: ProtocolAddress,
    pub content: Vec<u8>,
    pub msg_type: CiphertextMessageType,
    pub artifact: Option<crate::storage::receive_artifact::ReceiveArtifact>,
}

/// Typed rejection prevents callers guessing crypto commit safety from text.
#[derive(Debug, thiserror::Error)]
pub enum DurableReceiveError {
    #[error("unratcheted anonymous receive plaintext rejected")]
    PlaintextRejected,
    #[error(transparent)]
    Failure(#[from] ServiceError),
}
impl From<crate::error::StorageError> for DurableReceiveError {
    fn from(error: crate::error::StorageError) -> Self {
        Self::Failure(ServiceError::Storage(error))
    }
}

/// Every plaintext this actor encrypts for a peer (sealed, identified or
/// sender-key) is a `content_v1::Content`. Refuse one that breaks the rules
/// declared in content.proto, which every receiving client enforces, before it
/// reaches the ratchet: the caller gets an error instead of a peer silently
/// dropping the message. DecryptionErrorMessage reports are not Content and go
/// through `encrypt_decryption_error_sealed`, which this does not gate.
fn admit_outgoing_content(plaintext: &[u8]) -> Result<(), ServiceError> {
    use crate::network::validation::protovalidate::{ValidatedContent, validate_encoded};
    validate_encoded(
        <crate::proto::content_v1::Content as ValidatedContent>::FULL_NAME,
        plaintext,
    )
    .map_err(|e| ServiceError::Validation(format!("outgoing content refused: {e}")))
}

/// Map an identified-sender decrypt failure.
///
/// `UntrustedIdentity` must survive as its own error — collapsed into
/// `Protocol` it is indistinguishable from a parse failure and the accept path
/// can never be offered. The sender here is server-supplied envelope metadata,
/// so that is the provenance recorded.
fn map_identified_decrypt_err(
    e: libsignal_protocol::SignalProtocolError,
    new_identity_key: Option<Vec<u8>>,
) -> ServiceError {
    if let libsignal_protocol::SignalProtocolError::UntrustedIdentity(ref address) = e {
        return ServiceError::Crypto(crate::crypto::signal::untrusted_identity(
            address,
            new_identity_key,
            crate::error::IdentityChangeSource::ServerClaimed,
        ));
    }
    ServiceError::Crypto(CryptoError::Protocol(e.to_string()))
}

mod commands;
mod handle;
mod runtime;

use commands::SignalCommand;

/// Our own `ProtocolAddress`, read from the authenticated session.
///
/// libsignal >= 0.99 takes the LOCAL address alongside the remote one on every
/// encrypt/decrypt/process-bundle call: a session is keyed by both ends, so the
/// caller has to say which identity of ours is involved. Guessing is not an
/// option here — a wrong local address silently addresses a different session —
/// so this fails loudly when the device is not logged in.
fn local_protocol_address(settings: &SettingsRepository) -> Result<ProtocolAddress, ServiceError> {
    let uuid = settings
        .get_string(crate::storage::settings_keys::CURRENT_USER_ID)
        .map_err(ServiceError::Storage)?
        .ok_or_else(|| {
            ServiceError::Crypto(CryptoError::Protocol(
                "no local user id: cannot build the local ProtocolAddress libsignal now requires"
                    .into(),
            ))
        })?;
    // Stored as an INTEGER, not a string: `get_string` on this key fails with
    // "invalid type: integer `1`, expected a string". libsignal >= 0.99 narrowed
    // DeviceId to u8, so the value is range-checked on the way in.
    let device_id: u32 = settings
        .get(crate::storage::settings_keys::CURRENT_DEVICE_ID)
        .map_err(ServiceError::Storage)?
        .ok_or_else(|| ServiceError::Crypto(CryptoError::Protocol("no local device id".into())))?;
    let device_id = u8::try_from(device_id).map_err(|_| {
        ServiceError::Crypto(CryptoError::Protocol(format!(
            "local device id {device_id} does not fit libsignal's u8 DeviceId"
        )))
    })?;
    let device_id = DeviceId::new(device_id)
        .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?;
    Ok(ProtocolAddress::new(uuid, device_id))
}

/// Handle to the Signal Actor.
#[derive(Clone)]

pub struct SignalActorHandle {
    sender: mpsc::Sender<SignalCommand>,
}

