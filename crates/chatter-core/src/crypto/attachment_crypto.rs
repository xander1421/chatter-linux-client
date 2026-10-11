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

use aes::cipher::block_padding::NoPadding;
use aes::cipher::{BlockModeDecrypt, BlockModeEncrypt, KeyIvInit};
use sha2::{Digest, Sha256};

use crate::crypto::hmac_compat::Sha256Stream;

type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;
const AES_BLOCK_SIZE: usize = 16;
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
}

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
        {
            use rand::TryRngCore;
            rand::rngs::OsRng
                .try_fill_bytes(&mut *key)
                .expect("OS CSPRNG must not fail");
        }

        // Split key into AES and MAC portions
        let aes_key = &key[..AES_KEY_LENGTH];
        let mac_key = &key[AES_KEY_LENGTH..];

        // Generate random IV (16 bytes)
        let mut iv = [0u8; IV_LENGTH];
        {
            use rand::TryRngCore;
            rand::rngs::OsRng
                .try_fill_bytes(&mut iv)
                .expect("OS CSPRNG must not fail");
        }

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
    fn encrypt_aes_cbc_openssl(
        key: &[u8],
        iv: &[u8],
        plaintext: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        // Raw AES-256-CBC, no block padding (caller zero-pads to the block size).
        // Input is block-aligned → encrypt in place.
        let mut buf = plaintext.to_vec();
        let len = buf.len();
        Aes256CbcEnc::new_from_slices(key, iv)
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("AES init: {e}")))?
            .encrypt_padded::<NoPadding>(&mut buf, len)
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("AES encrypt: {e}")))?;
        Ok(buf)
    }

    /// Hardware-accelerated AES-256-CBC decryption using OpenSSL.
    ///
    /// Uses AES-NI instructions on x86_64 for ~100x faster decryption.
    fn decrypt_aes_cbc_openssl(
        key: &[u8],
        iv: &[u8],
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        // Cipher 0.5 decrypts directly from authenticated ciphertext into the output.
        Aes256CbcDec::new_from_slices(key, iv)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("AES init: {e}")))?
            .decrypt_padded_vec::<NoPadding>(ciphertext)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("AES decrypt: {e}")))
    }

    /// Compute HMAC-SHA256 over data using OpenSSL (hardware accelerated).
    #[inline]
    pub(crate) fn compute_hmac(mac_key: &[u8], data: &[u8]) -> [u8; 32] {
        let mut signer = crate::crypto::hmac_compat::hmac_init(mac_key).expect("HMAC init failed");
        signer.update(data).expect("HMAC update failed");
        let result = signer.finalize().expect("HMAC finalize failed");
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
            let err = CryptoError::AttachmentDecryptionFailed("ciphertext too short".to_string());
            tracing::warn!(
                target: "chatter::crypto::decrypt_failure",
                kind = "attachment",
                error = %err,
                "decryption failure"
            );
            return Err(err);
        }

        // Verify digest first (constant-time comparison)
        let actual_digest = Self::compute_digest(ciphertext);
        if actual_digest.ct_eq(expected_digest).unwrap_u8() != 1 {
            let err = CryptoError::AttachmentDigestMismatch;
            tracing::warn!(
                target: "chatter::crypto::decrypt_failure",
                kind = "attachment",
                error = %err,
                "decryption failure"
            );
            return Err(err);
        }

        // Split key into AES and MAC portions
        let aes_key = &key[..AES_KEY_LENGTH];
        let mac_key = &key[AES_KEY_LENGTH..];

        // Split ciphertext into IV, encrypted data, and HMAC
        let iv = &ciphertext[..IV_LENGTH];
        let encrypted = &ciphertext[IV_LENGTH..ciphertext.len() - HMAC_LENGTH];
        let expected_hmac = &ciphertext[ciphertext.len() - HMAC_LENGTH..];

        // Verify HMAC (constant-time comparison) using BoringSSL (hardware accelerated)
        let mut signer = crate::crypto::hmac_compat::hmac_init(mac_key)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("HMAC init: {}", e)))?;
        signer
            .update(iv)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("HMAC update: {}", e)))?;
        signer
            .update(encrypted)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("HMAC update: {}", e)))?;
        let computed_hmac = signer.finalize().map_err(|e| {
            CryptoError::AttachmentDecryptionFailed(format!("HMAC finalize: {}", e))
        })?;

        if computed_hmac.ct_eq(expected_hmac).unwrap_u8() != 1 {
            let err = CryptoError::AttachmentMacMismatch;
            tracing::warn!(
                target: "chatter::crypto::decrypt_failure",
                kind = "attachment",
                error = %err,
                "decryption failure"
            );
            return Err(err);
        }

        // Decrypt with hardware-accelerated AES-256-CBC via OpenSSL
        let mut decrypted = Self::decrypt_aes_cbc_openssl(aes_key, iv, encrypted).map_err(|e| {
            tracing::warn!(
                target: "chatter::crypto::decrypt_failure",
                kind = "attachment",
                error = %e,
                "decryption failure"
            );
            e
        })?;

        // Truncate to original plaintext size (remove zero padding)
        if plaintext_size > decrypted.len() {
            let err = CryptoError::AttachmentDecryptionFailed(
                "plaintext_size exceeds decrypted data".to_string(),
            );
            tracing::warn!(
                target: "chatter::crypto::decrypt_failure",
                kind = "attachment",
                error = %err,
                "decryption failure"
            );
            return Err(err);
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
        let mut hasher = Sha256::new();
        hasher.update(ciphertext);
        hasher.finalize().into()
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
        use std::io::{BufWriter, Read, Write};

        const CHUNK_SIZE: usize = 256 * 1024;

        // Get file size for padding calculation and size limit check
        let input_metadata = std::fs::metadata(input_path).map_err(|e| {
            CryptoError::AttachmentEncryptionFailed(format!("read metadata: {}", e))
        })?;
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
        {
            use rand::TryRngCore;
            rand::rngs::OsRng
                .try_fill_bytes(key.as_mut_slice())
                .expect("OS CSPRNG must not fail");
        }
        let aes_key = &key[..AES_KEY_LENGTH];
        let mac_key = &key[AES_KEY_LENGTH..];

        let mut iv = [0u8; IV_LENGTH];
        {
            use rand::TryRngCore;
            rand::rngs::OsRng
                .try_fill_bytes(&mut iv)
                .expect("OS CSPRNG must not fail");
        }

        // Open files
        let mut input =
            std::io::BufReader::new(std::fs::File::open(input_path).map_err(|e| {
                CryptoError::AttachmentEncryptionFailed(format!("open input: {}", e))
            })?);
        let mut output = BufWriter::new(std::fs::File::create(output_path).map_err(|e| {
            CryptoError::AttachmentEncryptionFailed(format!("create output: {}", e))
        })?);

        // Init crypto processors
        let mut encryptor = Aes256CbcEnc::new_from_slices(aes_key, &iv)
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("init cipher: {e}")))?;
        // Partial-block remainder; raw CBC encrypts whole 16-byte blocks only.
        let mut carry: Vec<u8> = Vec::new();

        let mut hmac_signer = crate::crypto::hmac_compat::hmac_init(mac_key)
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("init hmac: {}", e)))?;

        let mut digest_hasher = Sha256Stream::new()
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("init digest: {}", e)))?;

        // Write IV
        output
            .write_all(&iv)
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("write iv: {}", e)))?;
        hmac_signer
            .update(&iv)
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("hmac iv: {}", e)))?;
        digest_hasher
            .update(&iv)
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("digest iv: {}", e)))?;

        let mut read_buf = vec![0u8; CHUNK_SIZE];
        let mut bytes_fed: usize = 0; // total plaintext bytes fed (including padding)

        // Encrypt the whole-block prefix of (carry + new bytes), writing +
        // MAC-ing + digesting the ciphertext, and retain the partial-block tail.
        macro_rules! feed {
            ($data:expr) => {{
                carry.extend_from_slice($data);
                let full = carry.len() - (carry.len() % AES_BLOCK_SIZE);
                if full > 0 {
                    for blk in carry[..full].chunks_exact_mut(AES_BLOCK_SIZE) {
                        encryptor.encrypt_block(blk.try_into().expect("AES block length"));
                    }
                    output.write_all(&carry[..full]).map_err(|e| {
                        CryptoError::AttachmentEncryptionFailed(format!("write: {}", e))
                    })?;
                    hmac_signer.update(&carry[..full]).map_err(|e| {
                        CryptoError::AttachmentEncryptionFailed(format!("hmac: {}", e))
                    })?;
                    digest_hasher.update(&carry[..full]).map_err(|e| {
                        CryptoError::AttachmentEncryptionFailed(format!("digest: {}", e))
                    })?;
                    carry.drain(..full);
                }
            }};
        }

        // Read and encrypt file content in chunks
        loop {
            let n = input
                .read(&mut read_buf)
                .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("read: {}", e)))?;
            if n == 0 {
                break;
            }
            feed!(&read_buf[..n]);
            bytes_fed += n;
        }

        // Encrypt zero-padding if needed (padded_size > bytes read from file).
        // Write in chunks to avoid exceeding the encrypt_buf capacity.
        if bytes_fed < padded_size {
            let mut padding_remaining = padded_size - bytes_fed;
            let zero_chunk = vec![0u8; CHUNK_SIZE.min(padding_remaining)];
            while padding_remaining > 0 {
                let chunk_len = CHUNK_SIZE.min(padding_remaining);
                feed!(&zero_chunk[..chunk_len]);
                padding_remaining -= chunk_len;
            }
        }

        // Raw CBC, no padding: total input is block-aligned, nothing buffered.
        debug_assert!(carry.is_empty(), "CBC carry not block-aligned");

        // Finalize HMAC and write it
        let hmac_result = hmac_signer.finalize().map_err(|e| {
            CryptoError::AttachmentEncryptionFailed(format!("hmac finalize: {}", e))
        })?;
        output
            .write_all(&hmac_result)
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("write hmac: {}", e)))?;

        // Feed HMAC bytes into digest (digest covers IV + encrypted + HMAC)
        digest_hasher
            .update(&hmac_result)
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("digest hmac: {}", e)))?;

        // Finalize digest
        let digest_result = digest_hasher.finish().map_err(|e| {
            CryptoError::AttachmentEncryptionFailed(format!("digest finalize: {}", e))
        })?;
        let mut digest = [0u8; 32];
        digest.copy_from_slice(&digest_result);

        // Flush output
        output
            .flush()
            .map_err(|e| CryptoError::AttachmentEncryptionFailed(format!("flush: {}", e)))?;

        // Compute ciphertext size: IV + padded_encrypted + HMAC
        let ciphertext_size = std::fs::metadata(output_path)
            .map_err(|e| {
                CryptoError::AttachmentEncryptionFailed(format!("output metadata: {}", e))
            })?
            .len();

        Ok(StreamingEncryptResult {
            key: *key,
            digest,
            ciphertext_size,
            plaintext_size: plaintext_size as u64,
        })
    }

    /// Round up to the next multiple of 16 (AES block size).
    #[inline]
    fn round_to_block_size(size: usize) -> usize {
        (size + 15) & !15
    }
}

/// Maximum attachment size (2 GiB default).
///
/// Large files are streamed as multipart chunks by the upload path (see
/// `streaming_encrypt` in this module + the mailbox service's multipart
/// putter) so the whole file never has to fit in RAM. Cap raised from
/// 100 MiB after users hit the limit on video files, then to 2 GiB once
/// the client gained a true multipart CDN upload path.
const MAX_ATTACHMENT_SIZE: u64 = 2 * 1024 * 1024 * 1024;

#[cfg(test)]
thread_local! {
    /// Test-only override of the cap, scoped to the thread that set it.
    ///
    /// Lowering a process-wide cap would leak into every test running beside
    /// it (libtest runs them in parallel); a thread-local is seen only by the
    /// test that set it. Production reads the constant; this path does not
    /// exist outside `cfg(test)`.
    static MAX_ATTACHMENT_SIZE_OVERRIDE: std::cell::Cell<Option<u64>> =
        const { std::cell::Cell::new(None) };
}

pub fn max_attachment_size() -> u64 {
    #[cfg(test)]
    if let Some(size) = MAX_ATTACHMENT_SIZE_OVERRIDE.with(std::cell::Cell::get) {
        return size;
    }
    MAX_ATTACHMENT_SIZE
}

/// Lower (or raise) the cap for the current test only.
///
/// Returns a guard that restores the previous value on drop, so a test that
/// panics mid-way cannot leave the override set for whatever libtest schedules
/// on that thread next.
#[cfg(test)]
#[must_use]
pub(crate) fn override_max_attachment_size(size: u64) -> MaxAttachmentSizeOverride {
    let previous = MAX_ATTACHMENT_SIZE_OVERRIDE.with(|cell| cell.replace(Some(size)));
    MaxAttachmentSizeOverride { previous }
}

#[cfg(test)]
pub(crate) struct MaxAttachmentSizeOverride {
    previous: Option<u64>,
}

#[cfg(test)]
impl Drop for MaxAttachmentSizeOverride {
    fn drop(&mut self) {
        MAX_ATTACHMENT_SIZE_OVERRIDE.with(|cell| cell.set(self.previous));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let plaintext = b"Hello, Signal-compatible encryption!";

        // Encrypt
        let encrypted = AttachmentCrypto::encrypt(plaintext).expect("encryption should succeed");

        // Verify ciphertext structure: IV (16) + encrypted + HMAC (32)
        assert!(encrypted.ciphertext.len() >= MIN_CIPHERTEXT_LENGTH);
        assert_eq!(encrypted.plaintext_size, plaintext.len());

        // Decrypt
        let decrypted = AttachmentCrypto::decrypt(
            &encrypted.ciphertext,
            &encrypted.key,
            &encrypted.digest,
            encrypted.plaintext_size,
        )
        .expect("decryption should succeed");

        assert_eq!(decrypted.as_slice(), plaintext);
    }

    #[test]
    fn test_encrypt_decrypt_empty() {
        let plaintext = b"";

        let encrypted = AttachmentCrypto::encrypt(plaintext).expect("encryption should succeed");

        let decrypted = AttachmentCrypto::decrypt(
            &encrypted.ciphertext,
            &encrypted.key,
            &encrypted.digest,
            encrypted.plaintext_size,
        )
        .expect("decryption should succeed");

        assert_eq!(decrypted.as_slice(), plaintext);
    }

    #[test]
    fn test_encrypt_decrypt_large() {
        // Test with 1MB of data
        let plaintext: Vec<u8> = (0..1024 * 1024).map(|i| (i % 256) as u8).collect();

        let encrypted = AttachmentCrypto::encrypt(&plaintext).expect("encryption should succeed");

        let decrypted = AttachmentCrypto::decrypt(
            &encrypted.ciphertext,
            &encrypted.key,
            &encrypted.digest,
            encrypted.plaintext_size,
        )
        .expect("decryption should succeed");

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_digest_mismatch() {
        let plaintext = b"test data";
        let encrypted = AttachmentCrypto::encrypt(plaintext).expect("encryption should succeed");

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
        let encrypted = AttachmentCrypto::encrypt(plaintext).expect("encryption should succeed");

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

        assert!(matches!(
            result,
            Err(CryptoError::AttachmentDecryptionFailed(_))
        ));
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
            assert!(
                padded >= size,
                "padded size should be >= original for size {}",
                size
            );
            assert_eq!(
                padded % 16,
                0,
                "padded size should be multiple of 16 for size {}",
                size
            );
        }
    }

    #[test]
    fn test_different_plaintexts_different_ciphertexts() {
        let encrypted1 = AttachmentCrypto::encrypt(b"hello").expect("encryption should succeed");
        let encrypted2 = AttachmentCrypto::encrypt(b"hello").expect("encryption should succeed");

        // Same plaintext should produce different ciphertext (random IV and key)
        assert_ne!(encrypted1.ciphertext, encrypted2.ciphertext);
        assert_ne!(encrypted1.key, encrypted2.key);
        assert_ne!(encrypted1.digest, encrypted2.digest);
    }

    #[test]
    fn test_key_length() {
        let encrypted = AttachmentCrypto::encrypt(b"test").expect("encryption should succeed");

        assert_eq!(encrypted.key.len(), KEY_SET_LENGTH);
        assert_eq!(KEY_SET_LENGTH, 64);
    }

    #[test]
    fn test_ciphertext_format() {
        let plaintext = b"test message";
        let encrypted = AttachmentCrypto::encrypt(plaintext).expect("encryption should succeed");

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
        let result = AttachmentCrypto::encrypt_file_to_file(input_file.path(), output_file.path())
            .expect("streaming encryption should succeed");

        assert_eq!(result.plaintext_size, plaintext.len() as u64);

        // Read ciphertext and decrypt with in-memory decrypt
        let ciphertext = std::fs::read(output_file.path()).unwrap();
        assert_eq!(ciphertext.len() as u64, result.ciphertext_size);

        let decrypted =
            AttachmentCrypto::decrypt(&ciphertext, &result.key, &result.digest, plaintext.len())
                .expect("decryption should succeed");

        assert_eq!(decrypted.as_slice(), plaintext);
    }

    #[test]
    fn test_streaming_encrypt_large_file() {
        // 1MB of patterned data
        let plaintext: Vec<u8> = (0..1024 * 1024).map(|i| (i % 256) as u8).collect();

        let input_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(input_file.path(), &plaintext).unwrap();

        let output_file = tempfile::NamedTempFile::new().unwrap();

        let result = AttachmentCrypto::encrypt_file_to_file(input_file.path(), output_file.path())
            .expect("streaming encryption should succeed");

        let ciphertext = std::fs::read(output_file.path()).unwrap();

        let decrypted =
            AttachmentCrypto::decrypt(&ciphertext, &result.key, &result.digest, plaintext.len())
                .expect("decryption should succeed");

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

        let result = AttachmentCrypto::encrypt_file_to_file(input_file.path(), output_file.path())
            .expect("streaming encryption should succeed");

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

        let result = AttachmentCrypto::encrypt_file_to_file(input_file.path(), output_file.path())
            .expect("streaming encryption of empty file should succeed");

        assert_eq!(result.plaintext_size, 0);

        let ciphertext = std::fs::read(output_file.path()).unwrap();
        let decrypted = AttachmentCrypto::decrypt(&ciphertext, &result.key, &result.digest, 0)
            .expect("decryption of empty should succeed");

        assert!(decrypted.is_empty());
    }

    #[test]
    fn test_streaming_encrypt_key_length() {
        let input_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(input_file.path(), b"key length test").unwrap();

        let output_file = tempfile::NamedTempFile::new().unwrap();

        let result = AttachmentCrypto::encrypt_file_to_file(input_file.path(), output_file.path())
            .expect("streaming encryption should succeed");

        assert_eq!(result.key.len(), KEY_SET_LENGTH);
    }

    #[test]
    fn test_max_attachment_size_enforcement() {
        let _cap = override_max_attachment_size(100);

        let plaintext = vec![42u8; 200]; // 200 bytes, exceeds 100 byte limit

        let input_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(input_file.path(), &plaintext).unwrap();

        let output_file = tempfile::NamedTempFile::new().unwrap();

        let result = AttachmentCrypto::encrypt_file_to_file(input_file.path(), output_file.path());

        assert!(matches!(
            result,
            Err(CryptoError::AttachmentTooLarge { .. })
        ));
    }

    /// Verify decryption-failure metric events fire on the attachment path.
    ///
    /// Uses a minimal Subscriber implementation (not a Layer over Registry)
    /// to avoid interference from any global default subscriber another test
    /// may install. See the "Operational metrics" section for targets.
    #[test]
    fn decrypt_failure_metric_fires_on_attachment_error() {
        use std::sync::{Arc, Mutex};
        use tracing::span;
        use tracing::subscriber::with_default;
        use tracing::{Event, Metadata, Subscriber};

        #[derive(Clone, Default)]
        struct CaptureSub {
            events: Arc<Mutex<Vec<(String, String)>>>,
        }
        impl Subscriber for CaptureSub {
            fn register_callsite(
                &self,
                _meta: &'static Metadata<'static>,
            ) -> tracing::subscriber::Interest {
                // Force per-event `enabled()` evaluation so prior test
                // subscribers' cached "never" interest does not silence us.
                tracing::subscriber::Interest::sometimes()
            }
            fn enabled(&self, metadata: &Metadata<'_>) -> bool {
                metadata.target().starts_with("chatter::crypto::")
            }
            fn new_span(&self, _attrs: &span::Attributes<'_>) -> span::Id {
                span::Id::from_u64(1)
            }
            fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
            fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
            fn event(&self, event: &Event<'_>) {
                let target = event.metadata().target().to_string();
                struct KindVisitor(Option<String>);
                impl tracing::field::Visit for KindVisitor {
                    fn record_debug(
                        &mut self,
                        field: &tracing::field::Field,
                        value: &dyn std::fmt::Debug,
                    ) {
                        if field.name() == "kind" && self.0.is_none() {
                            // Strip surrounding quotes if Debug-formatted.
                            let s = format!("{:?}", value);
                            self.0 = Some(s.trim_matches('"').to_string());
                        }
                    }
                    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                        if field.name() == "kind" {
                            self.0 = Some(value.to_string());
                        }
                    }
                }
                let mut v = KindVisitor(None);
                event.record(&mut v);
                self.events
                    .lock()
                    .unwrap()
                    .push((target, v.0.unwrap_or_default()));
            }
            fn enter(&self, _: &span::Id) {}
            fn exit(&self, _: &span::Id) {}
        }

        fn captured_hit(captured: &[(String, String)]) -> bool {
            captured
                .iter()
                .any(|(t, k)| t == "chatter::crypto::decrypt_failure" && k == "attachment")
        }

        let sub = CaptureSub::default();
        let events = sub.events.clone();

        // Retried, because what is being raced here is process-global and cannot
        // be locked out from inside one test. `register_callsite` above asks for
        // per-event evaluation, but tracing caches interest per callsite for the
        // whole process, and `rebuild_interest_cache` only re-asks at the moment
        // it is called — another test on another thread can register or rebuild in
        // the window before our event fires, leaving the callsite cached as
        // "never" for that one emission. Each attempt is independent and a hit
        // ends the loop, so this costs nothing on the common path and removes a
        // failure that reported broken code when only the cache was stale.
        for _ in 0..8 {
            let attempt = sub.clone();
            with_default(attempt, || {
                tracing::callsite::rebuild_interest_cache();
                // Trigger an attachment decrypt failure: ciphertext too short.
                let key = [0u8; KEY_SET_LENGTH];
                let digest = [0u8; 32];
                let bad = vec![0u8; 4]; // shorter than MIN_CIPHERTEXT_LENGTH
                let _ = AttachmentCrypto::decrypt(&bad, &key, &digest, 0);
            });
            if captured_hit(&events.lock().unwrap()) {
                break;
            }
        }

        let captured = events.lock().unwrap().clone();
        assert!(
            captured_hit(&captured),
            "expected decrypt_failure event with kind=attachment, got: {captured:?}"
        );
    }
}
