//! Streaming attachment decryption.
//!
//! Decrypts large attachments in chunks without loading the entire
//! ciphertext into memory. Uses **single-pass** approach:
//!
//! 1. Read IV, initialize AES-CBC decryptor + HMAC + SHA256
//! 2. For each chunk: compute HMAC/SHA256 AND decrypt simultaneously
//! 3. Verify HMAC and digest at the end
//!
//! This is secure for encrypt-then-MAC with AES-CBC: worst case is we
//! decrypt garbage from tampered ciphertext, but we catch it at verification.
//!
//! ## Memory Usage
//!
//! For a 200MB attachment: ~512KB buffer + plaintext output
//!
//! ## Performance
//!
//! Uses OpenSSL for hardware-accelerated crypto (SHA-NI + AES-NI).
//! Single-pass reads the blob once, roughly halving I/O time.

use std::io::{Read, Seek, SeekFrom};

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecryptMut, KeyIvInit};
use subtle::ConstantTimeEq;

use crate::crypto::attachment_crypto::KEY_SET_LENGTH;
use crate::crypto::hmac_compat::Sha256Stream;
use crate::error::CryptoError;

type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;

/// AES block size (bytes).
const AES_BLOCK: usize = 16;
const IV_LENGTH: usize = 16;
const AES_KEY_LENGTH: usize = 32;
const HMAC_LENGTH: usize = 32;
const CHUNK_SIZE: usize = 512 * 1024; // 512KB chunks - larger for better SQLite blob throughput

/// Streaming decryptor for large attachments.
///
/// Decrypts attachment data from a streaming source (e.g., SQLite BLOB)
/// without loading the entire ciphertext into memory.
pub struct StreamingDecryptor<R: Read + Seek> {
    reader: R,
    aes_key: [u8; AES_KEY_LENGTH],
    mac_key: [u8; 32],
    expected_digest: [u8; 32],
    plaintext_size: usize,
    total_size: usize,
    verified: bool,
    decryption_position: usize,
    decryptor: Option<Aes256CbcDec>,
    /// Partial-block remainder (CBC works per 16-byte block; reads may not be
    /// block-aligned). Carried between `read_decrypted` calls.
    carry: Vec<u8>,
}

impl<R: Read + Seek> StreamingDecryptor<R> {
    /// Create a new streaming decryptor.
    ///
    /// # Arguments
    ///
    /// * `reader` - Source of encrypted data (must implement Read + Seek)
    /// * `key` - 64-byte key (32 AES + 32 HMAC)
    /// * `expected_digest` - Expected SHA256 digest for verification
    /// * `plaintext_size` - Expected size of decrypted data
    /// * `total_size` - Total size of ciphertext (including IV and HMAC)
    pub fn new(
        reader: R,
        key: &[u8; KEY_SET_LENGTH],
        expected_digest: &[u8; 32],
        plaintext_size: usize,
        total_size: usize,
    ) -> Self {
        let mut aes_key = [0u8; AES_KEY_LENGTH];
        let mut mac_key = [0u8; 32];
        aes_key.copy_from_slice(&key[..AES_KEY_LENGTH]);
        mac_key.copy_from_slice(&key[AES_KEY_LENGTH..]);

        // SECURITY: plaintext_size comes from the sender-controlled
        // AttachmentPointer. Clamp it to the actual ciphertext capacity
        // (total_size minus IV+HMAC) — real plaintext can never exceed the
        // ciphertext body, so this never truncates legitimate data. Without it
        // a hostile size triggers a huge `Vec::with_capacity` (alloc abort) and
        // an `encrypted_len - plaintext_size` usize underflow panic — a remote
        // client DoS from a single attachment, before any MAC verification.
        let ciphertext_body = total_size.saturating_sub(IV_LENGTH + HMAC_LENGTH);
        let plaintext_size = plaintext_size.min(ciphertext_body);

        Self {
            reader,
            aes_key,
            mac_key,
            expected_digest: *expected_digest,
            plaintext_size,
            total_size,
            verified: false,
            decryption_position: 0,
            decryptor: None,
            carry: Vec::new(),
        }
    }

    /// Verify the integrity of the ciphertext (pass 1).
    ///
    /// Streams through the entire ciphertext computing SHA256 and HMAC
    /// without holding the data in memory. Must be called before decryption.
    ///
    /// # Returns
    ///
    /// `Ok(())` if verification passes, `Err` if digest or HMAC mismatch.
    pub fn verify(&mut self) -> Result<(), CryptoError> {
        if self.verified {
            return Ok(());
        }

        // Seek to beginning
        self.reader
            .seek(SeekFrom::Start(0))
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("seek: {}", e)))?;

        let hmac_start = self.total_size - HMAC_LENGTH;

        // Initialize hashers using OpenSSL (hardware accelerated)
        let mut digest_hasher = Sha256Stream::new()
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("SHA256 init: {}", e)))?;
        let mut hmac = crate::crypto::hmac_compat::hmac_init(&self.mac_key)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("HMAC init: {}", e)))?;

        // Read and hash in chunks
        let mut chunk = vec![0u8; CHUNK_SIZE];
        let mut bytes_read = 0;

        while bytes_read < hmac_start {
            let to_read = CHUNK_SIZE.min(hmac_start - bytes_read);
            let n = self
                .reader
                .read(&mut chunk[..to_read])
                .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("read: {}", e)))?;

            if n == 0 {
                break;
            }

            // Both digest and HMAC include IV + encrypted data
            digest_hasher.update(&chunk[..n])
                .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("SHA256: {}", e)))?;
            hmac.update(&chunk[..n])
                .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("HMAC: {}", e)))?;
            bytes_read += n;
        }

        // Read the HMAC bytes
        let mut expected_hmac = [0u8; HMAC_LENGTH];
        self.reader
            .read_exact(&mut expected_hmac)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("read hmac: {}", e)))?;

        // Digest also includes HMAC bytes
        digest_hasher.update(&expected_hmac)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("SHA256: {}", e)))?;

        // Finalize
        let actual_digest = digest_hasher.finish()
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("SHA256 finalize: {}", e)))?;
        let computed_hmac = hmac.finalize()
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("HMAC finalize: {}", e)))?;

        // Verify digest (constant-time)
        if actual_digest.as_slice().ct_eq(&self.expected_digest).unwrap_u8() != 1 {
            return Err(CryptoError::AttachmentDigestMismatch);
        }

        // Verify HMAC (constant-time)
        if computed_hmac.ct_eq(&expected_hmac).unwrap_u8() != 1 {
            return Err(CryptoError::AttachmentMacMismatch);
        }

        self.verified = true;
        tracing::debug!(
            total_size = self.total_size,
            plaintext_size = self.plaintext_size,
            "Streaming verification passed"
        );

        Ok(())
    }

    /// Initialize the AES decryptor for streaming decryption (pass 2).
    ///
    /// Must call `verify()` first.
    pub fn init_decryption(&mut self) -> Result<(), CryptoError> {
        if !self.verified {
            return Err(CryptoError::AttachmentDecryptionFailed(
                "must verify before decryption".into(),
            ));
        }

        // Seek to beginning and read IV
        self.reader
            .seek(SeekFrom::Start(0))
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("seek: {}", e)))?;

        let mut iv = [0u8; IV_LENGTH];
        self.reader
            .read_exact(&mut iv)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("read iv: {}", e)))?;

        // Create AES-CBC decryptor (raw, no block padding)
        let decryptor = Aes256CbcDec::new_from_slices(&self.aes_key, &iv)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("AES init: {e}")))?;

        self.decryptor = Some(decryptor);
        self.carry.clear();
        self.decryption_position = 0;

        Ok(())
    }

    /// Read and decrypt a chunk of data.
    ///
    /// Returns the number of plaintext bytes written to `output`.
    /// Returns 0 when all data has been decrypted.
    ///
    /// # Arguments
    ///
    /// * `output` - Buffer to write decrypted data (should be at least CHUNK_SIZE + 16)
    pub fn read_decrypted(&mut self, output: &mut [u8]) -> Result<usize, CryptoError> {
        if self.decryptor.is_none() {
            return Err(CryptoError::AttachmentDecryptionFailed(
                "decryption not initialized".into(),
            ));
        }

        // Calculate how much ciphertext remains
        let encrypted_start = IV_LENGTH;
        let encrypted_end = self.total_size - HMAC_LENGTH;
        let encrypted_len = encrypted_end - encrypted_start;
        let ciphertext_remaining = encrypted_len.saturating_sub(self.decryption_position);

        if ciphertext_remaining == 0 {
            return Ok(0);
        }

        // Read a chunk of ciphertext
        let to_read = CHUNK_SIZE.min(ciphertext_remaining);
        let mut ciphertext = vec![0u8; to_read];
        let n = self
            .reader
            .read(&mut ciphertext[..to_read])
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("read: {}", e)))?;

        if n == 0 {
            return Ok(0);
        }
        self.decryption_position += n;

        // Decrypt the whole-block prefix of (carry + new bytes) into `output`,
        // keeping any partial-block remainder for the next call.
        self.carry.extend_from_slice(&ciphertext[..n]);
        let full = self.carry.len() - (self.carry.len() % AES_BLOCK);
        let decryptor = self.decryptor.as_mut().unwrap();
        for blk in self.carry[..full].chunks_exact_mut(AES_BLOCK) {
            decryptor.decrypt_block_mut(GenericArray::from_mut_slice(blk));
        }
        output[..full].copy_from_slice(&self.carry[..full]);
        self.carry.drain(..full);
        let decrypted_len = full;

        // Truncate to plaintext size if we're at the end
        if self.decryption_position >= encrypted_len {
            // This is the last chunk - may include zero padding
            let padding = encrypted_len - self.plaintext_size;
            let actual_len = decrypted_len.saturating_sub(padding);
            return Ok(actual_len);
        }

        Ok(decrypted_len)
    }

    /// Finalize decryption and return any remaining data.
    pub fn finalize(&mut self, _output: &mut [u8]) -> Result<usize, CryptoError> {
        if self.decryptor.is_none() {
            return Err(CryptoError::AttachmentDecryptionFailed(
                "decryption not initialized".into(),
            ));
        }
        // Raw CBC, no padding: every block is emitted by read_decrypted and the
        // ciphertext is block-aligned, so nothing remains buffered.
        debug_assert!(self.carry.is_empty(), "CBC carry not block-aligned at finalize");
        Ok(0)
    }

    /// Decrypt the entire ciphertext in a single pass.
    ///
    /// Reads the blob once, computing HMAC/SHA256 while decrypting.
    /// Verifies integrity at the end. This is ~2x faster than two-pass
    /// for large files since it only reads the data once.
    ///
    /// Security: For encrypt-then-MAC with AES-CBC, decrypting before
    /// verification is safe - worst case we decrypt garbage from tampered
    /// ciphertext, but we detect and reject it at the end.
    pub fn decrypt_all(&mut self) -> Result<Vec<u8>, CryptoError> {
        use std::time::Instant;
        let total_start = Instant::now();
        let mut read_time = std::time::Duration::ZERO;
        let mut decrypt_time = std::time::Duration::ZERO;
        let mut hash_time = std::time::Duration::ZERO;

        // Seek to beginning
        self.reader
            .seek(SeekFrom::Start(0))
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("seek: {}", e)))?;

        // Read IV
        let mut iv = [0u8; IV_LENGTH];
        self.reader
            .read_exact(&mut iv)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("read iv: {}", e)))?;

        // Initialize all processors using OpenSSL (hardware accelerated)
        let mut digest_hasher = Sha256Stream::new()
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("SHA256 init: {}", e)))?;
        let mut hmac = crate::crypto::hmac_compat::hmac_init(&self.mac_key)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("HMAC init: {}", e)))?;

        let mut decryptor = Aes256CbcDec::new_from_slices(&self.aes_key, &iv)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("AES init: {e}")))?;
        // Partial-block remainder between reads (raw CBC, no padding).
        let mut carry: Vec<u8> = Vec::new();

        // IV is included in both digest and HMAC
        digest_hasher.update(&iv)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("SHA256 update: {}", e)))?;
        hmac.update(&iv)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("HMAC update: {}", e)))?;

        // Calculate positions
        let encrypted_start = IV_LENGTH;
        let hmac_start = self.total_size - HMAC_LENGTH;
        let encrypted_len = hmac_start - encrypted_start;

        // Pre-allocate output
        let mut plaintext = Vec::with_capacity(self.plaintext_size);
        let mut ciphertext_chunk = vec![0u8; CHUNK_SIZE];
        let mut bytes_read = 0;

        // Single pass: read ciphertext, update hashes, decrypt
        while bytes_read < encrypted_len {
            let to_read = CHUNK_SIZE.min(encrypted_len - bytes_read);

            // Time the read (SQLite BLOB / SQLCipher)
            let read_start = Instant::now();
            let n = self
                .reader
                .read(&mut ciphertext_chunk[..to_read])
                .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("read: {}", e)))?;
            read_time += read_start.elapsed();

            if n == 0 {
                break;
            }

            // Time the hash computations (OpenSSL hardware accelerated)
            let hash_start = Instant::now();
            digest_hasher.update(&ciphertext_chunk[..n])
                .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("SHA256: {}", e)))?;
            hmac.update(&ciphertext_chunk[..n])
                .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("HMAC: {}", e)))?;
            hash_time += hash_start.elapsed();

            // Time the AES decryption: decrypt whole blocks, keep the remainder.
            let decrypt_start = Instant::now();
            carry.extend_from_slice(&ciphertext_chunk[..n]);
            let full = carry.len() - (carry.len() % AES_BLOCK);
            for blk in carry[..full].chunks_exact_mut(AES_BLOCK) {
                decryptor.decrypt_block_mut(GenericArray::from_mut_slice(blk));
            }
            plaintext.extend_from_slice(&carry[..full]);
            carry.drain(..full);
            decrypt_time += decrypt_start.elapsed();

            bytes_read += n;
        }

        // Raw CBC, no padding: ciphertext is block-aligned, nothing buffered.
        debug_assert!(carry.is_empty(), "CBC carry not block-aligned");

        // Read and verify HMAC
        let mut expected_hmac = [0u8; HMAC_LENGTH];
        self.reader
            .read_exact(&mut expected_hmac)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("read hmac: {}", e)))?;

        // HMAC is also included in digest
        digest_hasher.update(&expected_hmac)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("SHA256: {}", e)))?;

        // Verify HMAC (constant-time)
        let computed_hmac = hmac.finalize()
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("HMAC finalize: {}", e)))?;
        if computed_hmac.ct_eq(&expected_hmac).unwrap_u8() != 1 {
            return Err(CryptoError::AttachmentMacMismatch);
        }

        // Verify digest (constant-time)
        let actual_digest = digest_hasher.finish()
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("SHA256 finalize: {}", e)))?;
        if actual_digest.as_slice().ct_eq(&self.expected_digest).unwrap_u8() != 1 {
            return Err(CryptoError::AttachmentDigestMismatch);
        }

        // Truncate to exact plaintext size (remove padding)
        plaintext.truncate(self.plaintext_size);

        self.verified = true;

        // Log timing breakdown
        let total_time = total_start.elapsed();
        let size_mb = self.plaintext_size as f64 / (1024.0 * 1024.0);
        tracing::info!(
            "DECRYPT TIMING: total={:.2}s, read(SQLCipher)={:.2}s, decrypt(AES)={:.2}s, hash={:.2}s, size={:.1}MB, read_throughput={:.1}MB/s",
            total_time.as_secs_f64(),
            read_time.as_secs_f64(),
            decrypt_time.as_secs_f64(),
            hash_time.as_secs_f64(),
            size_mb,
            size_mb / read_time.as_secs_f64()
        );

        Ok(plaintext)
    }
}

/// Incremental MAC verifier for chunk-by-chunk attachment verification.
///
/// Verifies per-chunk HMAC-SHA256 digests as ciphertext arrives, allowing
/// early abort on tampered data without downloading the entire file.
///
/// # Signal Spec
///
/// - Chunk size: 64KB (65,536 bytes) of ciphertext (IV + encrypted data, before final HMAC)
/// - Each chunk gets its own HMAC-SHA256 using the same MAC key
/// - Digests are concatenated in order in the `incremental_mac` field
///
/// # Backward Compatibility
///
/// If `expected_macs` is empty, all operations are no-ops (legacy messages).
pub struct IncrementalMacVerifier {
    mac_key: [u8; 32],
    expected_macs: Vec<[u8; 32]>,
    chunk_size: usize,
    chunk_index: usize,
    accumulator: Vec<u8>,
}

impl IncrementalMacVerifier {
    /// Create a new verifier from the MAC key and concatenated expected digests.
    ///
    /// # Arguments
    ///
    /// * `mac_key` - 32-byte HMAC-SHA256 key (second half of the 64-byte key set)
    /// * `incremental_mac` - Concatenated 32-byte digests from AttachmentPointer
    /// * `chunk_size` - Chunk size in bytes (must be 65536 per Signal spec)
    ///
    /// Returns `None` if `incremental_mac` is empty (backward compat) or if
    /// its length is not a multiple of 32 (malformed input — fail fast rather
    /// than silently truncating).
    pub fn new(mac_key: &[u8; 32], incremental_mac: &[u8], chunk_size: u32) -> Option<Self> {
        if incremental_mac.is_empty() || chunk_size == 0 {
            return None;
        }
        if incremental_mac.len() % 32 != 0 {
            return None;
        }

        // Parse concatenated digests into individual 32-byte chunks
        let num_macs = incremental_mac.len() / 32;
        let mut expected_macs = Vec::with_capacity(num_macs);
        for i in 0..num_macs {
            let mut mac = [0u8; 32];
            mac.copy_from_slice(&incremental_mac[i * 32..(i + 1) * 32]);
            expected_macs.push(mac);
        }

        Some(Self {
            mac_key: *mac_key,
            expected_macs,
            chunk_size: chunk_size as usize,
            chunk_index: 0,
            accumulator: Vec::with_capacity(chunk_size as usize),
        })
    }

    /// Feed ciphertext bytes for incremental verification.
    ///
    /// Accumulates data and verifies complete chunks against expected MACs.
    /// Returns `Err(IncrementalMacMismatch)` on first bad chunk.
    pub fn feed(&mut self, data: &[u8]) -> Result<(), CryptoError> {
        self.accumulator.extend_from_slice(data);

        while self.accumulator.len() >= self.chunk_size {
            self.verify_chunk(&self.accumulator[..self.chunk_size].to_vec())?;
            self.accumulator.drain(..self.chunk_size);
        }

        Ok(())
    }

    /// Finalize verification — check any remaining partial chunk.
    ///
    /// Must be called after all ciphertext (excluding the trailing HMAC) has been fed.
    pub fn finalize(&mut self) -> Result<(), CryptoError> {
        if !self.accumulator.is_empty() {
            let remaining = std::mem::take(&mut self.accumulator);
            self.verify_chunk(&remaining)?;
        }

        // Verify we consumed all expected MACs
        if self.chunk_index != self.expected_macs.len() {
            return Err(CryptoError::IncrementalMacMismatch {
                chunk_index: self.chunk_index,
            });
        }

        Ok(())
    }

    /// Verify a single chunk against its expected MAC.
    fn verify_chunk(&mut self, chunk: &[u8]) -> Result<(), CryptoError> {
        if self.chunk_index >= self.expected_macs.len() {
            return Err(CryptoError::IncrementalMacMismatch {
                chunk_index: self.chunk_index,
            });
        }

        let mut signer = crate::crypto::hmac_compat::hmac_init(&self.mac_key)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("HMAC init: {}", e)))?;
        signer.update(chunk)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("HMAC update: {}", e)))?;
        let computed = signer.finalize()
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("HMAC finalize: {}", e)))?;

        // Constant-time comparison
        if computed.ct_eq(&self.expected_macs[self.chunk_index]).unwrap_u8() != 1 {
            return Err(CryptoError::IncrementalMacMismatch {
                chunk_index: self.chunk_index,
            });
        }

        self.chunk_index += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::attachment_crypto::{AttachmentCrypto, INCREMENTAL_MAC_CHUNK_SIZE};
    use std::io::Cursor;

    #[test]
    fn plaintext_size_clamped_to_ciphertext_capacity() {
        // Hostile AttachmentPointer: plaintext_size far larger than the actual
        // ciphertext. Must be clamped so `Vec::with_capacity` can't abort and
        // the `encrypted_len - plaintext_size` padding calc can't underflow.
        let key = [0u8; KEY_SET_LENGTH];
        let digest = [0u8; 32];

        let total_size = 100; // IV(16) + HMAC(32) overhead → body = 52
        let d = StreamingDecryptor::new(Cursor::new(Vec::<u8>::new()), &key, &digest, usize::MAX, total_size);
        assert_eq!(d.plaintext_size, total_size - IV_LENGTH - HMAC_LENGTH);

        // Degenerate total_size smaller than the IV+HMAC overhead must clamp to
        // 0, not underflow.
        let d2 = StreamingDecryptor::new(Cursor::new(Vec::<u8>::new()), &key, &digest, 1000, 10);
        assert_eq!(d2.plaintext_size, 0);
    }

    #[test]
    fn test_streaming_decrypt_small() {
        let plaintext = b"Hello, streaming decryption!";
        let encrypted = AttachmentCrypto::encrypt(plaintext).unwrap();

        let cursor = Cursor::new(encrypted.ciphertext.clone());
        let mut decryptor = StreamingDecryptor::new(
            cursor,
            &encrypted.key,
            &encrypted.digest,
            encrypted.plaintext_size,
            encrypted.ciphertext.len(),
        );

        let decrypted = decryptor.decrypt_all().unwrap();
        assert_eq!(decrypted.as_slice(), plaintext);
    }

    #[test]
    fn test_streaming_decrypt_large() {
        // 1MB test
        let plaintext: Vec<u8> = (0..1024 * 1024).map(|i| (i % 256) as u8).collect();
        let encrypted = AttachmentCrypto::encrypt(&plaintext).unwrap();

        let cursor = Cursor::new(encrypted.ciphertext.clone());
        let mut decryptor = StreamingDecryptor::new(
            cursor,
            &encrypted.key,
            &encrypted.digest,
            encrypted.plaintext_size,
            encrypted.ciphertext.len(),
        );

        let decrypted = decryptor.decrypt_all().unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_streaming_verify_fails_on_bad_digest() {
        let plaintext = b"test data";
        let encrypted = AttachmentCrypto::encrypt(plaintext).unwrap();

        let mut bad_digest = encrypted.digest;
        bad_digest[0] ^= 0xFF;

        let cursor = Cursor::new(encrypted.ciphertext.clone());
        let mut decryptor = StreamingDecryptor::new(
            cursor,
            &encrypted.key,
            &bad_digest,
            encrypted.plaintext_size,
            encrypted.ciphertext.len(),
        );

        let result = decryptor.verify();
        assert!(matches!(result, Err(CryptoError::AttachmentDigestMismatch)));
    }

    #[test]
    fn test_streaming_verify_fails_on_tampered_data() {
        let plaintext = b"test data";
        let encrypted = AttachmentCrypto::encrypt(plaintext).unwrap();

        // Tamper with ciphertext
        let mut tampered = encrypted.ciphertext.clone();
        tampered[20] ^= 0xFF;

        let cursor = Cursor::new(tampered);
        let mut decryptor = StreamingDecryptor::new(
            cursor,
            &encrypted.key,
            &encrypted.digest,
            encrypted.plaintext_size,
            encrypted.ciphertext.len(),
        );

        let result = decryptor.verify();
        // Either digest or HMAC will fail
        assert!(result.is_err());
    }

    #[test]
    fn test_streaming_chunked_read() {
        let plaintext: Vec<u8> = (0..100_000).map(|i| (i % 256) as u8).collect();
        let encrypted = AttachmentCrypto::encrypt(&plaintext).unwrap();

        let cursor = Cursor::new(encrypted.ciphertext.clone());
        let mut decryptor = StreamingDecryptor::new(
            cursor,
            &encrypted.key,
            &encrypted.digest,
            encrypted.plaintext_size,
            encrypted.ciphertext.len(),
        );

        decryptor.verify().unwrap();
        decryptor.init_decryption().unwrap();

        // Read in chunks
        let mut result = Vec::new();
        let mut output = vec![0u8; CHUNK_SIZE + 16];

        loop {
            let n = decryptor.read_decrypted(&mut output).unwrap();
            if n == 0 {
                break;
            }
            result.extend_from_slice(&output[..n]);
        }

        let n = decryptor.finalize(&mut output).unwrap();
        result.extend_from_slice(&output[..n]);
        result.truncate(encrypted.plaintext_size);

        assert_eq!(result, plaintext);
    }

    // =========================================================================
    // IncrementalMacVerifier tests
    // =========================================================================

    #[test]
    fn test_incremental_mac_verifier_empty_is_none() {
        let mac_key = [0u8; 32];
        assert!(IncrementalMacVerifier::new(&mac_key, &[], 0).is_none());
        assert!(IncrementalMacVerifier::new(&mac_key, &[], 65536).is_none());
        assert!(IncrementalMacVerifier::new(&mac_key, &[1; 32], 0).is_none());
    }

    #[test]
    fn test_incremental_mac_verifier_roundtrip() {
        let _cap = crate::crypto::attachment_crypto::SIZE_CAP_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use crate::crypto::attachment_crypto::AttachmentCrypto;
        use tempfile::NamedTempFile;
        use std::io::Write;

        // Create a file large enough for incremental MAC (>50KB)
        let mut input_file = NamedTempFile::new().unwrap();
        let plaintext: Vec<u8> = (0..200_000).map(|i| (i % 256) as u8).collect();
        input_file.write_all(&plaintext).unwrap();
        input_file.flush().unwrap();

        let output_file = NamedTempFile::new().unwrap();

        // Encrypt with incremental MAC
        let result = AttachmentCrypto::encrypt_file_to_file(
            input_file.path(),
            output_file.path(),
        ).unwrap();

        assert!(!result.incremental_mac.is_empty(), "incremental_mac should be populated for >50KB file");
        assert_eq!(result.incremental_chunk_size, INCREMENTAL_MAC_CHUNK_SIZE as u32);
        assert_eq!(result.incremental_mac.len() % 32, 0, "incremental_mac length must be multiple of 32");

        // Extract MAC key from the key set
        let mut mac_key = [0u8; 32];
        mac_key.copy_from_slice(&result.key[32..]);

        // Verify using IncrementalMacVerifier
        let mut verifier = IncrementalMacVerifier::new(
            &mac_key,
            &result.incremental_mac,
            result.incremental_chunk_size,
        ).expect("verifier should be Some for non-empty incremental_mac");

        // Read ciphertext and feed it to the verifier (excluding trailing HMAC)
        let ciphertext = std::fs::read(output_file.path()).unwrap();
        let ciphertext_without_hmac = &ciphertext[..ciphertext.len() - 32];
        verifier.feed(ciphertext_without_hmac).unwrap();
        verifier.finalize().unwrap();
    }

    #[test]
    fn test_incremental_mac_verifier_detects_tamper() {
        let _cap = crate::crypto::attachment_crypto::SIZE_CAP_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use crate::crypto::attachment_crypto::AttachmentCrypto;
        use tempfile::NamedTempFile;
        use std::io::Write;

        // Create a file large enough for multiple incremental MAC chunks
        let mut input_file = NamedTempFile::new().unwrap();
        let plaintext: Vec<u8> = (0..200_000).map(|i| (i % 256) as u8).collect();
        input_file.write_all(&plaintext).unwrap();
        input_file.flush().unwrap();

        let output_file = NamedTempFile::new().unwrap();

        let result = AttachmentCrypto::encrypt_file_to_file(
            input_file.path(),
            output_file.path(),
        ).unwrap();

        let mut mac_key = [0u8; 32];
        mac_key.copy_from_slice(&result.key[32..]);

        // Tamper with the ciphertext
        let mut ciphertext = std::fs::read(output_file.path()).unwrap();
        let ciphertext_without_hmac_len = ciphertext.len() - 32;
        // Tamper with a byte in the first chunk (after IV)
        ciphertext[20] ^= 0xFF;

        let mut verifier = IncrementalMacVerifier::new(
            &mac_key,
            &result.incremental_mac,
            result.incremental_chunk_size,
        ).unwrap();

        // Feed tampered ciphertext — should fail at first chunk
        let result = verifier.feed(&ciphertext[..ciphertext_without_hmac_len]);
        if result.is_ok() {
            // If first chunk hasn't been verified yet (data < chunk_size), finalize should catch it
            let finalize_result = verifier.finalize();
            assert!(
                finalize_result.is_err(),
                "finalize should detect tampered data"
            );
            match finalize_result.unwrap_err() {
                CryptoError::IncrementalMacMismatch { chunk_index } => {
                    assert_eq!(chunk_index, 0, "first chunk should fail");
                }
                other => panic!("expected IncrementalMacMismatch, got: {}", other),
            }
        } else {
            match result.unwrap_err() {
                CryptoError::IncrementalMacMismatch { chunk_index } => {
                    assert_eq!(chunk_index, 0, "first chunk should fail");
                }
                other => panic!("expected IncrementalMacMismatch, got: {}", other),
            }
        }
    }

    #[test]
    fn test_incremental_mac_small_file_no_mac() {
        let _cap = crate::crypto::attachment_crypto::SIZE_CAP_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use crate::crypto::attachment_crypto::AttachmentCrypto;
        use tempfile::NamedTempFile;
        use std::io::Write;

        // Create a file smaller than threshold (< 50KB)
        let mut input_file = NamedTempFile::new().unwrap();
        let plaintext = vec![42u8; 1024]; // 1KB
        input_file.write_all(&plaintext).unwrap();
        input_file.flush().unwrap();

        let output_file = NamedTempFile::new().unwrap();

        let result = AttachmentCrypto::encrypt_file_to_file(
            input_file.path(),
            output_file.path(),
        ).unwrap();

        assert!(result.incremental_mac.is_empty(), "small files should not have incremental MAC");
        assert_eq!(result.incremental_chunk_size, 0);
    }

    #[test]
    fn test_incremental_mac_chunked_feed() {
        let _cap = crate::crypto::attachment_crypto::SIZE_CAP_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use crate::crypto::attachment_crypto::AttachmentCrypto;
        use tempfile::NamedTempFile;
        use std::io::Write;

        // Create a file large enough for multiple chunks
        let mut input_file = NamedTempFile::new().unwrap();
        let plaintext: Vec<u8> = (0..300_000).map(|i| (i % 256) as u8).collect();
        input_file.write_all(&plaintext).unwrap();
        input_file.flush().unwrap();

        let output_file = NamedTempFile::new().unwrap();

        let result = AttachmentCrypto::encrypt_file_to_file(
            input_file.path(),
            output_file.path(),
        ).unwrap();

        let mut mac_key = [0u8; 32];
        mac_key.copy_from_slice(&result.key[32..]);

        let mut verifier = IncrementalMacVerifier::new(
            &mac_key,
            &result.incremental_mac,
            result.incremental_chunk_size,
        ).unwrap();

        // Feed in small chunks (simulating streaming download)
        let ciphertext = std::fs::read(output_file.path()).unwrap();
        let ciphertext_without_hmac = &ciphertext[..ciphertext.len() - 32];
        let feed_chunk_size = 4096; // 4KB at a time
        for chunk in ciphertext_without_hmac.chunks(feed_chunk_size) {
            verifier.feed(chunk).unwrap();
        }
        verifier.finalize().unwrap();
    }
}
