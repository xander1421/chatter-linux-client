//! Streaming attachment decryption.
//!
//! Decrypts large attachments in chunks without loading the entire
//! ciphertext into memory. **Two passes**, verify then decrypt:
//!
//! 1. Stream the blob through SHA-256 and HMAC-SHA256 and check both tags
//! 2. Only then read it again through AES-CBC
//!
//! AES never runs on bytes that have not been authenticated. CBC decrypts
//! every untampered block correctly, so a single pass that verifies at the end
//! holds real plaintext for a blob it is about to reject.
//!
//! ## Memory Usage
//!
//! For a 200MB attachment: ~512KB buffer + plaintext output
//!
//! ## Performance
//!
//! The second pass re-reads a file the first pass just pulled into the page
//! cache.

use std::io::{Read, Seek, SeekFrom};

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecryptMut, KeyIvInit};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

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
}

impl<R: Read + Seek> Drop for StreamingDecryptor<R> {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.aes_key.zeroize();
        self.mac_key.zeroize();
    }
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
        }
    }

    /// Length of the AES-CBC body, once `total_size` has been checked.
    ///
    /// `total_size` is the size of a blob the storage server handed us. Too
    /// short and the offsets derived from it underflow; not a whole number of
    /// blocks and it cannot be raw CBC.
    fn encrypted_len(&self) -> Result<usize, CryptoError> {
        self.total_size
            .checked_sub(IV_LENGTH + HMAC_LENGTH)
            .filter(|len| *len >= AES_BLOCK && len % AES_BLOCK == 0)
            .ok_or_else(|| {
                CryptoError::AttachmentDecryptionFailed(
                    "ciphertext too short or not block-aligned".into(),
                )
            })
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

        let hmac_start = IV_LENGTH + self.encrypted_len()?;

        // Seek to beginning
        self.reader
            .seek(SeekFrom::Start(0))
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("seek: {}", e)))?;

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
    /// * `output` - Buffer to write decrypted data, at least one AES block.
    ///   Whole blocks are decrypted in place in it, so up to 15 bytes past the
    ///   returned length may be overwritten with padding.
    pub fn read_decrypted(&mut self, output: &mut [u8]) -> Result<usize, CryptoError> {
        let Some(decryptor) = self.decryptor.as_mut() else {
            return Err(CryptoError::AttachmentDecryptionFailed(
                "decryption not initialized".into(),
            ));
        };

        // Everything past plaintext_size is padding; nothing left to hand out.
        let plaintext_remaining = self.plaintext_size.saturating_sub(self.decryption_position);
        if plaintext_remaining == 0 {
            return Ok(0);
        }
        if output.len() < AES_BLOCK {
            return Err(CryptoError::AttachmentDecryptionFailed(
                "output buffer smaller than one AES block".into(),
            ));
        }

        // Whole blocks only. plaintext_size is clamped to the block-aligned
        // body, so rounding the remainder up never reads into the HMAC.
        let to_read = (output.len() / AES_BLOCK * AES_BLOCK)
            .min(CHUNK_SIZE)
            .min(plaintext_remaining.next_multiple_of(AES_BLOCK));
        self.reader
            .read_exact(&mut output[..to_read])
            .map_err(|e| CryptoError::AttachmentDecryptionFailed(format!("read: {}", e)))?;
        for blk in output[..to_read].chunks_exact_mut(AES_BLOCK) {
            decryptor.decrypt_block_mut(GenericArray::from_mut_slice(blk));
        }
        self.decryption_position += to_read;

        Ok(to_read.min(plaintext_remaining))
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
        Ok(0)
    }

    /// Verify the entire ciphertext, then decrypt it.
    ///
    /// Two passes over the blob: [`Self::verify`], then AES. Nothing is
    /// decrypted unless both the HMAC and the digest matched.
    pub fn decrypt_all(&mut self) -> Result<Vec<u8>, CryptoError> {
        self.verify()?;
        self.init_decryption()?;

        // Zeroizing: an I/O error part-way leaves authentic plaintext behind.
        let mut plaintext =
            Zeroizing::new(vec![0u8; self.plaintext_size.next_multiple_of(AES_BLOCK)]);
        let mut filled = 0;
        while filled < self.plaintext_size {
            let n = self.read_decrypted(&mut plaintext[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        plaintext.truncate(self.plaintext_size);

        Ok(std::mem::take(&mut *plaintext))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::attachment_crypto::AttachmentCrypto;
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

    /// Pins verify-then-decrypt: a blob that fails authentication must never
    /// reach the point where an AES decryptor exists for it.
    #[test]
    fn decrypt_all_authenticates_before_aes() {
        let plaintext: Vec<u8> = (0..100_000).map(|i| (i % 256) as u8).collect();
        let encrypted = AttachmentCrypto::encrypt(&plaintext).unwrap();

        let mut tampered = encrypted.ciphertext.clone();
        tampered[IV_LENGTH + 5] ^= 1;
        // Digest recomputed over the tampered bytes so the digest check passes
        // and the HMAC check is the one under test.
        let digest = AttachmentCrypto::compute_digest(&tampered);

        let total = tampered.len();
        let mut d = StreamingDecryptor::new(
            Cursor::new(tampered),
            &encrypted.key,
            &digest,
            encrypted.plaintext_size,
            total,
        );
        assert!(matches!(d.decrypt_all(), Err(CryptoError::AttachmentMacMismatch)));
        assert!(d.decryptor.is_none(), "AES initialised for unauthenticated ciphertext");
    }

    /// `total_size` is the size of whatever the storage server returned.
    #[test]
    fn hostile_total_size_is_an_error_not_a_panic() {
        let key = [0u8; KEY_SET_LENGTH];
        let digest = [0u8; 32];
        // Shorter than the HMAC, shorter than IV + HMAC + one block, and a
        // body that is not a whole number of blocks.
        for total_size in [0, 10, IV_LENGTH + HMAC_LENGTH, IV_LENGTH + HMAC_LENGTH + AES_BLOCK + 1] {
            let blob = vec![0u8; total_size];
            let mut d = StreamingDecryptor::new(Cursor::new(blob), &key, &digest, 1, total_size);
            assert!(matches!(d.verify(), Err(CryptoError::AttachmentDecryptionFailed(_))));
            assert!(matches!(d.decrypt_all(), Err(CryptoError::AttachmentDecryptionFailed(_))));
        }
    }

    /// Bucket padding is up to 5% of the file, so for anything large it spans
    /// more than the last chunk. The chunked reader must stop at plaintext_size.
    #[test]
    fn chunked_read_stops_at_plaintext_size() {
        let plaintext: Vec<u8> = (0..1_000_000).map(|i| (i % 251) as u8).collect();
        let encrypted = AttachmentCrypto::encrypt(&plaintext).unwrap();
        let total = encrypted.ciphertext.len();
        let mut d = StreamingDecryptor::new(
            Cursor::new(encrypted.ciphertext.clone()),
            &encrypted.key,
            &encrypted.digest,
            encrypted.plaintext_size,
            total,
        );
        d.verify().unwrap();
        d.init_decryption().unwrap();

        let mut result = Vec::new();
        let mut output = vec![0u8; 4096];
        loop {
            let n = d.read_decrypted(&mut output).unwrap();
            if n == 0 {
                break;
            }
            result.extend_from_slice(&output[..n]);
        }
        assert_eq!(result, plaintext);
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
}
