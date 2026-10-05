//! Optimized attachment decryption with full hardware acceleration.
//!
//! Features:
//! - Hardware-accelerated AES-256-CBC via OpenSSL (AES-NI)
//! - Hardware-accelerated SHA-256 via OpenSSL (SHA-NI)
//! - Hardware-accelerated HMAC-SHA256 via OpenSSL
//! - Parallel SHA256 + HMAC using rayon for large files
//!
//! ## Ordering
//!
//! Verify, then decrypt. AES used to run inside the same `rayon::join` as the
//! hashes, which produced plaintext for ciphertext that had not been
//! authenticated yet; CBC decrypts every untampered block correctly, so that
//! buffer held real plaintext even when the tags then failed.

use aes::cipher::block_padding::NoPadding;
use aes::cipher::{BlockDecryptMut, KeyIvInit};
use subtle::ConstantTimeEq;

use crate::crypto::hmac_compat::Sha256Stream;

type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;

use crate::error::CryptoError;

use super::attachment_crypto::KEY_SET_LENGTH;

const IV_LENGTH: usize = 16;
const AES_KEY_LENGTH: usize = 32;
const HMAC_LENGTH: usize = 32;
const MIN_CIPHERTEXT_LENGTH: usize = IV_LENGTH + 16 + HMAC_LENGTH;

/// Threshold for parallel verification (1MB)
/// Below this, sequential is faster due to thread overhead
const PARALLEL_THRESHOLD: usize = 1024 * 1024;

/// SHA-256 over the whole ciphertext and HMAC-SHA256 over `IV || encrypted`.
#[inline]
fn verify_sequential(
    ciphertext: &[u8],
    mac_key: &[u8],
    iv_end: usize,
    hmac_start: usize,
) -> ([u8; 32], [u8; 32]) {
    let mut digest_hasher = Sha256Stream::new().expect("SHA256 init failed");
    let mut hmac = crate::crypto::hmac_compat::hmac_init(mac_key).expect("HMAC init");

    // Process IV
    digest_hasher
        .update(&ciphertext[..iv_end])
        .expect("SHA256 update");
    hmac.update(&ciphertext[..iv_end]).expect("HMAC update");

    // Process encrypted data in cache-friendly chunks
    const CHUNK_SIZE: usize = 64 * 1024;
    for chunk in ciphertext[iv_end..hmac_start].chunks(CHUNK_SIZE) {
        digest_hasher.update(chunk).expect("SHA256 update");
        hmac.update(chunk).expect("HMAC update");
    }

    // HMAC bytes only for digest
    digest_hasher
        .update(&ciphertext[hmac_start..])
        .expect("SHA256 update");

    let digest_result = digest_hasher.finish().expect("SHA256 finalize");
    let hmac_result = hmac.finalize().expect("HMAC finalize");

    let mut actual_digest = [0u8; 32];
    actual_digest.copy_from_slice(&digest_result);
    let mut computed_hmac = [0u8; 32];
    computed_hmac.copy_from_slice(&hmac_result);

    (actual_digest, computed_hmac)
}

/// Same tags as [`verify_sequential`], with SHA-256 and HMAC on separate
/// rayon threads. AES is deliberately NOT part of this join: it runs only
/// after both tags have been checked.
fn verify_parallel(ciphertext: &[u8], mac_key: &[u8], hmac_start: usize) -> ([u8; 32], [u8; 32]) {
    rayon::join(
        || {
            let mut hasher = Sha256Stream::new().expect("SHA256 init");
            hasher.update(ciphertext).expect("SHA256"); // IV + encrypted + HMAC bytes
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&hasher.finish().expect("SHA256 finalize"));
            arr
        },
        || {
            let mut signer = crate::crypto::hmac_compat::hmac_init(mac_key).expect("HMAC init");
            signer.update(&ciphertext[..hmac_start]).expect("HMAC"); // IV + encrypted
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&signer.finalize().expect("HMAC finalize"));
            arr
        },
    )
}

/// Verify-then-decrypt, in place.
///
/// Checks the SHA-256 digest and the HMAC (in parallel with each other above
/// [`PARALLEL_THRESHOLD`]) and only then runs AES-256-CBC, overwriting the
/// ciphertext buffer it was handed. No plaintext exists for a ciphertext that
/// fails authentication, and no second buffer is allocated.
pub fn decrypt_zero_copy(
    mut ciphertext: Vec<u8>,
    key: &[u8; KEY_SET_LENGTH],
    expected_digest: &[u8; 32],
    plaintext_size: usize,
) -> Result<Vec<u8>, CryptoError> {
    // Verify minimum length
    if ciphertext.len() < MIN_CIPHERTEXT_LENGTH {
        return Err(CryptoError::AttachmentDecryptionFailed(
            "ciphertext too short".to_string(),
        ));
    }

    // Split key
    let aes_key = &key[..AES_KEY_LENGTH];
    let mac_key = &key[AES_KEY_LENGTH..];

    // Extract positions
    let iv_end = IV_LENGTH;
    let hmac_start = ciphertext.len() - HMAC_LENGTH;
    let data_len = hmac_start - iv_end;

    // For small files, use sequential processing (thread overhead not worth it)
    let (actual_digest, computed_hmac) = if data_len < PARALLEL_THRESHOLD {
        verify_sequential(&ciphertext, mac_key, iv_end, hmac_start)
    } else {
        verify_parallel(&ciphertext, mac_key, hmac_start)
    };

    // Verify digest (constant-time)
    if actual_digest.ct_eq(expected_digest).unwrap_u8() != 1 {
        return Err(CryptoError::AttachmentDigestMismatch);
    }

    // Verify HMAC (constant-time)
    if computed_hmac.ct_eq(&ciphertext[hmac_start..]).unwrap_u8() != 1 {
        return Err(CryptoError::AttachmentMacMismatch);
    }

    if plaintext_size > data_len {
        return Err(CryptoError::AttachmentDecryptionFailed(
            "plaintext_size exceeds decrypted data".to_string(),
        ));
    }

    // Authenticated. Raw AES-256-CBC, no block padding (caller pads).
    let mut iv = [0u8; IV_LENGTH];
    iv.copy_from_slice(&ciphertext[..IV_LENGTH]);
    Aes256CbcDec::new_from_slices(aes_key, &iv)
        .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("AES init: {e}")))?
        .decrypt_padded_mut::<NoPadding>(&mut ciphertext[iv_end..hmac_start])
        .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("AES decrypt: {e}")))?;

    // Drop the padding and HMAC from the tail, then the IV from the head.
    ciphertext.truncate(iv_end + plaintext_size);
    ciphertext.drain(..iv_end);

    Ok(ciphertext)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::attachment_crypto::AttachmentCrypto;

    #[test]
    fn test_zero_copy_matches_original() {
        let plaintext = b"Hello, this is a test for zero-copy decryption!";

        let encrypted = AttachmentCrypto::encrypt(plaintext).unwrap();

        let decrypted = decrypt_zero_copy(
            encrypted.ciphertext.clone(),
            &encrypted.key,
            &encrypted.digest,
            encrypted.plaintext_size,
        )
        .unwrap();

        assert_eq!(decrypted.as_slice(), plaintext);
    }

    #[test]
    fn test_zero_copy_large_file() {
        // 1MB test - ensures zero-copy works for large files
        let plaintext: Vec<u8> = (0..1024 * 1024).map(|i| (i % 256) as u8).collect();

        let encrypted = AttachmentCrypto::encrypt(&plaintext).unwrap();

        let decrypted = decrypt_zero_copy(
            encrypted.ciphertext.clone(),
            &encrypted.key,
            &encrypted.digest,
            encrypted.plaintext_size,
        )
        .unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_zero_copy_digest_mismatch() {
        let plaintext = b"test";
        let encrypted = AttachmentCrypto::encrypt(plaintext).unwrap();

        let mut bad_digest = encrypted.digest;
        bad_digest[0] ^= 0xFF;

        let result = decrypt_zero_copy(
            encrypted.ciphertext.clone(),
            &encrypted.key,
            &bad_digest,
            encrypted.plaintext_size,
        );

        assert!(matches!(result, Err(CryptoError::AttachmentDigestMismatch)));
    }

    #[test]
    fn test_parallel_verification_large_file() {
        // 2MB test - triggers parallel path (> PARALLEL_THRESHOLD)
        let plaintext: Vec<u8> = (0..2 * 1024 * 1024).map(|i| (i % 256) as u8).collect();

        let encrypted = AttachmentCrypto::encrypt(&plaintext).unwrap();

        // This will use parallel SHA256 + HMAC
        let decrypted = decrypt_zero_copy(
            encrypted.ciphertext.clone(),
            &encrypted.key,
            &encrypted.digest,
            encrypted.plaintext_size,
        )
        .unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_sequential_verification_small_file() {
        // 100KB test - uses sequential path (< PARALLEL_THRESHOLD)
        let plaintext: Vec<u8> = (0..100 * 1024).map(|i| (i % 256) as u8).collect();

        let encrypted = AttachmentCrypto::encrypt(&plaintext).unwrap();

        let decrypted = decrypt_zero_copy(
            encrypted.ciphertext.clone(),
            &encrypted.key,
            &encrypted.digest,
            encrypted.plaintext_size,
        )
        .unwrap();

        assert_eq!(decrypted, plaintext);
    }

    /// Pins verify-then-decrypt on the parallel path. A body that is not
    /// block-aligned makes AES fail, so whichever of AES and authentication
    /// runs first decides the error: it must be the authentication one.
    #[test]
    fn parallel_path_authenticates_before_aes() {
        let plaintext: Vec<u8> = (0..2 * 1024 * 1024).map(|i| (i % 256) as u8).collect();
        let encrypted = AttachmentCrypto::encrypt(&plaintext).unwrap();

        let mut tampered = encrypted.ciphertext.clone();
        tampered.insert(IV_LENGTH, 0);
        assert!(tampered.len() - IV_LENGTH - HMAC_LENGTH >= PARALLEL_THRESHOLD);

        let result = decrypt_zero_copy(
            tampered,
            &encrypted.key,
            &encrypted.digest,
            encrypted.plaintext_size,
        );
        assert!(
            matches!(result, Err(CryptoError::AttachmentDigestMismatch)),
            "{result:?}"
        );
    }

    #[test]
    fn tampered_body_is_rejected_on_both_paths() {
        for size in [100 * 1024, 2 * 1024 * 1024] {
            let plaintext = vec![7u8; size];
            let encrypted = AttachmentCrypto::encrypt(&plaintext).unwrap();
            let mut tampered = encrypted.ciphertext.clone();
            tampered[IV_LENGTH + 5] ^= 1;

            // Digest recomputed over the tampered bytes so the digest check
            // passes and the HMAC check is the one under test.
            let digest = AttachmentCrypto::compute_digest(&tampered);
            let result =
                decrypt_zero_copy(tampered, &encrypted.key, &digest, encrypted.plaintext_size);
            assert!(
                matches!(result, Err(CryptoError::AttachmentMacMismatch)),
                "{result:?}"
            );
        }
    }
}
