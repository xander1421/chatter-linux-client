//! Attachment encryption/decryption.
//!
//! Signal-compatible file attachment encryption using AES-256-CBC + HMAC-SHA256.
//!
//! ## Format
//!
//! - **Key:** 64 bytes = [AES key (32)] + [MAC key (32)]
//! - **Ciphertext:** IV (16) || AES-CBC(padded plaintext) || HMAC-SHA256 (32)
//! - **Digest:** SHA256(entire ciphertext)
//!
//! ## Padding
//!
//! Uses log-bucketing to prevent file size inference:
//! `padded_size = max(541, ceil(1.05^ceil(log(size)/log(1.05))))`
//!
//! ## Cross-Platform Hardware Acceleration
//!
//! Uses OpenSSL for hardware-accelerated crypto on all platforms:
//! - **AES-256-CBC**: AES-NI (x86_64), ARM Crypto Extensions
//! - **SHA-256**: SHA-NI (x86_64), ARM SHA2
//! - **HMAC-SHA256**: Hardware-accelerated via OpenSSL
//!
//! ## Performance (AMD Ryzen 7 5800X)
//!
//! - AES-256-CBC: ~2 GB/s (AES-NI)
//! - SHA-256: ~2.2 GB/s (SHA-NI)
//! - Full pipeline: ~1 GB/s

use openssl::hash::{Hasher, MessageDigest};
use openssl::pkey::PKey;
use openssl::sign::Signer;
use openssl::symm::{Cipher, Crypter, Mode};
use rand::RngCore;
use subtle::ConstantTimeEq;
use zeroize::{ZeroizeOnDrop, Zeroizing};

use crate::crypto::secure_buffer::SecureBuffer;
use crate::error::CryptoError;

/// AES block size and IV length.
const IV_LENGTH: usize = 16;

/// AES-256 key length.
const AES_KEY_LENGTH: usize = 32;

/// HMAC-SHA256 key length.
const MAC_KEY_LENGTH: usize = 32;

/// HMAC-SHA256 output length.
const HMAC_LENGTH: usize = 32;

/// Combined key length (AES key + MAC key).
pub const KEY_SET_LENGTH: usize = AES_KEY_LENGTH + MAC_KEY_LENGTH;

/// Minimum ciphertext length: IV + one AES block + HMAC.
const MIN_CIPHERTEXT_LENGTH: usize = IV_LENGTH + 16 + HMAC_LENGTH;

/// Minimum padded size for log-bucketing.
const MIN_PADDED_SIZE: usize = 541;

/// Log-bucketing base (1.05).
const BUCKET_BASE: f64 = 1.05;

/// Encrypted attachment with metadata.
///
/// Contains the encrypted ciphertext, the encryption key (which should be
/// included in the E2E encrypted message), the digest for verification,
/// and the original plaintext size for proper decryption.
///
/// # Security
///
/// The encryption key is automatically zeroized when this struct is dropped,
/// preventing key material from persisting in memory.
#[derive(Debug, Clone, ZeroizeOnDrop)]
pub struct EncryptedAttachment {
    /// Encrypted data: IV (16) || AES-CBC ciphertext || HMAC-SHA256 (32)
    #[zeroize(skip)]
    pub ciphertext: Vec<u8>,

    /// Combined encryption key: [AES key (32)] + [MAC key (32)]
    /// This key should be included in the E2E encrypted message, never sent to server.
    /// Automatically zeroized on drop.
    pub key: [u8; KEY_SET_LENGTH],

    /// SHA-256 digest of the complete ciphertext.
    /// Used to verify integrity before decryption.
    #[zeroize(skip)]
    pub digest: [u8; 32],

    /// Original plaintext size in bytes.
    /// Required to remove padding after decryption.
    #[zeroize(skip)]
    pub plaintext_size: usize,
}

/// Result of streaming file-to-file encryption.
///
/// Contains the key material and digest needed to build an `AttachmentPointer`,
/// without holding the ciphertext in memory.
#[derive(Debug, ZeroizeOnDrop)]
pub struct StreamingEncryptResult {
    /// Combined encryption key: [AES key (32)] + [MAC key (32)].
    pub key: [u8; KEY_SET_LENGTH],
    /// SHA-256 digest of the complete ciphertext (IV + encrypted + HMAC).
    #[zeroize(skip)]
    pub digest: [u8; 32],
    /// Total ciphertext size in bytes.
    #[zeroize(skip)]
    pub ciphertext_size: u64,
    /// Original plaintext size in bytes.
    #[zeroize(skip)]
    pub plaintext_size: u64,
    /// Concatenated per-chunk HMAC-SHA256 digests (32 bytes each) for incremental verification.
    /// Empty if plaintext <= INCREMENTAL_MAC_THRESHOLD.
    #[zeroize(skip)]
    pub incremental_mac: Vec<u8>,
    /// Chunk size used for incremental MAC (64KB per Signal spec). 0 if no incremental MAC.
    #[zeroize(skip)]
    pub incremental_chunk_size: u32,
}

/// Chunk size for incremental MAC computation (64KB per Signal spec).
pub const INCREMENTAL_MAC_CHUNK_SIZE: usize = 64 * 1024;

/// Plaintext size threshold for incremental MAC (50KB per Signal spec).
const INCREMENTAL_MAC_THRESHOLD: u64 = 50 * 1024;

/// Attachment encryption service.
///
/// Provides Signal-compatible encryption using AES-256-CBC + HMAC-SHA256.
/// This is a stateless service - all state is passed as parameters.
pub struct AttachmentCrypto;

impl AttachmentCrypto {
    /// Encrypt plaintext with a randomly generated key.
    ///
    /// Generates a random 64-byte key, pads plaintext using log-bucketing,
    /// encrypts with AES-256-CBC, and authenticates with HMAC-SHA256.
    ///
    /// # Performance
    ///
    /// - Hardware-accelerated AES via OpenSSL (AES-NI on x86_64)
    /// - In-place buffer management (single allocation)
    /// - ~1 GB/s throughput on modern CPUs
    ///
    /// # Arguments
    ///
    /// * `plaintext` - The data to encrypt
    ///
    /// # Returns
    ///
    /// An `EncryptedAttachment` containing the ciphertext, key, digest,
    /// and original plaintext size. The key should be included in the
    /// E2E encrypted message.
    ///
    /// # Errors
    ///
    /// Returns `CryptoError::AttachmentEncryptionFailed` if encryption fails.
    pub fn encrypt(plaintext: &[u8]) -> Result<EncryptedAttachment, CryptoError> {
        // Generate 64-byte random key: [AES key (32)] + [MAC key (32)].
        // Wrapped in `Zeroizing` so any `?` return path wipes the local buffer
        // before any unwinding; the returned `EncryptedAttachment` carries its
        // own `ZeroizeOnDrop` via `EncryptedAttachment`'s derive.
        let mut key: Zeroizing<[u8; KEY_SET_LENGTH]> = Zeroizing::new([0u8; KEY_SET_LENGTH]);
        rand::rng().fill_bytes(&mut *key);

        // Split key into AES and MAC portions
        let aes_key = &key[..AES_KEY_LENGTH];
        let mac_key = &key[AES_KEY_LENGTH..];

        // Generate random IV (16 bytes)
        let mut iv = [0u8; IV_LENGTH];
        rand::rng().fill_bytes(&mut iv);

        // Pad plaintext using log-bucketing
        let padded_size = Self::padded_size(plaintext.len());

        // Create padded plaintext buffer
        let mut padded = vec![0u8; padded_size];
        padded[..plaintext.len()].copy_from_slice(plaintext);

        // Encrypt using OpenSSL's hardware-accelerated AES-256-CBC
        let encrypted = Self::encrypt_aes_cbc_openssl(aes_key, &iv, &padded)?;

        // Pre-allocate final ciphertext buffer: IV || encrypted || HMAC
        let total_size = IV_LENGTH + encrypted.len() + HMAC_LENGTH;
        let mut ciphertext = Vec::with_capacity(total_size);
        ciphertext.extend_from_slice(&iv);
        ciphertext.extend_from_slice(&encrypted);

        // Compute HMAC over IV + encrypted
        let hmac_result = Self::compute_hmac(mac_key, &ciphertext);

        // Append HMAC
        ciphertext.extend_from_slice(&hmac_result);

        // Compute final digest over entire ciphertext
        let final_digest = Self::compute_digest(&ciphertext);

        Ok(EncryptedAttachment {
            ciphertext,
            key: *key,
            digest: final_digest,
            plaintext_size: plaintext.len(),
        })
    }

    /// Hardware-accelerated AES-256-CBC encryption using OpenSSL.
    ///
    /// Uses AES-NI instructions on x86_64 for ~100x faster encryption.
    fn encrypt_aes_cbc_openssl(key: &[u8], iv: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let cipher = Cipher::aes_256_cbc();

        // Create encryptor with no padding (we handle padding ourselves)
        let mut crypter = Crypter::new(cipher, Mode::Encrypt, key, Some(iv))
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("OpenSSL init: {}", e)))?;
        crypter.pad(false); // No PKCS7 padding - we use zero padding

        // Allocate output buffer (same size as input for no-padding mode)
        let mut ciphertext = vec![0u8; plaintext.len() + cipher.block_size()];

        let mut count = crypter
            .update(plaintext, &mut ciphertext)
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("OpenSSL encrypt: {}", e)))?;

        count += crypter
            .finalize(&mut ciphertext[count..])
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("OpenSSL finalize: {}", e)))?;

        ciphertext.truncate(count);
        Ok(ciphertext)
    }

    /// Hardware-accelerated AES-256-CBC decryption using OpenSSL.
    ///
    /// Uses AES-NI instructions on x86_64 for ~100x faster decryption.
    fn decrypt_aes_cbc_openssl(key: &[u8], iv: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let cipher = Cipher::aes_256_cbc();

        // Create decryptor with no padding (we handle padding ourselves)
        let mut crypter = Crypter::new(cipher, Mode::Decrypt, key, Some(iv))
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("OpenSSL init: {}", e)))?;
        crypter.pad(false); // No PKCS7 padding - we use zero padding

        // Allocate output buffer
        let mut plaintext = vec![0u8; ciphertext.len() + cipher.block_size()];

        let mut count = crypter
            .update(ciphertext, &mut plaintext)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("OpenSSL decrypt: {}", e)))?;

        count += crypter
            .finalize(&mut plaintext[count..])
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("OpenSSL finalize: {}", e)))?;

        plaintext.truncate(count);
        Ok(plaintext)
    }

    /// Compute HMAC-SHA256 over data using OpenSSL (hardware accelerated).
    #[inline]
    pub(crate) fn compute_hmac(mac_key: &[u8], data: &[u8]) -> [u8; 32] {
        let pkey = PKey::hmac(mac_key).expect("HMAC key creation failed");
        let mut signer = Signer::new(MessageDigest::sha256(), &pkey)
            .expect("HMAC signer creation failed");
        signer.update(data).expect("HMAC update failed");
        let result = signer.sign_to_vec().expect("HMAC finalize failed");
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&result);
        arr
    }

    /// Decrypt ciphertext with provided key and verify integrity.
    ///
    /// Verifies the digest first, then the HMAC, then decrypts.
    /// This is the verify-then-decrypt pattern required for security.
    ///
    /// # Arguments
    ///
    /// * `ciphertext` - The encrypted data: IV (16) || encrypted || HMAC (32)
    /// * `key` - The 64-byte combined key: [AES key (32)] + [MAC key (32)]
    /// * `expected_digest` - The expected SHA-256 digest of the ciphertext
    /// * `plaintext_size` - The original plaintext size (for removing padding)
    ///
    /// # Returns
    ///
    /// The decrypted plaintext.
    ///
    /// # Errors
    ///
    /// - `CryptoError::AttachmentDigestMismatch` if digest verification fails
    /// - `CryptoError::AttachmentMacMismatch` if HMAC verification fails
    /// - `CryptoError::AttachmentDecryptionFailed` if decryption fails
    pub fn decrypt(
        ciphertext: &[u8],
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

        // Verify digest first (constant-time comparison)
        let actual_digest = Self::compute_digest(ciphertext);
        if actual_digest.ct_eq(expected_digest).unwrap_u8() != 1 {
            return Err(CryptoError::AttachmentDigestMismatch);
        }

        // Split key into AES and MAC portions
        let aes_key = &key[..AES_KEY_LENGTH];
        let mac_key = &key[AES_KEY_LENGTH..];

        // Split ciphertext into IV, encrypted data, and HMAC
        let iv = &ciphertext[..IV_LENGTH];
        let encrypted = &ciphertext[IV_LENGTH..ciphertext.len() - HMAC_LENGTH];
        let expected_hmac = &ciphertext[ciphertext.len() - HMAC_LENGTH..];

        // Verify HMAC (constant-time comparison) using OpenSSL (hardware accelerated)
        let pkey = PKey::hmac(mac_key)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("HMAC key: {}", e)))?;
        let mut signer = Signer::new(MessageDigest::sha256(), &pkey)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("HMAC signer: {}", e)))?;
        signer.update(iv)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("HMAC update: {}", e)))?;
        signer.update(encrypted)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("HMAC update: {}", e)))?;
        let computed_hmac = signer.sign_to_vec()
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("HMAC finalize: {}", e)))?;

        if computed_hmac.ct_eq(expected_hmac).unwrap_u8() != 1 {
            return Err(CryptoError::AttachmentMacMismatch);
        }

        // Decrypt with hardware-accelerated AES-256-CBC via OpenSSL
        let mut decrypted = Self::decrypt_aes_cbc_openssl(aes_key, iv, encrypted)?;

        // Truncate to original plaintext size (remove zero padding)
        if plaintext_size > decrypted.len() {
            return Err(CryptoError::AttachmentDecryptionFailed(
                "plaintext_size exceeds decrypted data".to_string(),
            ));
        }
        decrypted.truncate(plaintext_size);

        Ok(decrypted)
    }

    /// Decrypt ciphertext and return a secure buffer that auto-zeroizes.
    ///
    /// This is the recommended decryption method for security-sensitive applications.
    /// The returned `SecureBuffer` automatically zeroizes its contents when dropped,
    /// preventing decrypted plaintext from persisting in memory.
    ///
    /// # Arguments
    ///
    /// * `ciphertext` - The encrypted data: IV (16) || encrypted || HMAC (32)
    /// * `key` - The 64-byte combined key: [AES key (32)] + [MAC key (32)]
    /// * `expected_digest` - The expected SHA-256 digest of the ciphertext
    /// * `plaintext_size` - The original plaintext size (for removing padding)
    ///
    /// # Returns
    ///
    /// A `SecureBuffer` containing the decrypted plaintext. The buffer will
    /// automatically zeroize its contents when dropped.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let secure_data = AttachmentCrypto::decrypt_secure(&ciphertext, &key, &digest, size)?;
    /// // Use the data
    /// player.load(secure_data.as_slice())?;
    /// // secure_data dropped here → memory automatically zeroized
    /// ```
    pub fn decrypt_secure(
        ciphertext: &[u8],
        key: &[u8; KEY_SET_LENGTH],
        expected_digest: &[u8; 32],
        plaintext_size: usize,
    ) -> Result<SecureBuffer, CryptoError> {
        let decrypted = Self::decrypt(ciphertext, key, expected_digest, plaintext_size)?;
        Ok(SecureBuffer::new(decrypted))
    }

    /// Compute SHA-256 digest of ciphertext using OpenSSL (hardware accelerated).
    ///
    /// This digest is included in the AttachmentPointer and verified
    /// before decryption to detect corruption or tampering.
    #[must_use]
    pub fn compute_digest(ciphertext: &[u8]) -> [u8; 32] {
        let mut hasher = Hasher::new(MessageDigest::sha256())
            .expect("SHA-256 hasher creation failed");
        hasher.update(ciphertext).expect("SHA-256 update failed");
        let result = hasher.finish().expect("SHA-256 finalize failed");
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&result);
        arr
    }

    /// Compute padded size using Signal's log-bucketing algorithm.
    ///
    /// This prevents file size leakage while minimizing overhead.
    /// Formula: `max(541, ceil(1.05^ceil(log(size)/log(1.05))))`
    ///
    /// The result is always a multiple of 16 (AES block size).
    #[must_use]
    pub fn padded_size(original_size: usize) -> usize {
        if original_size == 0 {
            // Minimum size that's a multiple of 16 and >= 541
            return Self::round_to_block_size(MIN_PADDED_SIZE);
        }

        let bucket = (original_size as f64).ln() / BUCKET_BASE.ln();
        let rounded = bucket.ceil();
        let padded = BUCKET_BASE.powf(rounded) as usize;
        let result = padded.max(MIN_PADDED_SIZE);

        // Ensure result is a multiple of AES block size (16)
        Self::round_to_block_size(result)
    }

    /// Encrypt a file to another file in a streaming fashion.
    ///
    /// Reads the input in 256KB chunks, encrypts with AES-256-CBC, and writes
    /// the ciphertext (IV + encrypted + HMAC) to the output file. Memory usage
    /// is O(CHUNK_SIZE) regardless of file size.
    ///
    /// The output format is identical to [`Self::encrypt`]: Signal-compatible
    /// `IV (16) || AES-CBC(padded plaintext) || HMAC-SHA256 (32)`.
    ///
    /// This function is synchronous and should be called from `spawn_blocking`.
    pub fn encrypt_file_to_file(
        input_path: &std::path::Path,
        output_path: &std::path::Path,
    ) -> Result<StreamingEncryptResult, CryptoError> {
        use std::io::{Read, Write, BufWriter};

        const CHUNK_SIZE: usize = 256 * 1024;

        // Get file size for padding calculation and size limit check
        let input_metadata = std::fs::metadata(input_path)
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("read metadata: {}", e)))?;
        let plaintext_size = input_metadata.len() as usize;
        let padded_size = Self::padded_size(plaintext_size);

        // Check max attachment size
        let max_size = max_attachment_size();
        if (plaintext_size as u64) > max_size {
            return Err(CryptoError::AttachmentTooLarge {
                size: plaintext_size as u64,
                max: max_size,
            });
        }

        // Generate random key and IV.
        // Wrap `key` in `Zeroizing` so any early `?` return zeroizes the 64-byte
        // key material before unwinding. On the success path the key is copied
        // into `StreamingEncryptResult` (which itself is `ZeroizeOnDrop`), and
        // the local `Zeroizing` wrapper is dropped (zeroizing its copy) at
        // function exit.
        let mut key: Zeroizing<[u8; KEY_SET_LENGTH]> = Zeroizing::new([0u8; KEY_SET_LENGTH]);
        rand::RngCore::fill_bytes(&mut rand::rng(), key.as_mut_slice());
        let aes_key = &key[..AES_KEY_LENGTH];
        let mac_key = &key[AES_KEY_LENGTH..];

        let mut iv = [0u8; IV_LENGTH];
        rand::RngCore::fill_bytes(&mut rand::rng(), &mut iv);

        // Open files
        let mut input = std::io::BufReader::new(
            std::fs::File::open(input_path)
                .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("open input: {}", e)))?,
        );
        let mut output = BufWriter::new(
            std::fs::File::create(output_path)
                .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("create output: {}", e)))?,
        );

        // Init crypto processors
        let cipher = Cipher::aes_256_cbc();
        let mut crypter = Crypter::new(cipher, Mode::Encrypt, aes_key, Some(&iv))
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("init crypter: {}", e)))?;
        crypter.pad(false); // We handle padding ourselves

        let pkey = PKey::hmac(mac_key)
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("init hmac key: {}", e)))?;
        let mut hmac_signer = Signer::new(MessageDigest::sha256(), &pkey)
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("init hmac: {}", e)))?;

        let mut digest_hasher = Hasher::new(MessageDigest::sha256())
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("init digest: {}", e)))?;

        // Incremental MAC state: compute per-chunk HMAC-SHA256 for files > threshold
        let use_incremental_mac = (plaintext_size as u64) > INCREMENTAL_MAC_THRESHOLD;
        let mut incremental_mac_digests: Vec<u8> = Vec::new();
        let mut inc_mac_accumulator: Vec<u8> = Vec::new();

        // Write IV
        output.write_all(&iv)
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("write iv: {}", e)))?;
        hmac_signer.update(&iv)
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("hmac iv: {}", e)))?;
        digest_hasher.update(&iv)
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("digest iv: {}", e)))?;

        // IV is part of the ciphertext, so include it in incremental MAC accumulation
        if use_incremental_mac {
            inc_mac_accumulator.extend_from_slice(&iv);
        }

        // Encrypt buffer: chunk + one AES block headroom
        let mut encrypt_buf = vec![0u8; CHUNK_SIZE + cipher.block_size()];
        let mut read_buf = vec![0u8; CHUNK_SIZE];
        let mut bytes_fed: usize = 0; // total plaintext bytes fed to crypter (including padding)

        // Read and encrypt file content in chunks
        loop {
            let n = input.read(&mut read_buf)
                .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("read: {}", e)))?;
            if n == 0 {
                break;
            }

            let count = crypter.update(&read_buf[..n], &mut encrypt_buf)
                .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("encrypt: {}", e)))?;

            if count > 0 {
                output.write_all(&encrypt_buf[..count])
                    .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("write: {}", e)))?;
                hmac_signer.update(&encrypt_buf[..count])
                    .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("hmac: {}", e)))?;
                digest_hasher.update(&encrypt_buf[..count])
                    .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("digest: {}", e)))?;

                // Accumulate ciphertext for incremental MAC
                if use_incremental_mac {
                    Self::accumulate_incremental_mac(
                        mac_key, &encrypt_buf[..count],
                        &mut inc_mac_accumulator, &mut incremental_mac_digests,
                    );
                }
            }

            bytes_fed += n;
        }

        // Encrypt zero-padding if needed (padded_size > bytes read from file).
        // Write in chunks to avoid exceeding the encrypt_buf capacity.
        if bytes_fed < padded_size {
            let mut padding_remaining = padded_size - bytes_fed;
            let zero_chunk = vec![0u8; CHUNK_SIZE.min(padding_remaining)];
            while padding_remaining > 0 {
                let chunk_len = CHUNK_SIZE.min(padding_remaining);
                let count = crypter.update(&zero_chunk[..chunk_len], &mut encrypt_buf)
                    .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("encrypt pad: {}", e)))?;
                if count > 0 {
                    output.write_all(&encrypt_buf[..count])
                        .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("write pad: {}", e)))?;
                    hmac_signer.update(&encrypt_buf[..count])
                        .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("hmac pad: {}", e)))?;
                    digest_hasher.update(&encrypt_buf[..count])
                        .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("digest pad: {}", e)))?;

                    if use_incremental_mac {
                        Self::accumulate_incremental_mac(
                            mac_key, &encrypt_buf[..count],
                            &mut inc_mac_accumulator, &mut incremental_mac_digests,
                        );
                    }
                }
                padding_remaining -= chunk_len;
            }
        }

        // Finalize AES
        let final_count = crypter.finalize(&mut encrypt_buf)
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("finalize: {}", e)))?;
        if final_count > 0 {
            output.write_all(&encrypt_buf[..final_count])
                .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("write final: {}", e)))?;
            hmac_signer.update(&encrypt_buf[..final_count])
                .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("hmac final: {}", e)))?;
            digest_hasher.update(&encrypt_buf[..final_count])
                .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("digest final: {}", e)))?;

            if use_incremental_mac {
                Self::accumulate_incremental_mac(
                    mac_key, &encrypt_buf[..final_count],
                    &mut inc_mac_accumulator, &mut incremental_mac_digests,
                );
            }
        }

        // Flush any remaining partial chunk for incremental MAC
        if use_incremental_mac && !inc_mac_accumulator.is_empty() {
            let hmac = Self::compute_hmac(mac_key, &inc_mac_accumulator);
            incremental_mac_digests.extend_from_slice(&hmac);
            inc_mac_accumulator.clear();
        }

        // Finalize HMAC and write it
        let hmac_result = hmac_signer.sign_to_vec()
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("hmac finalize: {}", e)))?;
        output.write_all(&hmac_result)
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("write hmac: {}", e)))?;

        // Feed HMAC bytes into digest (digest covers IV + encrypted + HMAC)
        digest_hasher.update(&hmac_result)
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("digest hmac: {}", e)))?;

        // Finalize digest
        let digest_result = digest_hasher.finish()
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("digest finalize: {}", e)))?;
        let mut digest = [0u8; 32];
        digest.copy_from_slice(&digest_result);

        // Flush output
        output.flush()
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("flush: {}", e)))?;

        // Compute ciphertext size: IV + padded_encrypted + HMAC
        let ciphertext_size = std::fs::metadata(output_path)
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("output metadata: {}", e)))?
            .len();

        Ok(StreamingEncryptResult {
            key: *key,
            digest,
            ciphertext_size,
            plaintext_size: plaintext_size as u64,
            incremental_mac: incremental_mac_digests,
            incremental_chunk_size: if use_incremental_mac { INCREMENTAL_MAC_CHUNK_SIZE as u32 } else { 0 },
        })
    }

    /// Accumulate ciphertext bytes and compute incremental MAC for complete 64KB chunks.
    ///
    /// Appends data to the accumulator. When the accumulator reaches INCREMENTAL_MAC_CHUNK_SIZE,
    /// computes HMAC-SHA256 of the chunk and appends the 32-byte digest to `digests`.
    fn accumulate_incremental_mac(
        mac_key: &[u8],
        data: &[u8],
        accumulator: &mut Vec<u8>,
        digests: &mut Vec<u8>,
    ) {
        accumulator.extend_from_slice(data);
        while accumulator.len() >= INCREMENTAL_MAC_CHUNK_SIZE {
            let hmac = Self::compute_hmac(mac_key, &accumulator[..INCREMENTAL_MAC_CHUNK_SIZE]);
            digests.extend_from_slice(&hmac);
            accumulator.drain(..INCREMENTAL_MAC_CHUNK_SIZE);
        }
    }

    /// Round up to the next multiple of 16 (AES block size).
    #[inline]
    fn round_to_block_size(size: usize) -> usize {
        (size + 15) & !15
    }
}

/// Maximum attachment size (1 GiB default).
///
/// Large files are streamed as multipart chunks by the upload path (see
/// `streaming_encrypt` in this module + the mailbox service's multipart
/// putter) so the whole file never has to fit in RAM. Cap raised from
/// 100 MiB after users hit the limit on video files.
///
/// Can be overridden via `CHATTER_MAX_ATTACHMENT_SIZE` environment variable.
pub fn max_attachment_size() -> u64 {
    std::env::var("CHATTER_MAX_ATTACHMENT_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024 * 1024 * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let plaintext = b"Hello, Signal-compatible encryption!";

        // Encrypt
        let encrypted = AttachmentCrypto::encrypt(plaintext)
            .expect("encryption should succeed");

        // Verify ciphertext structure: IV (16) + encrypted + HMAC (32)
        assert!(encrypted.ciphertext.len() >= MIN_CIPHERTEXT_LENGTH);
        assert_eq!(encrypted.plaintext_size, plaintext.len());

        // Decrypt
        let decrypted = AttachmentCrypto::decrypt(
            &encrypted.ciphertext,
            &encrypted.key,
            &encrypted.digest,
            encrypted.plaintext_size,
        ).expect("decryption should succeed");

        assert_eq!(decrypted.as_slice(), plaintext);
    }

    #[test]
    fn test_encrypt_decrypt_empty() {
        let plaintext = b"";

        let encrypted = AttachmentCrypto::encrypt(plaintext)
            .expect("encryption should succeed");

        let decrypted = AttachmentCrypto::decrypt(
            &encrypted.ciphertext,
            &encrypted.key,
            &encrypted.digest,
            encrypted.plaintext_size,
        ).expect("decryption should succeed");

        assert_eq!(decrypted.as_slice(), plaintext);
    }

    #[test]
    fn test_encrypt_decrypt_large() {
        // Test with 1MB of data
        let plaintext: Vec<u8> = (0..1024 * 1024).map(|i| (i % 256) as u8).collect();

        let encrypted = AttachmentCrypto::encrypt(&plaintext)
            .expect("encryption should succeed");

        let decrypted = AttachmentCrypto::decrypt(
            &encrypted.ciphertext,
            &encrypted.key,
            &encrypted.digest,
            encrypted.plaintext_size,
        ).expect("decryption should succeed");

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_digest_mismatch() {
        let plaintext = b"test data";
        let encrypted = AttachmentCrypto::encrypt(plaintext)
            .expect("encryption should succeed");

        // Tamper with digest
        let mut bad_digest = encrypted.digest;
        bad_digest[0] ^= 0xFF;

        let result = AttachmentCrypto::decrypt(
            &encrypted.ciphertext,
            &encrypted.key,
            &bad_digest,
            encrypted.plaintext_size,
        );

        assert!(matches!(result, Err(CryptoError::AttachmentDigestMismatch)));
    }

    #[test]
    fn test_mac_mismatch() {
        let plaintext = b"test data";
        let encrypted = AttachmentCrypto::encrypt(plaintext)
            .expect("encryption should succeed");

        // Tamper with ciphertext (the encrypted portion, not the HMAC)
        // This will cause HMAC verification to fail
        let mut bad_ciphertext = encrypted.ciphertext.clone();
        bad_ciphertext[IV_LENGTH + 5] ^= 0xFF;

        // Recompute digest for the tampered ciphertext
        let bad_digest = AttachmentCrypto::compute_digest(&bad_ciphertext);

        let result = AttachmentCrypto::decrypt(
            &bad_ciphertext,
            &encrypted.key,
            &bad_digest,
            encrypted.plaintext_size,
        );

        assert!(matches!(result, Err(CryptoError::AttachmentMacMismatch)));
    }

    #[test]
    fn test_ciphertext_too_short() {
        let key = [0u8; KEY_SET_LENGTH];
        let digest = [0u8; 32];
        let short_ciphertext = vec![0u8; 10];

        let result = AttachmentCrypto::decrypt(&short_ciphertext, &key, &digest, 0);

        assert!(matches!(result, Err(CryptoError::AttachmentDecryptionFailed(_))));
    }

    #[test]
    fn test_padded_size_zero() {
        let padded = AttachmentCrypto::padded_size(0);
        assert!(padded >= MIN_PADDED_SIZE);
        assert_eq!(padded % 16, 0, "padded size should be multiple of 16");
    }

    #[test]
    fn test_padded_size_small() {
        let padded = AttachmentCrypto::padded_size(100);
        assert!(padded >= MIN_PADDED_SIZE);
        assert_eq!(padded % 16, 0, "padded size should be multiple of 16");
    }

    #[test]
    fn test_padded_size_large() {
        let padded = AttachmentCrypto::padded_size(10_000);
        assert!(padded >= 10_000);
        assert_eq!(padded % 16, 0, "padded size should be multiple of 16");
    }

    #[test]
    fn test_padded_size_always_larger() {
        for size in [1, 100, 1000, 10_000, 100_000, 1_000_000] {
            let padded = AttachmentCrypto::padded_size(size);
            assert!(padded >= size, "padded size should be >= original for size {}", size);
            assert_eq!(padded % 16, 0, "padded size should be multiple of 16 for size {}", size);
        }
    }

    #[test]
    fn test_different_plaintexts_different_ciphertexts() {
        let encrypted1 = AttachmentCrypto::encrypt(b"hello")
            .expect("encryption should succeed");
        let encrypted2 = AttachmentCrypto::encrypt(b"hello")
            .expect("encryption should succeed");

        // Same plaintext should produce different ciphertext (random IV and key)
        assert_ne!(encrypted1.ciphertext, encrypted2.ciphertext);
        assert_ne!(encrypted1.key, encrypted2.key);
        assert_ne!(encrypted1.digest, encrypted2.digest);
    }

    #[test]
    fn test_key_length() {
        let encrypted = AttachmentCrypto::encrypt(b"test")
            .expect("encryption should succeed");

        assert_eq!(encrypted.key.len(), KEY_SET_LENGTH);
        assert_eq!(KEY_SET_LENGTH, 64);
    }

    #[test]
    fn test_ciphertext_format() {
        let plaintext = b"test message";
        let encrypted = AttachmentCrypto::encrypt(plaintext)
            .expect("encryption should succeed");

        // Format: IV (16) + encrypted (padded, multiple of 16) + HMAC (32)
        let expected_padded = AttachmentCrypto::padded_size(plaintext.len());
        let expected_length = IV_LENGTH + expected_padded + HMAC_LENGTH;

        assert_eq!(encrypted.ciphertext.len(), expected_length);
    }

    // =========================================================================
    // Streaming encryption tests
    // =========================================================================

    #[test]
    fn test_streaming_encrypt_decrypt_roundtrip() {
        let plaintext = b"Hello, streaming Signal-compatible encryption!";

        // Write plaintext to a temp file
        let input_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(input_file.path(), plaintext).unwrap();

        let output_file = tempfile::NamedTempFile::new().unwrap();

        // Encrypt via streaming
        let result = AttachmentCrypto::encrypt_file_to_file(
            input_file.path(),
            output_file.path(),
        ).expect("streaming encryption should succeed");

        assert_eq!(result.plaintext_size, plaintext.len() as u64);

        // Read ciphertext and decrypt with in-memory decrypt
        let ciphertext = std::fs::read(output_file.path()).unwrap();
        assert_eq!(ciphertext.len() as u64, result.ciphertext_size);

        let decrypted = AttachmentCrypto::decrypt(
            &ciphertext,
            &result.key,
            &result.digest,
            plaintext.len(),
        ).expect("decryption should succeed");

        assert_eq!(decrypted.as_slice(), plaintext);
    }

    #[test]
    fn test_streaming_encrypt_large_file() {
        // 1MB of patterned data
        let plaintext: Vec<u8> = (0..1024 * 1024).map(|i| (i % 256) as u8).collect();

        let input_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(input_file.path(), &plaintext).unwrap();

        let output_file = tempfile::NamedTempFile::new().unwrap();

        let result = AttachmentCrypto::encrypt_file_to_file(
            input_file.path(),
            output_file.path(),
        ).expect("streaming encryption should succeed");

        let ciphertext = std::fs::read(output_file.path()).unwrap();

        let decrypted = AttachmentCrypto::decrypt(
            &ciphertext,
            &result.key,
            &result.digest,
            plaintext.len(),
        ).expect("decryption should succeed");

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_streaming_encrypt_format_matches_in_memory() {
        // Verify the output format is the same as in-memory encrypt:
        // IV (16) + padded_encrypted + HMAC (32)
        let plaintext = b"format compatibility test";

        let input_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(input_file.path(), plaintext).unwrap();

        let output_file = tempfile::NamedTempFile::new().unwrap();

        let result = AttachmentCrypto::encrypt_file_to_file(
            input_file.path(),
            output_file.path(),
        ).expect("streaming encryption should succeed");

        let ciphertext = std::fs::read(output_file.path()).unwrap();

        // Verify format: IV(16) + encrypted(padded_size) + HMAC(32)
        let expected_padded = AttachmentCrypto::padded_size(plaintext.len());
        let expected_length = IV_LENGTH + expected_padded + HMAC_LENGTH;
        assert_eq!(ciphertext.len(), expected_length);

        // Verify digest matches compute_digest on the output
        let recomputed_digest = AttachmentCrypto::compute_digest(&ciphertext);
        assert_eq!(result.digest, recomputed_digest);
    }

    #[test]
    fn test_streaming_encrypt_empty_file() {
        let input_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(input_file.path(), b"").unwrap();

        let output_file = tempfile::NamedTempFile::new().unwrap();

        let result = AttachmentCrypto::encrypt_file_to_file(
            input_file.path(),
            output_file.path(),
        ).expect("streaming encryption of empty file should succeed");

        assert_eq!(result.plaintext_size, 0);

        let ciphertext = std::fs::read(output_file.path()).unwrap();
        let decrypted = AttachmentCrypto::decrypt(
            &ciphertext,
            &result.key,
            &result.digest,
            0,
        ).expect("decryption of empty should succeed");

        assert!(decrypted.is_empty());
    }

    #[test]
    fn test_streaming_encrypt_key_length() {
        let input_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(input_file.path(), b"key length test").unwrap();

        let output_file = tempfile::NamedTempFile::new().unwrap();

        let result = AttachmentCrypto::encrypt_file_to_file(
            input_file.path(),
            output_file.path(),
        ).expect("streaming encryption should succeed");

        assert_eq!(result.key.len(), KEY_SET_LENGTH);
    }

    // =========================================================================
    // Incremental MAC tests
    // =========================================================================

    #[test]
    fn test_streaming_encrypt_small_file_no_incremental_mac() {
        // Files <= 50KB should NOT have incremental MAC
        let plaintext = vec![42u8; 1024]; // 1KB

        let input_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(input_file.path(), &plaintext).unwrap();

        let output_file = tempfile::NamedTempFile::new().unwrap();

        let result = AttachmentCrypto::encrypt_file_to_file(
            input_file.path(),
            output_file.path(),
        ).unwrap();

        assert!(result.incremental_mac.is_empty());
        assert_eq!(result.incremental_chunk_size, 0);
    }

    #[test]
    fn test_streaming_encrypt_large_file_has_incremental_mac() {
        // Files > 50KB should have incremental MAC
        let plaintext: Vec<u8> = (0..100_000).map(|i| (i % 256) as u8).collect(); // 100KB

        let input_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(input_file.path(), &plaintext).unwrap();

        let output_file = tempfile::NamedTempFile::new().unwrap();

        let result = AttachmentCrypto::encrypt_file_to_file(
            input_file.path(),
            output_file.path(),
        ).unwrap();

        assert!(!result.incremental_mac.is_empty());
        assert_eq!(result.incremental_chunk_size, INCREMENTAL_MAC_CHUNK_SIZE as u32);
        // incremental_mac is a concatenation of 32-byte digests
        assert_eq!(result.incremental_mac.len() % 32, 0);

        // Number of chunks should match ciphertext size divided by chunk size
        let ciphertext = std::fs::read(output_file.path()).unwrap();
        let ciphertext_without_hmac = ciphertext.len() - 32; // exclude trailing HMAC
        let expected_chunks = (ciphertext_without_hmac + INCREMENTAL_MAC_CHUNK_SIZE - 1) / INCREMENTAL_MAC_CHUNK_SIZE;
        assert_eq!(result.incremental_mac.len() / 32, expected_chunks);
    }

    #[test]
    fn test_streaming_encrypt_incremental_mac_deterministic() {
        // Same plaintext, same key → different ciphertext (random IV/key)
        // But the relationship between ciphertext and incremental MAC should be consistent
        let plaintext: Vec<u8> = (0..100_000).map(|i| (i % 256) as u8).collect();

        let input_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(input_file.path(), &plaintext).unwrap();

        let output_file = tempfile::NamedTempFile::new().unwrap();

        let result = AttachmentCrypto::encrypt_file_to_file(
            input_file.path(),
            output_file.path(),
        ).unwrap();

        // Verify incremental MAC is consistent with the ciphertext by recomputing
        let ciphertext = std::fs::read(output_file.path()).unwrap();
        let mac_key = &result.key[32..];
        let ciphertext_without_hmac = &ciphertext[..ciphertext.len() - 32];

        // Recompute incremental MAC from the ciphertext
        let mut recomputed_mac = Vec::new();
        for chunk in ciphertext_without_hmac.chunks(INCREMENTAL_MAC_CHUNK_SIZE) {
            let hmac = AttachmentCrypto::compute_hmac(mac_key, chunk);
            recomputed_mac.extend_from_slice(&hmac);
        }

        assert_eq!(result.incremental_mac, recomputed_mac);
    }

    #[test]
    fn test_max_attachment_size_enforcement() {
        // This test uses env var to set a very small max size
        // SAFETY: Single-threaded test (--test-threads=1), no concurrent env reads
        unsafe { std::env::set_var("CHATTER_MAX_ATTACHMENT_SIZE", "100") };

        let plaintext = vec![42u8; 200]; // 200 bytes, exceeds 100 byte limit

        let input_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(input_file.path(), &plaintext).unwrap();

        let output_file = tempfile::NamedTempFile::new().unwrap();

        let result = AttachmentCrypto::encrypt_file_to_file(
            input_file.path(),
            output_file.path(),
        );

        // SAFETY: Single-threaded test, cleaning up before any concurrent access
        unsafe { std::env::remove_var("CHATTER_MAX_ATTACHMENT_SIZE") };

        assert!(matches!(result, Err(CryptoError::AttachmentTooLarge { .. })));
    }
}
