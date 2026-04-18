//! Generic key derivation helpers.
//!
//! Thin wrappers around `hkdf`/`sha2` so callers don't need to depend on those
//! crates directly. Use with a fixed-length output array:
//!
//! ```ignore
//! use chatter_core::crypto::kdf::hkdf_sha256;
//! let key: [u8; 32] = hkdf_sha256(ikm, None, b"my-info")?;
//! ```
//!
//! For password-derived profile keys + username hashing, prefer
//! [`super::auth_crypto`]; those helpers carry domain-specific info strings
//! that must not drift.

use hkdf::Hkdf;
use sha2::Sha256;

use crate::error::CryptoError;

/// HKDF-SHA256 extract + expand into a fixed-size byte array.
///
/// `salt = None` uses the all-zero salt per RFC 5869 §3.1. `info` provides
/// domain separation — always pick a unique byte string per call site.
pub fn hkdf_sha256<const N: usize>(
    ikm: &[u8],
    salt: Option<&[u8]>,
    info: &[u8],
) -> Result<[u8; N], CryptoError> {
    let hk = Hkdf::<Sha256>::new(salt, ikm);
    let mut out = [0u8; N];
    hk.expand(info, &mut out)
        .map_err(|_| CryptoError::KeyDerivationFailed(format!("HKDF expand failed ({} bytes)", N)))?;
    Ok(out)
}
