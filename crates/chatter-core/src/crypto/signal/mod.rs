//! Signal Protocol implementation.
//!
//! Core Signal Protocol cryptographic operations.

pub mod identity;
pub mod prekey;
pub mod sealed_sender;
pub mod sender_key;
pub mod session;
pub mod store;

pub use identity::IdentityKeyManager;
pub use prekey::PreKeyManager;
pub use sealed_sender::{
    ContentHint, SealedSenderDecryptionResult, SealedSenderManager, SenderCertificate,
    ServerCertificate, UnidentifiedSenderMessageContent,
};
pub use sender_key::SenderKeyManager;
pub use session::SessionManager;
pub use store::SignalStore;

use crate::error::{CryptoError, IdentityChangeSource};
use libsignal_protocol::ProtocolAddress;

/// Build [`CryptoError::UntrustedIdentity`] from libsignal's address-only
/// `SignalProtocolError::UntrustedIdentity`.
///
/// Centralised because `peer_id` must be `address.name()` (identity keys are
/// pinned per ACCOUNT — see `store::SignalStore::is_trusted_identity`) and never
/// the "account.device" `Display` form. The four call sites must not disagree
/// about that split.
pub(crate) fn untrusted_identity(
    address: &ProtocolAddress,
    new_identity_key: Option<Vec<u8>>,
    attribution: IdentityChangeSource,
) -> CryptoError {
    CryptoError::UntrustedIdentity {
        peer_id: address.name().to_string(),
        device_id: address.device_id().into(),
        new_identity_key,
        attribution,
    }
}
