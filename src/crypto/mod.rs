//! Cryptographic operations layer.
//!
//! All encryption, decryption, and key management operations.
//! Wraps libsignal with proper storage backends.

pub mod attachment_crypto;
pub mod device_credentials;
pub mod group_encryption;
pub mod key_derivation;
pub mod rng;
pub mod sealed_sender;
pub mod signal;

pub use signal::{IdentityKeyManager, SessionManager, SignalStore};
