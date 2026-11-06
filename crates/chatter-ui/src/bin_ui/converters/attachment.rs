//! Attachment converters.

use slint::{Image, ModelRc, VecModel};
use std::sync::Arc;

use chatter_core::domain::attachment::{Attachment, AttachmentStatus};
use chatter_core::state::PendingAttachment;

use crate::bin_ui::audio_state;
use crate::{MessageAttachmentData, PendingAttachmentData};

use super::utils::{category_to_icon_and_color, format_file_size, mime_to_category};

/// Give the media subset stable positions without duplicating its thumbnail model.
pub fn assign_album_positions(attachments: &mut [MessageAttachmentData]) {
    let (positions, count) = chatter_core::media::album::album_positions(
        attachments.iter().map(|a| (a.is_image, a.is_video)),
    );
    for (attachment, index) in attachments.iter_mut().zip(positions) {
        attachment.album_count = count;
        attachment.album_index = index;
    }
}

/// Indices of attachments that need their own row outside the album grid.
pub fn standalone_attachment_indices(attachments: &[MessageAttachmentData]) -> ModelRc<i32> {
    let indices = chatter_core::media::album::standalone_indices(
        &attachments
            .iter()
            .map(|a| a.album_index)
            .collect::<Vec<_>>(),
        attachments.first().map_or(0, |a| a.album_count),
    );
    ModelRc::new(VecModel::from(indices))
}

/// Decode only media that the inline bubble actually paints.
pub fn decode_inline_thumbnails(attachments: &mut [AttachmentDisplayData]) {
    decode_inline_thumbnails_guarded(
        attachments,
        crate::bin_ui::decode_cache::thumbnail_generation(),
    );
}

pub fn decode_inline_thumbnails_guarded(
    attachments: &mut [AttachmentDisplayData],
    generation: u64,
) {
    let media_count = attachments
        .iter()
        .filter(|a| a.is_image || a.is_video)
        .count();
    let mut media_index = 0;
    for attachment in attachments {
        if attachment.is_image || attachment.is_video {
            if media_count < 2 || media_index < 10 {
                attachment.decode_thumbnail_guarded(generation);
            }
            media_index += 1;
        }
    }
}

/// Data needed to create attachment UI (can be sent across threads).
#[derive(Clone, Debug)]
pub struct AttachmentDisplayData {
    pub id: String,
    pub filename: String,
    pub size_display: String,
    pub mime_type: String,
    pub category: String,
    pub thumbnail_url: String,
    pub download_url: String,
    pub local_path: Option<String>,
    pub is_image: bool,
    pub is_video: bool,
    pub width: i32,
    pub height: i32,
    pub is_playing: bool,
    pub icon: String,
    pub icon_color: slint::Color,
    /// Audio waveform samples (32 values, 0-100) for visualization.
    pub waveform: Vec<i32>,
    /// Formatted duration string (e.g., "1:23").
    pub duration_display: String,
    /// Transfer progress fields
    pub is_uploading: bool,
    pub is_downloading: bool,
    pub is_download_failed: bool,
    pub progress: f32,
    /// Per-file outgoing send state ("" / "queued" / "uploading" / "failed" /
    /// "cancelled"), rendered on the composer's in-flight bubble.
    pub upload_state: String,
    /// Thumbnail JPEG bytes from sender (for instant preview without download).
    pub thumbnail_bytes: Arc<[u8]>,
    /// Decoded thumbnail, from `decode_cache` — decode before the UI thread.
    /// Shared with the cache, so the UI thread wraps it without copying.
    pub thumbnail_pixels: Option<crate::bin_ui::decode_cache::Pixels>,
}

impl AttachmentDisplayData {
    /// Derive only from the client's authenticated encrypted attachment, off the UI thread.
    pub fn derive_video_thumbnail(&mut self, client: &chatter_core::client::ChatterClient) {
        #[cfg(target_os = "android")]
        if self.is_video && self.thumbnail_bytes.is_empty() {
            if let Ok(id) = self.id.parse() {
                if client
                    .get_attachment_metadata(&id)
                    .is_some_and(|att| att.needs_download())
                {
                    return;
                }
                match client.decrypt_and_extract_video_thumbnail(&id) {
                    Ok(result) => {
                        self.thumbnail_bytes = result.image_bytes.into();
                        self.width = result.width as i32;
                        self.height = result.height as i32;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Video preview extraction failed; keeping play placeholder")
                    }
                }
            }
        }
        #[cfg(not(target_os = "android"))]
        let _ = client;
    }

    /// Pre-decode thumbnail bytes to RGBA pixels (call this before invoke_from_event_loop).
    pub fn decode_thumbnail(&mut self) {
        self.decode_thumbnail_guarded(crate::bin_ui::decode_cache::thumbnail_generation());
    }

    pub fn decode_thumbnail_guarded(&mut self, generation: u64) {
        if generation != crate::bin_ui::decode_cache::thumbnail_generation() {
            return;
        }
        if self.thumbnail_pixels.is_some() {
            return; // Already decoded
        }
        // Audio / generic file attachments never have thumbnail bitmaps —
        // skip the decode to avoid `image` noisily failing on a FLAC/OGG/etc.
        // header. Videos still go through here: the video thumbnail is
        // sender-provided as a JPEG/PNG in `thumbnail_bytes`.
        if !self.is_image && !self.is_video {
            return;
        }
        if !self.thumbnail_bytes.is_empty() {
            let bytes = &self.thumbnail_bytes;
            self.thumbnail_pixels = crate::bin_ui::decode_cache::thumbnail_guarded(
                &self.id,
                bytes.len(),
                generation,
                || {
                    chatter_core::media::ImageProcessor::inline_pixels_from_memory(bytes)
                        .ok()
                        .map(image::DynamicImage::ImageRgba8)
                },
            );
            if self.thumbnail_pixels.is_some() {
                return;
            }
        }
        // No decode from `local_path`: a stored attachment file is ciphertext, and
        // `image` guessing a format from encrypted bytes "succeeds" into garbage —
        // the grey/noise album tiles. The only valid inline preview is the sender's
        // decrypted `thumbnail_bytes` above; without it the attachment shows its
        // placeholder and decrypts on open. (Populating album thumbnails from the
        // encrypted sidecar is a separate download step — see OPEN-ITEMS.)
    }

    /// Convert to Slint MessageAttachmentData (must be called on UI thread).
    pub fn to_slint(&self) -> MessageAttachmentData {
        self.to_slint_with_media(true)
    }

    /// A placeholder with the final image aspect ratio for the first paint.
    pub fn to_slint_with_media(&self, media_ready: bool) -> MessageAttachmentData {
        // Priority 0: Use pre-decoded pixels (fastest - no decoding on UI thread)
        // Priority 1: Try to load thumbnail from sender-provided bytes (instant preview)
        // Priority 2: Load from local path after download
        let (has_thumbnail, thumbnail, img_width, img_height) = if !media_ready {
            (false, Image::default(), self.width, self.height)
        } else if let Some(pixels) = &self.thumbnail_pixels {
            // Pre-decoded and shared: wrapping it is a reference count, no copy.
            let (w, h) = (pixels.width(), pixels.height());
            (true, Image::from_rgba8(pixels.clone()), w as i32, h as i32)
        } else {
            // Missing/failed or intentionally deferred decodes keep placeholders.
            // Every bitmap must be prepared before event-loop publication.
            (false, Image::default(), self.width, self.height)
        };

        // Convert waveform to Slint model
        let waveform_model: ModelRc<i32> = ModelRc::new(VecModel::from(self.waveform.clone()));

        MessageAttachmentData {
            id: self.id.clone().into(),
            filename: self.filename.clone().into(),
            size_display: self.size_display.clone().into(),
            mime_type: self.mime_type.clone().into(),
            category: self.category.clone().into(),
            thumbnail_url: self.thumbnail_url.clone().into(),
            download_url: self.download_url.clone().into(),
            is_image: self.is_image,
            is_video: self.is_video,
            album_index: -1,
            album_count: 0,
            width: img_width,
            height: img_height,
            has_thumbnail,
            thumbnail,
            is_playing: self.is_playing,
            icon: self.icon.clone().into(),
            icon_color: self.icon_color,
            waveform: waveform_model,
            duration_display: self.duration_display.clone().into(),
            is_uploading: self.is_uploading,
            is_downloading: self.is_downloading,
            is_download_failed: self.is_download_failed,
            progress: self.progress,
            upload_state: self.upload_state.clone().into(),
        }
    }
}

/// Convert stored/wire amplitudes to the UI's percentage bar scale.
pub(crate) fn waveform_bar(sample: i32) -> i32 {
    (sample.clamp(0, 255) * 100 + 127) / 255
}

/// Convert a domain Attachment to thread-safe AttachmentDisplayData.
pub fn attachment_to_display_data(attachment: &Attachment) -> AttachmentDisplayData {
    let meta = &attachment.metadata;
    let category = mime_to_category(&meta.mime_type);
    let id_str = attachment.id.to_string();

    // Check if this attachment is currently playing audio
    let is_playing = meta.is_audio() && audio_state::is_attachment_playing(&id_str);

    // Get icon and color for category
    let (icon, icon_color) = category_to_icon_and_color(category);

    // Normalize wire/storage bytes (0-255) to the Slint bar scale (0-100).
    let waveform: Vec<i32> = meta
        .waveform_samples
        .as_ref()
        .filter(|samples| !samples.is_empty())
        .map(|samples| {
            let converted: Vec<i32> = samples
                .iter()
                .map(|&s| waveform_bar(i32::from(s)))
                .collect();
            if meta.is_audio() {
                let max = converted.iter().max().copied().unwrap_or(0);
                tracing::debug!(
                    "attachment_to_display_data: waveform has {} samples, max={}, first_5={:?}",
                    converted.len(),
                    max,
                    &converted[..5.min(converted.len())]
                );
            }
            converted
        })
        .unwrap_or_else(|| {
            if meta.is_audio() {
                tracing::warn!(
                    "attachment_to_display_data: no waveform_samples for audio, using flat default"
                );
            }
            vec![10; 32]
        }); // Default flat waveform

    // Format duration for audio
    let duration_display = meta
        .duration_seconds
        .map(|secs| {
            let total_secs = secs as u64;
            let minutes = total_secs / 60;
            let seconds = total_secs % 60;
            format!("{}:{:02}", minutes, seconds)
        })
        .unwrap_or_default();

    // Determine transfer status from attachment status
    let is_uploading = attachment.status == AttachmentStatus::Uploading;
    let is_downloading = attachment.status == AttachmentStatus::Downloading;
    let is_download_failed =
        attachment.status == AttachmentStatus::Failed && attachment.remote_url.is_some();

    AttachmentDisplayData {
        id: id_str,
        filename: meta.bubble_filename().to_string(),
        size_display: format_file_size(meta.size),
        mime_type: meta.mime_type.clone(),
        category: category.to_string(),
        thumbnail_url: String::new(), // Populated by caller if available
        download_url: attachment.remote_url.clone().unwrap_or_default(),
        local_path: None,
        is_image: meta.is_image(),
        is_video: chatter_core::domain::attachment::FileCategory::from_mime_and_filename(
            &meta.mime_type,
            &meta.name,
        ) == chatter_core::domain::attachment::FileCategory::Video,
        width: meta.width.unwrap_or(0) as i32,
        height: meta.height.unwrap_or(0) as i32,
        is_playing,
        icon: icon.to_string(),
        icon_color,
        waveform,
        duration_display,
        is_uploading,
        is_downloading,
        is_download_failed,
        progress: 0.0, // Will be updated by progress callback
        // A stored/received attachment is never mid-outgoing-upload.
        upload_state: String::new(),
        // Use stored thumbnail from sender (for instant preview)
        thumbnail_bytes: {
            let bytes: Arc<[u8]> = meta.thumbnail.as_deref().unwrap_or_default().into();
            if meta.is_image() || meta.is_video() {
                tracing::debug!(
                    "attachment_to_display_data: {} thumbnail_bytes={}, waveform_samples={:?}",
                    meta.name,
                    bytes.len(),
                    meta.waveform_samples.as_ref().map(|w| w.len())
                );
            }
            bytes
        },
        thumbnail_pixels: None, // Pre-decode with decode_thumbnail() before UI thread
    }
}

/// Convert a PendingAttachment to Slint PendingAttachmentData.
/// Note: This should be called from the UI thread for image loading to work.
pub fn pending_attachment_to_slint(attachment: &PendingAttachment) -> PendingAttachmentData {
    let is_image = attachment.mime_type.starts_with("image/");

    // Preview workers populate pixels later; path loaders may synchronously
    // decode the original and must never run in this presentation converter.
    let (has_thumbnail, thumbnail) = (false, Image::default());

    PendingAttachmentData {
        id: attachment.id.to_string().into(),
        filename: attachment.filename.clone().into(),
        size_display: format_file_size(attachment.size).into(),
        category: mime_to_category(&attachment.mime_type).into(),
        is_ready: true, // Already uploaded
        has_thumbnail,
        thumbnail,
    }
}

/// Off-thread history resolver: old downloaded videos also receive previews.
/// Four attempts per page bound decoder work independently of history size.
pub fn attachment_display_resolver(
    client: &chatter_core::client::ChatterClient,
) -> impl Fn(&chatter_core::domain::ids::AttachmentId) -> Option<Attachment> + '_ {
    #[cfg(target_os = "android")]
    let remaining = std::cell::Cell::new(4usize);
    let revision = client.session_revision();
    move |id| {
        if client.session_revision() != revision {
            return None;
        }
        let attachment = client.get_attachment_metadata(id)?;
        #[cfg(target_os = "android")]
        let mut attachment = attachment;
        #[cfg(target_os = "android")]
        if attachment.metadata.is_video()
            && attachment
                .metadata
                .thumbnail
                .as_ref()
                .is_none_or(|b| b.is_empty())
            && !attachment.needs_download()
            && remaining.get() > 0
        {
            remaining.set(remaining.get() - 1);
            match client.decrypt_and_extract_video_thumbnail(id) {
                Ok(result) => attachment.metadata.thumbnail = Some(result.image_bytes),
                Err(e) => tracing::warn!(error = %e, "History video preview extraction failed"),
            }
        }
        (client.session_revision() == revision).then_some(attachment)
    }
}
