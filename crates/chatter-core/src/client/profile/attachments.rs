use super::*;

impl ChatterClient {
    /// Get an attachment record by id (local storage).
    pub fn get_attachment(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Option<crate::domain::attachment::Attachment> {
        self.attachments().get(attachment_id).ok().flatten()
    }

    pub fn get_attachment_metadata(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Option<crate::domain::attachment::Attachment> {
        self.attachments()
            .get_metadata(attachment_id)
            .ok()
            .flatten()
    }

    pub fn decrypt_attachment_auto(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Result<
        crate::crypto::secure_buffer::SecureBuffer,
        crate::services::attachment::AttachmentError,
    > {
        self.attachments().decrypt_auto(attachment_id)
    }

    /// Open a random-access decryptor for streaming playback (video/audio).
    ///
    /// Authenticates the whole file, then serves decrypted ranges on demand so
    /// the player starts without a full decrypt-to-memory. See
    /// `AttachmentService::open_range_decryptor`.
    pub fn open_attachment_range(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Result<
        crate::services::attachment::CbcRangeDecryptor,
        crate::services::attachment::AttachmentError,
    > {
        self.attachments().open_range_decryptor(attachment_id)
    }

    pub async fn download_attachment_to_filesystem(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Result<(), crate::services::attachment::AttachmentError> {
        self.attachments()
            .download_to_filesystem(attachment_id)
            .await
    }

    pub fn decrypt_and_decode_gif_first_frame(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Result<
        Option<crate::services::attachment::GifDecodeResult>,
        crate::services::attachment::AttachmentError,
    > {
        self.attachments()
            .decrypt_and_decode_gif_first_frame(attachment_id)
    }

    pub fn decrypt_and_get_image_info(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Result<
        crate::services::attachment::ImageDecodeResult,
        crate::services::attachment::AttachmentError,
    > {
        self.attachments().decrypt_and_get_image_info(attachment_id)
    }

    /// Prepare a downloaded still image entirely on the bounded worker.
    pub async fn prepare_downloaded_image_preview(
        self: &std::sync::Arc<Self>,
        id: crate::domain::ids::AttachmentId,
        session: crate::client::SessionGuard,
    ) -> Result<
        crate::services::attachment::ImagePreview,
        crate::services::attachment::AttachmentError,
    > {
        let client = self.clone();
        crate::services::attachment::run_attachment_work(move || {
            client.attachments().prepare_image_preview_guarded(
                &id,
                std::sync::Arc::new(move || session.is_current()),
            )
        })
        .await
        .map_err(|_| {
            crate::services::attachment::AttachmentError::InvalidAttachment(
                "image preview worker failed".into(),
            )
        })?
    }

    /// Viewport hydration uses exactly the same bounded preparation as inbox
    /// delivery. A durable thumbnail prevents repeat original decryption.
    pub async fn prepare_visible_attachment_preview(
        self: &std::sync::Arc<Self>,
        id: crate::domain::ids::AttachmentId,
        session: crate::client::SessionGuard,
    ) -> Option<crate::services::attachment::ImagePreview> {
        let client = self.clone();
        let lookup_session = session.clone();
        let derive_local = crate::services::attachment::run_attachment_work(move || {
            if !lookup_session.is_current() {
                return None;
            }
            let attachment = client.get_attachment_metadata(&id)?;
            Some(attachment.metadata.is_image() && attachment.has_local_content())
        })
        .await
        .ok()
        .flatten()?;
        if derive_local {
            self.prepare_downloaded_image_preview(id, session)
                .await
                .ok()
        } else {
            self.ensure_attachment_preview(id, session).await
        }
    }

    /// Fetch/authenticate the sidecar, then decode only small model pixels.
    pub async fn ensure_attachment_preview(
        self: &std::sync::Arc<Self>,
        id: crate::domain::ids::AttachmentId,
        session: crate::client::SessionGuard,
    ) -> Option<crate::services::attachment::ImagePreview> {
        let fetch_session = session.clone();
        let bytes = self
            .attachments()
            .ensure_thumbnail_guarded(&id, std::sync::Arc::new(move || fetch_session.is_current()))
            .await?;
        let client = self.clone();
        crate::services::attachment::run_attachment_work(move || {
            if !session.is_current() {
                return None;
            }
            let pixels =
                crate::services::attachment::AttachmentService::decode_image_sidecar(&bytes)
                    .ok()?;
            let attachment = client.get_attachment_metadata(&id)?;
            if !session.is_current() {
                return None;
            }
            Some(crate::services::attachment::ImagePreview {
                width: attachment
                    .metadata
                    .width
                    .filter(|n| *n > 0)
                    .unwrap_or(pixels.width()),
                height: attachment
                    .metadata
                    .height
                    .filter(|n| *n > 0)
                    .unwrap_or(pixels.height()),
                pixels,
            })
        })
        .await
        .ok()
        .flatten()
    }

    /// Decrypt a video attachment and extract a thumbnail frame.
    ///
    /// Blocking platform decode — call from a background thread. Used to derive a
    /// thumbnail for server-DM videos, which arrive without inline thumbnails.
    pub fn decrypt_and_extract_video_thumbnail(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Result<
        crate::services::attachment::ImageDecodeResult,
        crate::services::attachment::AttachmentError,
    > {
        let revision = self.auth_state().get().session_revision;
        let auth = self.auth_state().clone();
        let result = self
            .attachments()
            .decrypt_and_extract_video_thumbnail_guarded(
                attachment_id,
                std::sync::Arc::new(move || auth.get().session_revision == revision),
            );
        if self.auth_state().get().session_revision != revision {
            return Err(
                crate::services::attachment::AttachmentError::InvalidAttachment(
                    "video preview owner changed".into(),
                ),
            );
        }
        result
    }

    /// Decode every frame of a GIF from already-decrypted bytes.
    ///
    /// Blocking — call from a background thread (e.g. `spawn_blocking`).
    /// Thin façade over [`AttachmentService::decode_gif_all_frames`] so the UI
    /// never has to reach into the service directly.
    pub fn decode_gif_all_frames(
        &self,
        gif_bytes: &[u8],
    ) -> Result<
        crate::services::attachment::GifFullDecodeResult,
        crate::services::attachment::AttachmentError,
    > {
        crate::services::attachment::AttachmentService::decode_gif_all_frames(gif_bytes)
    }

    pub fn update_attachment(
        &self,
        attachment: &crate::domain::attachment::Attachment,
    ) -> Result<(), crate::services::attachment::AttachmentError> {
        self.attachments().update(attachment)
    }

    pub fn get_attachment_preview_image(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Option<(Vec<u8>, u32, u32)> {
        self.attachments().get_preview_image(attachment_id)
    }

    /// Fetch + decrypt the thumbnail sidecar for a received attachment, caching
    /// the bytes locally. Gives a preview without downloading the full blob.
    pub async fn ensure_attachment_thumbnail(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Option<Vec<u8>> {
        self.attachments().ensure_thumbnail(attachment_id).await
    }

    /// Encrypt/upload a recorded voice note, preserving voice metadata on every
    /// platform; the caller retains the recording for retry until sent/cancelled.
    pub async fn upload_voice_note(
        &self,
        note: &crate::services::voice_recording::RecordedVoiceNote,
    ) -> Result<crate::state::chat_state::PendingAttachment, String> {
        use crate::services::voice_recording::recording_context_is_current;
        if !recording_context_is_current(self.chat_state(), note.conversation) {
            return Err("Conversation changed; recording discarded".into());
        }
        // Routed for the recording's conversation: Tor-only for a secret chat.
        let started = std::time::Instant::now();
        let upload = self
            .attachments()
            .for_conversation(&note.conversation)
            .upload_recorded_voice_note(note.file.path(), &note.waveform, note.duration_ms)
            .await
            .map_err(|e| e.to_string())?;
        if note.conversation.is_anon() {
            tracing::info!(
                elapsed_ms = started.elapsed().as_millis(),
                "secret voice: encrypted upload complete"
            );
        }
        if !recording_context_is_current(self.chat_state(), note.conversation) {
            return Err("Conversation changed; recording discarded".into());
        }
        let mut pointer = upload.to_attachment_pointer();
        pointer.voice_note = true;
        pointer.waveform = note.waveform.clone();
        pointer.duration_ms = note.duration_ms;
        pointer.file_name = crate::domain::attachment::GENERIC_VOICE_NOTE_FILENAME.into();
        // The sender renders from the already-uploaded local attachment row;
        // receivers render from the pointer. Keep the same metadata on both.
        let mut attachment = self
            .attachments()
            .get(&upload.attachment_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "Uploaded voice attachment is missing from local storage".to_string())?;
        attachment.metadata.name = pointer.file_name.clone();
        attachment.metadata.waveform_samples = Some(pointer.waveform.clone());
        attachment.metadata.duration_seconds = Some(pointer.duration_ms as f64 / 1000.0);
        self.attachments()
            .update(&attachment)
            .map_err(|e| e.to_string())?;

        if note.conversation.is_anon() {
            tracing::info!(
                elapsed_ms = started.elapsed().as_millis(),
                "secret voice: pending attachment ready"
            );
        }
        Ok(crate::state::chat_state::PendingAttachment::with_pointer(
            upload.attachment_id,
            std::path::PathBuf::new(),
            pointer.file_name.clone(),
            upload.plaintext_size,
            if cfg!(target_os = "android") {
                "audio/mp4"
            } else {
                "audio/wav"
            }
            .into(),
            pointer,
        ))
    }

    /// Fetch GIF picker media through the server's GIF media proxy.
    pub async fn fetch_gif_media(
        &self,
        url: &str,
    ) -> Result<Vec<u8>, crate::services::attachment::AttachmentError> {
        if !self.gif_picker_available() {
            return Err(
                crate::services::attachment::AttachmentError::InvalidAttachment(
                    "GIFs are not available in a secret chat".into(),
                ),
            );
        }
        self.attachments().fetch_gif_media(url).await
    }

    pub async fn download_and_upload_gif(
        &self,
        url: &str,
        gif_id: &str,
    ) -> Result<
        crate::services::attachment::CdnUploadResult,
        crate::services::attachment::AttachmentError,
    > {
        // The GIF is sent into the open conversation; a secret chat's upload
        // goes over Tor or not at all.
        self.attachments()
            .for_conversation(&self.chat_state().active_conversation())
            .download_and_upload_gif(url, gif_id)
            .await
    }
}
