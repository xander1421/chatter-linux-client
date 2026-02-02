//! Cryptographic operations layer.
//!
//! All encryption, decryption, and key management operations.
//! Wraps libsignal with proper storage backends.

pub mod signal;
pub mod sealed_sender;
pub mod group_encryption;
pub mod attachment_crypto;
pub mod key_derivation;
pub mod device_credentials;
pub mod rng;
