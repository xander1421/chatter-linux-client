//! Download operations for attachments.

use crate::domain::ids::AttachmentId;

use super::{attachment_file_path, ensure_attachments_dir, AttachmentError, AttachmentService};

impl AttachmentService {
    /// Download encrypted content from CDN and store on filesystem.
    ///
    /// Downloads the ciphertext and stores it on the filesystem
    /// WITHOUT decrypting. Decryption happens only during playback
    /// via `decrypt_to_memory()`.
    ///
    /// This is the secure flow: encrypted content stored on filesystem,
    /// decrypted only to memory during playback, then zeroized.
    ///
    /// ## Storage Location
    ///
    /// Files are stored at: `~/.local/share/chatter/attachments/{attachment_id}.enc`
    pub async fn download_to_filesystem(
        &self,
                attachment_id: &AttachmentId,
    ) -> Result<(), AttachmentError> {
        // Get attachment with CDN key
        let attachment = self
            .attachment_repo
            .get(attachment_id)
            .map_err(|e| AttachmentError::StorageError(e.to_string()))?
            .ok_or_else(|| {
                AttachmentError::InvalidAttachment(format!("Attachment {} not found", attachment_id))
            })?;

        let cdn_key = attachment.remote_url.as_ref().ok_or_else(|| {
            AttachmentError::InvalidAttachment("Attachment has no CDN key".to_string())
        })?;

        tracing::debug!(
            attachment_id = %attachment_id,
            cdn_key = %cdn_key,
            "Downloading encrypted content to filesystem"
        );

        // Get presigned download URL
        let mut file_client = self.grpc_pool()?.file_client();
        let download_response = file_client
            .get_attachment_download_url(cdn_key)
            .await
            .map_err(|e| AttachmentError::NetworkError(format!("Failed to get download URL: {}", e)))?;

        // Download encrypted ciphertext via HTTP GET (over Tor in anon mode).
        let ciphertext = self.cdn_http_get(&download_response.download_url).await?;

        let ciphertext_size = ciphertext.len();

        tracing::debug!(
            attachment_id = %attachment_id,
            ciphertext_size = ciphertext_size,
            "Downloaded ciphertext, storing on filesystem"
        );

        // Ensure attachments directory exists
        ensure_attachments_dir()
            .map_err(|e| AttachmentError::FileError(format!("Failed to create attachments dir: {}", e)))?;

        // Write encrypted content to filesystem
        let file_path = attachment_file_path(attachment_id);
        tokio::fs::write(&file_path, &ciphertext)
            .await
            .map_err(|e| AttachmentError::FileError(format!("Failed to write attachment file: {}", e)))?;

        // Store file path in database (NOT the content)
        // This atomically sets status=Ready and content_location=OnFilesystem
        self.attachment_repo
            .store_file_path(attachment_id, &file_path)
            .map_err(|e| AttachmentError::StorageError(e.to_string()))?;

        tracing::info!(
            attachment_id = %attachment_id,
            ciphertext_size = ciphertext_size,
            file_path = %file_path.display(),
            "Encrypted content stored on filesystem"
        );

        Ok(())
    }
}
