//! Shared constants and validation for Signal-compatible attachment crypto.
//!
//! Used by `attachment_crypto`, `attachment_crypto_fast`, `attachment_crypto_portable`,
//! and `streaming_decrypt` to avoid duplicating crypto format definitions.

use crate::error::CryptoError;

/// AES block size and IV length.
pub const IV_LENGTH: usize = 16;

/// AES-256 key length.
pub const AES_KEY_LENGTH: usize = 32;

/// HMAC-SHA256 key length.
pub const MAC_KEY_LENGTH: usize = 32;

/// HMAC-SHA256 output length.
pub const HMAC_LENGTH: usize = 32;

/// Combined key length (AES key + MAC key).
pub const KEY_SET_LENGTH: usize = AES_KEY_LENGTH + MAC_KEY_LENGTH;

/// Minimum ciphertext length: IV + one AES block + HMAC.
pub const MIN_CIPHERTEXT_LENGTH: usize = IV_LENGTH + 16 + HMAC_LENGTH;

/// Validated and split ciphertext components (borrowed from input).
pub struct CiphertextParts<'a> {
    pub aes_key: &'a [u8],
    pub mac_key: &'a [u8],
    pub iv: &'a [u8],
    pub encrypted: &'a [u8],
    pub expected_hmac: &'a [u8],
}

/// Validate ciphertext length and split key + ciphertext into components.
///
/// Returns an error if the ciphertext is too short. This replaces the
/// identical validation block that was duplicated across all attachment
/// crypto implementations.
pub fn validate_and_split<'a>(
    key: &'a [u8],
    ciphertext: &'a [u8],
) -> Result<CiphertextParts<'a>, CryptoError> {
    if ciphertext.len() < MIN_CIPHERTEXT_LENGTH {
        return Err(CryptoError::AttachmentDecryptionFailed(
            "ciphertext too short".to_string(),
        ));
    }

    Ok(CiphertextParts {
        aes_key: &key[..AES_KEY_LENGTH],
        mac_key: &key[AES_KEY_LENGTH..],
        iv: &ciphertext[..IV_LENGTH],
        encrypted: &ciphertext[IV_LENGTH..ciphertext.len() - HMAC_LENGTH],
        expected_hmac: &ciphertext[ciphertext.len() - HMAC_LENGTH..],
    })
}
