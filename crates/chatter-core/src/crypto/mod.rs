//! Cryptographic operations layer.
//!
//! All encryption, decryption, and key management operations.
//! Wraps libsignal with proper storage backends.
//!
//! Note: For blind signatures, use `crate::mailbox::blind_signature` which implements
//! deterministic RSABSSA per NDSS '21 spec, compatible with the Go server.

pub mod attachment_crypto;
pub mod attachment_crypto_fast;
pub(crate) mod hmac_compat;
pub mod auth_crypto;
pub mod call_key_provider;
pub mod device_credentials;
pub mod device_name;
pub mod dm_crypto;
pub mod ecies;
pub mod envelope_crypto;
pub mod kdf;
pub mod opaque;
pub mod keytransparency;
pub mod profile_crypto;
pub mod provisioning;
pub mod secret_key;
pub mod secret_string;
pub mod secure_buffer;
pub mod signal;
pub mod streaming_decrypt;
pub mod message_backup;
pub mod svr;
pub mod svr_backup;
pub mod vrf;

pub use attachment_crypto::{AttachmentCrypto, EncryptedAttachment, StreamingEncryptResult, KEY_SET_LENGTH, max_attachment_size, set_max_attachment_size};
pub use call_key_provider::{
    derive_call_key, CallKeyState,
    // Ephemeral key exchange (Signal-style PFS for DM calls)
    EphemeralCallKey, derive_call_key_ephemeral,
    // Group call key generation
    generate_group_call_key,
};
pub use device_credentials::{derive_uak, DeviceCredentials, DeviceToken};
pub use keytransparency::{
    verify_consistency_proof as verify_kt_consistency_proof, verify_proof as verify_kt_proof,
    KtError,
};
pub use dm_crypto::{decrypt_dm_bytes, decrypt_dm_message, encrypt_dm_bytes, encrypt_dm_message};
pub use envelope_crypto::{decrypt_envelope, encrypt_envelope, generate_mailbox_encryption_key};
pub use secret_key::{SecretKey32, SecretKey64};
pub use secret_string::SecretString;
pub use secure_buffer::{SecureBuffer, SecureCursor};
pub use signal::{IdentityKeyManager, PreKeyManager, SealedSenderManager, SenderKeyManager, SessionManager, SignalStore};
pub use provisioning::{
    decrypt_envelope as decrypt_provision_envelope,
    encrypt_envelope as encrypt_provision_envelope, ProvisioningKeypair,
};
