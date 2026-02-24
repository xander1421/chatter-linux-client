//! Cryptographic operations layer.
//!
//! All encryption, decryption, and key management operations.
//! Wraps libsignal with proper storage backends.
//!
//! Note: For blind signatures, use `crate::mailbox::blind_signature` which implements
//! deterministic RSABSSA per NDSS '21 spec, compatible with the Go server.

pub mod attachment_crypto;
pub mod call_key_provider;
pub mod device_credentials;
pub mod dm_crypto;
pub mod envelope_crypto;
pub mod group_encryption;
pub mod key_derivation;
pub mod rng;
pub mod saved_message_crypto;
pub mod sealed_sender;
pub mod secure_buffer;
pub mod signal;

pub use attachment_crypto::{AttachmentCrypto, EncryptedAttachment, KEY_SET_LENGTH};
pub use call_key_provider::{derive_call_key, derive_call_shared_secret, CallKeyState};
pub use device_credentials::{derive_uak, DeviceCredentials, DeviceToken};
pub use dm_crypto::{decrypt_dm_bytes, decrypt_dm_message, encrypt_dm_bytes, encrypt_dm_message};
pub use envelope_crypto::{decrypt_envelope, ed25519_to_x25519_public, encrypt_envelope};
pub use saved_message_crypto::{SavedMessageCrypto, SavedMessageSource};
pub use secure_buffer::{SecureBuffer, SecureCursor};
pub use signal::{IdentityKeyManager, PreKeyManager, SealedSenderManager, SenderKeyManager, SessionManager, SignalStore};
