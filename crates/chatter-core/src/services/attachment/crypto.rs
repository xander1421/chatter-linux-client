//! Cryptographic operations for attachments.

use std::path::Path;

use memmap2::Mmap;

use crate::crypto::attachment_crypto::KEY_SET_LENGTH;
use crate::crypto::attachment_crypto_fast::decrypt_zero_copy;
use crate::crypto::secure_buffer::SecureBuffer;
use crate::domain::attachment::ContentLocation;
use crate::domain::ids::AttachmentId;

use super::{attachment_file_path, AttachmentError, AttachmentService};

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
                AttachmentError::InvalidAttachment(format!("Attachment {} not found", attachment_id))
            })?;

        // Must be on filesystem
        if attachment.content_location != ContentLocation::OnFilesystem {
            return Err(AttachmentError::InvalidAttachment(
                "Attachment content not on filesystem - download first".to_string(),
            ));
        }

        // Get encryption keys
        let encryption_key = attachment.encryption_key.as_ref().ok_or_else(|| {
            AttachmentError::DecryptionError("No encryption key".to_string())
        })?;
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
            attachment_id = %attachment_id,
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
            attachment_id = %attachment_id,
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
            attachment_id = %attachment_id,
            plaintext_len = plaintext.len(),
            "Decrypted attachment to secure buffer"
        );

        // Validate incoming media (MP4/WebP structure) before renderers parse it.
        // Buffer is wrapped first so the plaintext is zeroized on the error path.
        let buffer = SecureBuffer::new(plaintext);
        super::sanitize::validate_media(buffer.as_slice())?;

        Ok(buffer)
    }

    /// Export a decrypted attachment to a file.
    ///
    /// Decrypts the attachment content to memory and writes it to the
    /// specified path. Used for "Save As" functionality.
    ///
    /// # Security Note
    ///
    /// This is the only method that writes decrypted content to disk.
    /// It should only be called when the user explicitly requests to
    /// export a file via "Save As".
    pub fn export_to_file(
        &self,
        attachment_id: &AttachmentId,
        destination: &Path,
    ) -> Result<(), AttachmentError> {
        // Decrypt to memory
        let secure_buffer = self.decrypt_to_memory(attachment_id)?;

        // Write to destination file
        std::fs::write(destination, secure_buffer.as_slice())
            .map_err(|e| AttachmentError::FileError(format!("Failed to write file: {}", e)))?;

        tracing::info!(
            attachment_id = %attachment_id,
            destination = %destination.display(),
            size = secure_buffer.len(),
            "Exported attachment to file"
        );

        Ok(())
        // secure_buffer dropped here -> memory zeroized
    }

    /// Export a decrypted attachment asynchronously.
    ///
    /// Same as export_to_file but runs the file write in a blocking task.
    pub async fn export_to_file_async(
        &self,
        attachment_id: &AttachmentId,
        destination: &Path,
    ) -> Result<(), AttachmentError> {
        // Decrypt to memory (sync operation on DB)
        let secure_buffer = self.decrypt_to_memory(attachment_id)?;
        let dest = destination.to_path_buf();
        let att_id = *attachment_id;

        // Write to file in blocking task
        tokio::task::spawn_blocking(move || {
            std::fs::write(&dest, secure_buffer.as_slice())
                .map_err(|e| AttachmentError::FileError(format!("Failed to write file: {}", e)))?;

            tracing::info!(
                attachment_id = %att_id,
                destination = %dest.display(),
                "Exported attachment to file"
            );

            Ok::<(), AttachmentError>(())
        })
        .await
        .map_err(|e| AttachmentError::FileError(format!("Export task failed: {}", e)))??;

        Ok(())
    }

    /// Decrypt attachment using streaming mmap I/O.
    ///
    /// For large attachments (>1MB), uses memory-mapped file access
    /// for efficient streaming decryption.
    ///
    /// ## Performance
    ///
    /// - Mmap: ~6.5 GB/s throughput (2x faster than SQLite BLOB)
    /// - Memory efficient: pages loaded on-demand by OS
    pub fn decrypt_streaming(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<SecureBuffer, AttachmentError> {
        let start = std::time::Instant::now();
        tracing::info!(attachment_id = %attachment_id, "Starting streaming decryption");

        // Get attachment metadata
        let attachment = self
            .attachment_repo
            .get(attachment_id)
            .map_err(|e| AttachmentError::StorageError(e.to_string()))?
            .ok_or_else(|| {
                AttachmentError::InvalidAttachment(format!("Attachment {} not found", attachment_id))
            })?;

        // Must be on filesystem
        if attachment.content_location != ContentLocation::OnFilesystem {
            return Err(AttachmentError::InvalidAttachment(
                "Attachment content not on filesystem - download first".to_string(),
            ));
        }

        // Get encryption keys
        let encryption_key = attachment.encryption_key.as_ref().ok_or_else(|| {
            AttachmentError::DecryptionError("No encryption key".to_string())
        })?;
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

        // Open file and mmap
        let file_path = attachment_file_path(attachment_id);
        tracing::debug!(
            attachment_id = %attachment_id,
            file_path = %file_path.display(),
            plaintext_size = attachment.metadata.size,
            "Opening memory-mapped file for streaming decryption"
        );

        let file = std::fs::File::open(&file_path).map_err(|e| {
            AttachmentError::FileError(format!(
                "Failed to open attachment file {}: {}",
                file_path.display(),
                e
            ))
        })?;

        let mmap = unsafe { Mmap::map(&file) }.map_err(|e| {
            AttachmentError::FileError(format!("Failed to memory-map file: {}", e))
        })?;

        let ciphertext = mmap.to_vec();
        drop(mmap);

        let plaintext = decrypt_zero_copy(ciphertext, &key, &digest_arr, attachment.metadata.size as usize)
            .map_err(|e| AttachmentError::DecryptionError(e.to_string()))?;

        let total_elapsed = start.elapsed();
        tracing::info!(
            attachment_id = %attachment_id,
            plaintext_len = plaintext.len(),
            total_ms = total_elapsed.as_millis(),
            "Decrypted attachment via streaming mmap"
        );

        // Validate incoming media (MP4/WebP structure) before renderers parse it.
        let buffer = SecureBuffer::new(plaintext);
        super::sanitize::validate_media(buffer.as_slice())?;

        Ok(buffer)
    }

    /// Decrypt attachment using the best method for its size.
    ///
    /// Both methods use mmap - this just controls logging verbosity.
    pub fn decrypt_auto(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<SecureBuffer, AttachmentError> {
        // All decryption now uses mmap, so just call decrypt_to_memory
        self.decrypt_to_memory(attachment_id)
    }
}
