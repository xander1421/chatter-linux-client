//! Cryptographic operations for attachments.

use std::path::Path;

use crate::crypto::attachment_crypto::{AttachmentCrypto, KEY_SET_LENGTH};
use crate::crypto::secure_buffer::SecureBuffer;
use crate::domain::attachment::ContentLocation;
use crate::domain::ids::AttachmentId;

use super::{AttachmentError, AttachmentService};

impl AttachmentService {
    /// Decrypt attachment content to memory.
    ///
    /// Reads encrypted content from database and decrypts it to a
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

        // Check if content is in database
        if attachment.content_location != ContentLocation::InDatabase {
            return Err(AttachmentError::InvalidAttachment(
                "Attachment content not in database - download first".to_string(),
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

        // Get encrypted content from database
        let ciphertext = self
            .attachment_repo
            .get_encrypted_content(attachment_id)
            .map_err(|e| AttachmentError::StorageError(e.to_string()))?
            .ok_or_else(|| {
                AttachmentError::InvalidAttachment("Encrypted content not found in database".to_string())
            })?;

        tracing::debug!(
            attachment_id = %attachment_id,
            ciphertext_len = ciphertext.len(),
            plaintext_size = attachment.metadata.size,
            "Decrypting attachment to memory"
        );

        // Convert keys to fixed-size arrays
        let mut key = [0u8; KEY_SET_LENGTH];
        key.copy_from_slice(encryption_key);
        let mut digest_arr = [0u8; 32];
        if digest.len() >= 32 {
            digest_arr.copy_from_slice(&digest[..32]);
        } else {
            digest_arr[..digest.len()].copy_from_slice(digest);
        }

        // Decrypt to memory
        let plaintext = AttachmentCrypto::decrypt(
            &ciphertext,
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

        Ok(SecureBuffer::new(plaintext))
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
}
