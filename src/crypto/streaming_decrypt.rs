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

use openssl::hash::{Hasher, MessageDigest};
use openssl::pkey::PKey;
use openssl::sign::Signer;
use openssl::symm::{Cipher, Crypter, Mode};
use subtle::ConstantTimeEq;

use crate::crypto::attachment_constants::*;
use crate::error::CryptoError;
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
    crypter: Option<Crypter>,
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

        Self {
            reader,
            aes_key,
            mac_key,
            expected_digest: *expected_digest,
            plaintext_size,
            total_size,
            verified: false,
            decryption_position: 0,
            crypter: None,
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
        let mut digest_hasher = Hasher::new(MessageDigest::sha256())
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("SHA256 init: {}", e)))?;
        let pkey = PKey::hmac(&self.mac_key)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("HMAC key: {}", e)))?;
        let mut hmac = Signer::new(MessageDigest::sha256(), &pkey)
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
        let computed_hmac = hmac.sign_to_vec()
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("HMAC finalize: {}", e)))?;

        // Verify digest (constant-time)
        if actual_digest.as_ref().ct_eq(&self.expected_digest).unwrap_u8() != 1 {
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

        // Create AES-CBC decryptor
        let cipher = Cipher::aes_256_cbc();
        let mut crypter = Crypter::new(cipher, Mode::Decrypt, &self.aes_key, Some(&iv))
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("OpenSSL init: {}", e)))?;
        crypter.pad(false); // No PKCS7 padding

        self.crypter = Some(crypter);
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
        let crypter = self.crypter.as_mut().ok_or_else(|| {
            CryptoError::AttachmentDecryptionFailed("decryption not initialized".into())
        })?;

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

        // Decrypt
        let decrypted_len = crypter
            .update(&ciphertext[..n], output)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("decrypt: {}", e)))?;

        self.decryption_position += n;

        // Truncate to plaintext size if we're at the end
        if self.decryption_position >= encrypted_len {
            // This is the last chunk - may include padding
            let padding = encrypted_len - self.plaintext_size;
            let actual_len = decrypted_len.saturating_sub(padding);
            return Ok(actual_len);
        }

        Ok(decrypted_len)
    }

    /// Finalize decryption and return any remaining data.
    pub fn finalize(&mut self, output: &mut [u8]) -> Result<usize, CryptoError> {
        let crypter = self.crypter.as_mut().ok_or_else(|| {
            CryptoError::AttachmentDecryptionFailed("decryption not initialized".into())
        })?;

        let len = crypter
            .finalize(output)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("finalize: {}", e)))?;

        Ok(len)
    }

    /// Decrypt the entire ciphertext to a Vec (two-pass: verify then decrypt).
    ///
    /// Use `decrypt_all_single_pass` for better performance.
    #[allow(dead_code)]
    pub fn decrypt_all_two_pass(&mut self) -> Result<Vec<u8>, CryptoError> {
        self.verify()?;
        self.init_decryption()?;

        let mut plaintext = Vec::with_capacity(self.plaintext_size);
        let mut output = vec![0u8; CHUNK_SIZE + 16];

        loop {
            let n = self.read_decrypted(&mut output)?;
            if n == 0 {
                break;
            }
            plaintext.extend_from_slice(&output[..n]);
        }

        let n = self.finalize(&mut output)?;
        plaintext.extend_from_slice(&output[..n]);
        plaintext.truncate(self.plaintext_size);

        Ok(plaintext)
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
        let mut digest_hasher = Hasher::new(MessageDigest::sha256())
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("SHA256 init: {}", e)))?;
        let pkey = PKey::hmac(&self.mac_key)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("HMAC key: {}", e)))?;
        let mut hmac = Signer::new(MessageDigest::sha256(), &pkey)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("HMAC init: {}", e)))?;

        let cipher = Cipher::aes_256_cbc();
        let mut crypter = Crypter::new(cipher, Mode::Decrypt, &self.aes_key, Some(&iv))
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("AES init: {}", e)))?;
        crypter.pad(false);

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
        let mut decrypt_buf = vec![0u8; CHUNK_SIZE + 16]; // Extra for block padding
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

            // Time the AES decryption
            let decrypt_start = Instant::now();
            let decrypted_len = crypter
                .update(&ciphertext_chunk[..n], &mut decrypt_buf)
                .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("decrypt: {}", e)))?;
            decrypt_time += decrypt_start.elapsed();

            plaintext.extend_from_slice(&decrypt_buf[..decrypted_len]);
            bytes_read += n;
        }

        // Finalize decryption
        let final_len = crypter
            .finalize(&mut decrypt_buf)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("finalize: {}", e)))?;
        plaintext.extend_from_slice(&decrypt_buf[..final_len]);

        // Read and verify HMAC
        let mut expected_hmac = [0u8; HMAC_LENGTH];
        self.reader
            .read_exact(&mut expected_hmac)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("read hmac: {}", e)))?;

        // HMAC is also included in digest
        digest_hasher.update(&expected_hmac)
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("SHA256: {}", e)))?;

        // Verify HMAC (constant-time)
        let computed_hmac = hmac.sign_to_vec()
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("HMAC finalize: {}", e)))?;
        if computed_hmac.ct_eq(&expected_hmac).unwrap_u8() != 1 {
            return Err(CryptoError::AttachmentMacMismatch);
        }

        // Verify digest (constant-time)
        let actual_digest = digest_hasher.finish()
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("SHA256 finalize: {}", e)))?;
        if actual_digest.as_ref().ct_eq(&self.expected_digest).unwrap_u8() != 1 {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::attachment_crypto::AttachmentCrypto;
    use std::io::Cursor;

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
}
