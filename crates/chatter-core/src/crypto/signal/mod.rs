//! Signal Protocol implementation.
//!
//! Core Signal Protocol cryptographic operations.

pub mod identity;
pub mod prekey;
pub mod sealed_sender;
pub mod sender_key;
pub mod sesame;
pub mod session;
pub mod store;

pub use identity::IdentityKeyManager;
pub use prekey::PreKeyManager;
pub use sealed_sender::{
    ContentHint, SealedSenderDecryptionResult, SealedSenderManager, SenderCertificate,
    ServerCertificate, UnidentifiedSenderMessageContent,
};
pub use sender_key::SenderKeyManager;
pub use sesame::{
    ConvergenceResult, DeviceRecord, PendingMessage, SesameManager, SesameStore, SessionEntry,
    SqliteSesameStore, UserRecord, MAX_INACTIVE_SESSIONS, MAX_PENDING_MESSAGES, PENDING_MESSAGE_TTL_SECS,
};
pub use session::SessionManager;
pub use store::SignalStore;

use libsignal_protocol::ProtocolAddress;
use crate::error::CryptoError;

/// Build [`CryptoError::UntrustedIdentity`] from libsignal's address-only
/// `SignalProtocolError::UntrustedIdentity`.
///
/// libsignal raises that error at four sites (session.rs:61/186,
/// session_cipher.rs:145/317) and carries only the address, so every caller has
/// to re-attach the new key from whatever it was holding — the failing
/// PreKeySignalMessage or the prekey bundle. Centralised here so the three
/// call sites cannot disagree about the account/device split, which is
/// load-bearing: identity keys are pinned per ACCOUNT (see
/// `store::SignalStore::is_trusted_identity`), so `peer_id` must be
/// `address.name()` and never the "account.device" `Display` form.
pub(crate) fn untrusted_identity(
    address: &ProtocolAddress,
    new_identity_key: Option<Vec<u8>>,
) -> CryptoError {
    CryptoError::UntrustedIdentity {
        peer_id: address.name().to_string(),
        device_id: address.device_id().into(),
        new_identity_key,
    }
}
