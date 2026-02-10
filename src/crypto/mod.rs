//! Cryptographic operations layer.
//!
//! All encryption, decryption, and key management operations.
//! Wraps libsignal with proper storage backends.

pub mod attachment_crypto;
pub mod blind_signature;
pub mod device_credentials;
pub mod dm_crypto;
pub mod group_encryption;
pub mod key_derivation;
pub mod rng;
pub mod saved_message_crypto;
pub mod sealed_sender;
pub mod signal;

pub use attachment_crypto::{AttachmentCrypto, EncryptedAttachment, KEY_SET_LENGTH};
pub use device_credentials::{derive_uak, DeviceCredentials, DeviceToken};
pub use dm_crypto::{decrypt_dm_bytes, decrypt_dm_message, encrypt_dm_bytes, encrypt_dm_message};
pub use saved_message_crypto::{SavedMessageCrypto, SavedMessageSource};
pub use signal::{IdentityKeyManager, PreKeyManager, SealedSenderManager, SenderKeyManager, SessionManager, SignalStore};
