//! Cryptographic operations for attachments.

use memmap2::Mmap;

use crate::crypto::attachment_crypto::KEY_SET_LENGTH;
use crate::crypto::attachment_crypto_fast::decrypt_zero_copy;
use crate::crypto::secure_buffer::SecureBuffer;
use crate::crypto::streaming_decrypt::StreamingDecryptor;
use crate::domain::attachment::ContentLocation;
use crate::domain::ids::AttachmentId;

use super::{AttachmentError, AttachmentService, attachment_file_path};

impl AttachmentService {
    /// Decrypt attachment content to memory.
    ///
    /// Reads encrypted content from filesystem via mmap and decrypts it to a
    /// `SecureBuffer` that auto-zeroizes when dropped.
    ///
    /// **Security**: The decrypted plaintext exists ONLY in memory
    /// and is automatically wiped when the buffer goes out of scope.
    pub fn decrypt_to_memory(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<SecureBuffer, AttachmentError> {
        // Get attachment metadata
        let attachment = self
            .attachment_repo
            .get(attachment_id)
            .map_err(|e| AttachmentError::StorageError(e.to_string()))?
            .ok_or_else(|| {
                AttachmentError::InvalidAttachment(format!(
                    "Attachment {} not found",
                    attachment_id
                ))
            })?;

        // Must be on filesystem
        if attachment.content_location != ContentLocation::OnFilesystem {
            return Err(AttachmentError::InvalidAttachment(
                "Attachment content not on filesystem - download first".to_string(),
            ));
        }

        // Get encryption keys
        let encryption_key = attachment
            .encryption_key
            .as_ref()
            .ok_or_else(|| AttachmentError::DecryptionError("No encryption key".to_string()))?;
        let digest = attachment.encryption_iv.as_ref().ok_or_else(|| {
            AttachmentError::DecryptionError("No digest for verification".to_string())
        })?;

        if encryption_key.len() != KEY_SET_LENGTH {
            return Err(AttachmentError::DecryptionError(format!(
                "Invalid key length: expected {}, got {}",
                KEY_SET_LENGTH,
                encryption_key.len()
            )));
        }

        // Read from filesystem using mmap
        let file_path = attachment_file_path(attachment_id);
        tracing::debug!(
            file_path = %file_path.display(),
            "Reading ciphertext from filesystem via mmap"
        );

        let file = std::fs::File::open(&file_path).map_err(|e| {
            AttachmentError::FileError(format!(
                "Failed to open attachment file {}: {}",
                file_path.display(),
                e
            ))
        })?;

        // SAFETY: File is read-only and not modified while mapped
        let mmap = unsafe { Mmap::map(&file) }.map_err(|e| {
            AttachmentError::FileError(format!(
                "Failed to mmap attachment file {}: {}",
                file_path.display(),
                e
            ))
        })?;

        // Convert keys to fixed-size arrays
        let mut key = [0u8; KEY_SET_LENGTH];
        key.copy_from_slice(encryption_key);
        let mut digest_arr = [0u8; 32];
        if digest.len() >= 32 {
            digest_arr.copy_from_slice(&digest[..32]);
        } else {
            digest_arr[..digest.len()].copy_from_slice(digest);
        }

        tracing::debug!(
            ciphertext_len = mmap.len(),
            plaintext_size = attachment.metadata.size,
            "Decrypting attachment to memory"
        );

        // Convert to owned Vec for decrypt_zero_copy (which decrypts in-place)
        let ciphertext = mmap.to_vec();
        drop(mmap);

        let plaintext = decrypt_zero_copy(
            ciphertext,
            &key,
            &digest_arr,
            attachment.metadata.size as usize,
        )
        .map_err(|e| AttachmentError::DecryptionError(e.to_string()))?;

        tracing::info!(
            plaintext_len = plaintext.len(),
            "Decrypted attachment to secure buffer"
        );

        // Validate incoming media (MP4/WebP structure) before renderers parse it.
        // Buffer is wrapped first so the plaintext is zeroized on the error path.
        let buffer = SecureBuffer::new(plaintext);
        super::sanitize::validate_media(buffer.as_slice())?;

        Ok(buffer)
    }

    /// Decrypt attachment using true streaming I/O.
    ///
    /// Uses `StreamingDecryptor`, which reads in 512KB chunks. Two passes:
    /// SHA256 and HMAC-SHA256 over the whole file first, AES-CBC only once
    /// both have matched.
    ///
    /// ## Memory Usage
    ///
    /// For an 874MB attachment: ~512KB buffer (not 1.7GB like decrypt_zero_copy).
    /// The output plaintext still requires memory, but the ciphertext is never
    /// fully loaded — it streams from the file.
    ///
    pub fn decrypt_streaming(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<SecureBuffer, AttachmentError> {
        let start = std::time::Instant::now();
        tracing::info!("Starting streaming decryption");

        // Get attachment metadata
        let attachment = self
            .attachment_repo
            .get(attachment_id)
            .map_err(|e| AttachmentError::StorageError(e.to_string()))?
            .ok_or_else(|| {
                AttachmentError::InvalidAttachment(format!(
                    "Attachment {} not found",
                    attachment_id
                ))
            })?;

        // Must be on filesystem
        if attachment.content_location != ContentLocation::OnFilesystem {
            return Err(AttachmentError::InvalidAttachment(
                "Attachment content not on filesystem - download first".to_string(),
            ));
        }

        // Get encryption keys
        let encryption_key = attachment
            .encryption_key
            .as_ref()
            .ok_or_else(|| AttachmentError::DecryptionError("No encryption key".to_string()))?;
        let digest = attachment.encryption_iv.as_ref().ok_or_else(|| {
            AttachmentError::DecryptionError("No digest for verification".to_string())
        })?;

        if encryption_key.len() != KEY_SET_LENGTH {
            return Err(AttachmentError::DecryptionError(format!(
                "Invalid key length: expected {}, got {}",
                KEY_SET_LENGTH,
                encryption_key.len()
            )));
        }

        // Convert keys to fixed-size arrays
        let mut key = [0u8; KEY_SET_LENGTH];
        key.copy_from_slice(encryption_key);
        let mut digest_arr = [0u8; 32];
        if digest.len() >= 32 {
            digest_arr.copy_from_slice(&digest[..32]);
        } else {
            digest_arr[..digest.len()].copy_from_slice(digest);
        }

        // Open file for streaming (BufReader for efficient I/O)
        let file_path = attachment_file_path(attachment_id);
        let file_size = std::fs::metadata(&file_path)
            .map_err(|e| AttachmentError::FileError(format!("Failed to stat file: {}", e)))?
            .len() as usize;

        tracing::debug!(
            file_path = %file_path.display(),
            file_size = file_size,
            plaintext_size = attachment.metadata.size,
            "Opening file for streaming decryption"
        );

        let file = std::fs::File::open(&file_path).map_err(|e| {
            AttachmentError::FileError(format!(
                "Failed to open attachment file {}: {}",
                file_path.display(),
                e
            ))
        })?;
        let reader = std::io::BufReader::with_capacity(512 * 1024, file);

        // Use StreamingDecryptor — verify pass, then decrypt pass, 512KB chunks
        let mut decryptor = StreamingDecryptor::new(
            reader,
            &key,
            &digest_arr,
            attachment.metadata.size as usize,
            file_size,
        );

        let plaintext = decryptor
            .decrypt_all()
            .map_err(|e| AttachmentError::DecryptionError(e.to_string()))?;

        let total_elapsed = start.elapsed();
        let size_mb = plaintext.len() as f64 / (1024.0 * 1024.0);
        tracing::info!(
            plaintext_len = plaintext.len(),
            total_ms = total_elapsed.as_millis(),
            throughput_mbps = format_args!("{:.1}", size_mb / total_elapsed.as_secs_f64()),
            "Decrypted attachment via streaming"
        );

        // Validate incoming media (MP4/WebP structure) before renderers parse it.
        let buffer = SecureBuffer::new(plaintext);
        super::sanitize::validate_media(buffer.as_slice())?;

        Ok(buffer)
    }

    /// Open a random-access decryptor for streaming playback (e.g. video).
    ///
    /// Verifies the whole-file HMAC/digest up front (streaming, no plaintext
    /// materialized), then returns a [`CbcRangeDecryptor`] over an mmap of the
    /// ciphertext. The caller (media pipeline) pulls decrypted ranges on demand,
    /// so playback starts without decrypting the whole file into memory.
    ///
    /// **Security**: authentication happens here, before any range is served.
    pub fn open_range_decryptor(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<super::CbcRangeDecryptor, AttachmentError> {
        let attachment = self
            .attachment_repo
            .get(attachment_id)
            .map_err(|e| AttachmentError::StorageError(e.to_string()))?
            .ok_or_else(|| {
                AttachmentError::InvalidAttachment(format!(
                    "Attachment {} not found",
                    attachment_id
                ))
            })?;

        if attachment.content_location != ContentLocation::OnFilesystem {
            return Err(AttachmentError::InvalidAttachment(
                "Attachment content not on filesystem - download first".to_string(),
            ));
        }

        let encryption_key = attachment
            .encryption_key
            .as_ref()
            .ok_or_else(|| AttachmentError::DecryptionError("No encryption key".to_string()))?;
        let digest = attachment.encryption_iv.as_ref().ok_or_else(|| {
            AttachmentError::DecryptionError("No digest for verification".to_string())
        })?;
        if encryption_key.len() != KEY_SET_LENGTH {
            return Err(AttachmentError::DecryptionError(format!(
                "Invalid key length: expected {}, got {}",
                KEY_SET_LENGTH,
                encryption_key.len()
            )));
        }

        let mut key = [0u8; KEY_SET_LENGTH];
        key.copy_from_slice(encryption_key);
        let mut digest_arr = [0u8; 32];
        if digest.len() >= 32 {
            digest_arr.copy_from_slice(&digest[..32]);
        } else {
            digest_arr[..digest.len()].copy_from_slice(digest);
        }

        let plaintext_size = attachment.metadata.size as usize;
        let file_path = attachment_file_path(attachment_id);
        let file_size = std::fs::metadata(&file_path)
            .map_err(|e| AttachmentError::FileError(format!("Failed to stat file: {}", e)))?
            .len() as usize;

        // Auth pass (A1): HMAC + digest over the whole ciphertext. Streaming,
        // no AES, no plaintext buffer — cheap relative to a full decrypt.
        {
            let file = std::fs::File::open(&file_path).map_err(|e| {
                AttachmentError::FileError(format!(
                    "Failed to open attachment file {}: {}",
                    file_path.display(),
                    e
                ))
            })?;
            let reader = std::io::BufReader::with_capacity(512 * 1024, file);
            let mut verifier =
                StreamingDecryptor::new(reader, &key, &digest_arr, plaintext_size, file_size);
            verifier
                .verify()
                .map_err(|e| AttachmentError::DecryptionError(e.to_string()))?;
        }

        // mmap the (now authenticated) ciphertext for on-demand range reads.
        let file = std::fs::File::open(&file_path).map_err(|e| {
            AttachmentError::FileError(format!(
                "Failed to open attachment file {}: {}",
                file_path.display(),
                e
            ))
        })?;
        let mmap = unsafe { Mmap::map(&file) }
            .map_err(|e| AttachmentError::FileError(format!("Failed to mmap attachment: {}", e)))?;

        let mut aes_key = [0u8; 32];
        aes_key.copy_from_slice(&key[..32]);
        super::CbcRangeDecryptor::new(mmap, &aes_key, plaintext_size)
            .map_err(AttachmentError::DecryptionError)
    }

    /// Decrypt attachment using the best method for its size.
    ///
    /// - Small files (<1MB): `decrypt_to_memory`, whole ciphertext in memory
    /// - Large files (>=1MB): `decrypt_streaming`, ciphertext streamed from disk
    ///
    /// Both authenticate the ciphertext before decrypting any of it.
    pub fn decrypt_auto(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<SecureBuffer, AttachmentError> {
        // Check file size to pick strategy
        let file_path = attachment_file_path(attachment_id);
        let file_size = std::fs::metadata(&file_path).map(|m| m.len()).unwrap_or(0);

        const STREAMING_THRESHOLD: u64 = 1024 * 1024; // 1MB

        if file_size >= STREAMING_THRESHOLD {
            tracing::debug!(
                file_size = file_size,
                "Using streaming decryption (file >= 1MB)"
            );
            self.decrypt_streaming(attachment_id)
        } else {
            self.decrypt_to_memory(attachment_id)
        }
    }
}
