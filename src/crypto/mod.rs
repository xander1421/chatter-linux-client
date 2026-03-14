//! Cryptographic operations layer.
//!
//! All encryption, decryption, and key management operations.
//! Wraps libsignal with proper storage backends.
//!
//! Note: For blind signatures, use `crate::mailbox::blind_signature` which implements
//! deterministic RSABSSA per NDSS '21 spec, compatible with the Go server.

pub mod attachment_crypto;
pub mod attachment_crypto_fast;
pub mod attachment_crypto_portable;
pub mod call_key_provider;
pub mod device_credentials;
pub mod dm_crypto;
pub mod ecies;
pub mod envelope_crypto;
pub mod group_encryption;
pub mod key_derivation;
pub mod profile_crypto;
pub mod rng;
pub mod saved_message_crypto;
pub mod sealed_sender;
pub mod secret_key;
pub mod secret_string;
pub mod secure_buffer;
pub mod signal;
pub mod streaming_decrypt;
pub mod svr;

pub use attachment_crypto::{AttachmentCrypto, EncryptedAttachment, KEY_SET_LENGTH};
pub use call_key_provider::{
    derive_call_key, derive_call_shared_secret, CallKeyState,
    // Ephemeral key exchange (Signal-style PFS for DM calls)
    EphemeralCallKey, derive_call_key_ephemeral,
    // Group call key generation
    generate_group_call_key,
};
pub use device_credentials::{derive_uak, DeviceCredentials, DeviceToken};
pub use dm_crypto::{decrypt_dm_bytes, decrypt_dm_message, encrypt_dm_bytes, encrypt_dm_message};
pub use envelope_crypto::{decrypt_envelope, ed25519_to_x25519_public, encrypt_envelope};
pub use saved_message_crypto::{SavedMessageCrypto, SavedMessageSource};
pub use secret_key::{SecretKey32, SecretKey64};
pub use secret_string::SecretString;
pub use secure_buffer::{SecureBuffer, SecureCursor};
pub use signal::{IdentityKeyManager, PreKeyManager, SealedSenderManager, SenderKeyManager, SessionManager, SignalStore};
pub use ecies::{encrypt as ecies_encrypt, decrypt as ecies_decrypt, EciesMessage, EciesError};
