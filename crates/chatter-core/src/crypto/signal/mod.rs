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
