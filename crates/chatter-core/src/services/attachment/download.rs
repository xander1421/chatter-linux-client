//! Download operations for attachments.

use crate::domain::ids::AttachmentId;
use tokio::io::AsyncWriteExt;

use super::{AttachmentError, AttachmentService, attachment_file_path, ensure_attachments_dir};

impl AttachmentService {
    /// Download encrypted content from CDN and store on filesystem.
    ///
    /// Downloads the ciphertext and stores it on the filesystem
    /// without writing plaintext to disk. Missing or degenerate waveforms are repaired
    /// in memory only for small voice notes, using the actual ciphertext size.
    ///
    /// This is the secure flow: encrypted content stored on filesystem,
    /// decrypted only to memory for metadata/playback, then zeroized.
    ///
    /// ## Storage Location
    ///
    /// Files are stored at: `~/.local/share/chatter/attachments/{attachment_id}.enc`
    ///
    /// An attachment carried by a secret-chat message is fetched over Tor only.
    pub async fn download_to_filesystem(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<(), AttachmentError> {
        self.for_download(attachment_id)?
            .download_to_filesystem_routed(attachment_id)
            .await
    }

    async fn download_to_filesystem_routed(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<(), AttachmentError> {
        // Get attachment with CDN key
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

        let cdn_key = attachment.remote_url.as_ref().ok_or_else(|| {
            AttachmentError::InvalidAttachment("Attachment has no CDN key".to_string())
        })?;

        tracing::debug!(
            attachment_id = %attachment_id,
            "Downloading encrypted content to filesystem"
        );

        // Get presigned download URL
        let mut file_client = self.file_client()?;
        let download_response = file_client
            .get_attachment_download_url(cdn_key)
            .await
            .map_err(|e| {
                AttachmentError::NetworkError(format!("Failed to get download URL: {}", e))
            })?;

        // Ensure attachments directory exists
        ensure_attachments_dir().map_err(|e| {
            AttachmentError::FileError(format!("Failed to create attachments dir: {}", e))
        })?;

        // A private temporary ciphertext file is removed on error/cancellation.
        // Publish only a complete, flushed download; never truncate a previous
        // cached object and never keep a multi-GiB response in memory.
        let file_path = attachment_file_path(attachment_id);
        let staging =
            tempfile::NamedTempFile::new_in(file_path.parent().ok_or_else(|| {
                AttachmentError::FileError("attachment directory missing".into())
            })?)
            .map_err(|_| {
                AttachmentError::FileError("cannot create encrypted staging file".into())
            })?;
        let mut file = tokio::fs::File::from_std(staging.reopen().map_err(|_| {
            AttachmentError::FileError("cannot open encrypted staging file".into())
        })?);
        let ciphertext_size = tokio::time::timeout(
            std::time::Duration::from_secs(20 * 60),
            self.cdn_http_get_to_file(&download_response.download_url, &mut file),
        )
        .await
        .map_err(|_| AttachmentError::NetworkError("storage download timed out".into()))??;
        // Tokio File may buffer the final write; flush must surface its I/O
        // error before sync_all (which only waits for the in-flight task).
        file.flush().await.map_err(|_| {
            AttachmentError::FileError("cannot write encrypted staging file".into())
        })?;
        file.sync_all().await.map_err(|_| {
            AttachmentError::FileError("cannot flush encrypted staging file".into())
        })?;
        drop(file);
        staging.persist(&file_path).map_err(|_| {
            AttachmentError::FileError("cannot publish encrypted attachment".into())
        })?;

        // Store file path in database (NOT the content)
        // This atomically sets status=Ready and content_location=OnFilesystem
        self.attachment_repo
            .store_file_path(attachment_id, &file_path)
            .map_err(|e| AttachmentError::StorageError(e.to_string()))?;

        if attachment.metadata.is_audio()
            // This is optional voice-note presentation, never an excuse to
            // allocate a large/incorrectly advertised audio object in RAM.
            && ciphertext_size <= 8 * 1024 * 1024
            && crate::media::waveform::needs_waveform_repair(
                attachment.metadata.waveform_samples.as_deref(),
            )
        {
            // inline, voice notes are small (seconds of audio); move
            // to spawn_blocking with an owned service if long audio shows up here.
            let result = self
                .decrypt_to_memory(attachment_id)
                .and_then(|audio| self.repair_waveform(attachment_id, audio.as_slice()));
            if let Err(error) = result {
                tracing::warn!(%attachment_id, %error, "Could not recover audio waveform");
            }
        }

        tracing::info!(
            attachment_id = %attachment_id,
            ciphertext_size = ciphertext_size,
            file_path = %file_path.display(),
            "Encrypted content stored on filesystem"
        );

        Ok(())
    }

    /// Repair missing or degenerate waveform metadata, preserving the latest transfer state.
    fn repair_waveform(&self, id: &AttachmentId, audio: &[u8]) -> Result<(), AttachmentError> {
        let waveform = crate::media::extract_waveform_from_memory(audio)
            .map_err(|e| AttachmentError::InvalidAttachment(e.to_string()))?;
        let mut attachment = self.get(id)?.ok_or_else(|| {
            AttachmentError::InvalidAttachment(format!("Attachment {} not found", id))
        })?;
        if crate::media::waveform::needs_waveform_repair(
            attachment.metadata.waveform_samples.as_deref(),
        ) {
            attachment.metadata.waveform_samples = Some(waveform);
            self.update(&attachment)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn recovered_waveform_is_persisted_without_overwriting_wire_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            crate::storage::database::Database::open(
                &dir.path().join("waveform-repair.db"),
                "testpass",
            )
            .unwrap(),
        );
        db.run_migrations().unwrap();
        let service = AttachmentService::new_local(Arc::new(
            crate::storage::repositories::AttachmentRepository::new(db),
        ));
        let ids = service.create_from_pointers(&[crate::proto::content_v1::AttachmentPointer {
            cdn_key: "missing-waveform".into(),
            content_type: "audio/wav".into(),
            ..Default::default()
        }]);
        let mut audio = std::io::Cursor::new(Vec::new());
        {
            let mut writer = hound::WavWriter::new(
                &mut audio,
                hound::WavSpec {
                    channels: 1,
                    sample_rate: 8000,
                    bits_per_sample: 16,
                    sample_format: hound::SampleFormat::Int,
                },
            )
            .unwrap();
            for sample in 0..64_i16 {
                writer.write_sample(sample).unwrap();
            }
            writer.finalize().unwrap();
        }
        service.repair_waveform(&ids[0], audio.get_ref()).unwrap();
        let mut stored = service.get(&ids[0]).unwrap().unwrap();
        let bars = stored.metadata.waveform_samples.as_ref().unwrap();
        assert_eq!(bars.len(), 32);
        assert_eq!(bars[31], 255);
        assert!(bars[0] < bars[16]);
        assert_eq!(
            stored.content_location,
            crate::domain::attachment::ContentLocation::NotDownloaded
        );
        for old in [vec![0; 32], vec![80; 32], vec![0, 1, 3, 13]] {
            stored.metadata.waveform_samples = Some(old);
            service.update(&stored).unwrap();
            service.repair_waveform(&ids[0], audio.get_ref()).unwrap();
            let repaired = service.get(&ids[0]).unwrap().unwrap();
            let bars = repaired.metadata.waveform_samples.unwrap();
            assert_eq!(bars.len(), 32);
            assert_eq!(bars[31], 255);
            assert!(bars[0] < bars[16]);
        }
        stored.metadata.waveform_samples = Some(vec![13, 255]);
        service.update(&stored).unwrap();
        service.repair_waveform(&ids[0], audio.get_ref()).unwrap();
        assert_eq!(
            service
                .get(&ids[0])
                .unwrap()
                .unwrap()
                .metadata
                .waveform_samples,
            Some(vec![13, 255])
        );
    }
}
