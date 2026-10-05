//! Attachment converters.

use slint::{Image, ModelRc, VecModel};

use chatter_core::domain::attachment::{Attachment, AttachmentStatus};
use chatter_core::state::PendingAttachment;

use crate::bin_ui::audio_state;
use crate::{MessageAttachmentData, PendingAttachmentData};

use super::utils::{category_to_icon_and_color, format_file_size, mime_to_category};

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
    pub progress: f32,
    /// Thumbnail JPEG bytes from sender (for instant preview without download).
    pub thumbnail_bytes: Vec<u8>,
    /// Decoded thumbnail, from `decode_cache` — decode before the UI thread.
    /// Shared with the cache, so the UI thread wraps it without copying.
    pub thumbnail_pixels: Option<crate::bin_ui::decode_cache::Pixels>,
}

impl AttachmentDisplayData {
    /// Pre-decode thumbnail bytes to RGBA pixels (call this before invoke_from_event_loop).
    pub fn decode_thumbnail(&mut self) {
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
        // The stored thumbnail is not a postage stamp: it is generated at
        // `INLINE_THUMBNAIL_MAX_SIZE` = 768px (media/image_processor.rs), ~2.7x
        // the ~280 logical px the bubble draws. Decoding it costs ~0.08 ms.
        //
        // This used to prefer the full-resolution original instead, downscaled
        // with Lanczos3 "so the bubble is crisp". Measured on a 12 MP photo
        // (`chatter-core/tests/conversation_open_bench.rs`): 46 ms to decode +
        // 900 ms to Lanczos3-resize, per image, every time the conversation is
        // opened — 95% of the entire conversation-open budget, for a difference
        // invisible at 280px. Cheap first, original only as a fallback.
        if !self.thumbnail_bytes.is_empty() {
            let bytes = &self.thumbnail_bytes;
            self.thumbnail_pixels = crate::bin_ui::decode_cache::thumbnail(&self.id, bytes.len(), || {
                image::load_from_memory(bytes).ok()
            });
            if self.thumbnail_pixels.is_some() {
                return;
            }
        }
        // No stored thumbnail (sidecar not fetched yet, or a pre-thumbnail
        // local file). Downscale the original with the fast box sampler —
        // `thumbnail()`, not `resize(Lanczos3)`: 6.6x cheaper and the
        // difference does not survive being drawn at bubble width.
        if self.is_image {
            if let Some(path) = self
                .local_path
                .as_deref()
                .filter(|p| std::path::Path::new(p).exists())
            {
                // Cap the long edge; ample even at 2x HiDPI while staying
                // cheap to hold in memory.
                const DISPLAY_MAX: u32 = 1024;
                self.thumbnail_pixels = crate::bin_ui::decode_cache::thumbnail(&self.id, 0, || {
                    image::open(path).ok().map(|img| img.thumbnail(DISPLAY_MAX, DISPLAY_MAX))
                });
            }
        }
    }

    /// Convert to Slint MessageAttachmentData (must be called on UI thread).
    pub fn to_slint(&self) -> MessageAttachmentData {
        // Priority 0: Use pre-decoded pixels (fastest - no decoding on UI thread)
        // Priority 1: Try to load thumbnail from sender-provided bytes (instant preview)
        // Priority 2: Load from local path after download
        let (has_thumbnail, thumbnail, img_width, img_height) = if let Some(pixels) = &self.thumbnail_pixels {
            // Pre-decoded and shared: wrapping it is a reference count, no copy.
            let (w, h) = (pixels.width(), pixels.height());
            (true, Image::from_rgba8(pixels.clone()), w as i32, h as i32)
        } else if !self.thumbnail_bytes.is_empty() {
            // Fallback: decode on UI thread (should be avoided - call decode_thumbnail() first)
            tracing::warn!(
                "Decoding thumbnail on UI thread for {} - consider calling decode_thumbnail() earlier",
                self.filename
            );
            match image::load_from_memory(&self.thumbnail_bytes) {
                Ok(img) => {
                    let rgba = img.to_rgba8();
                    let (w, h) = rgba.dimensions();
                    let slint_img = crate::bin_ui::slint_conversions::rgba_to_slint_image(rgba.as_raw(),
                        w,
                        h,);
                    (true, slint_img, w as i32, h as i32)
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to decode thumbnail bytes for {}: {:?}",
                        self.filename,
                        e
                    );
                    (false, Image::default(), self.width, self.height)
                }
            }
        } else if let Some(ref path) = self.local_path {
            // Load from local file after download
            if self.is_image || self.is_video {
                let path_obj = std::path::Path::new(path);
                if !path_obj.exists() {
                    tracing::info!("Attachment thumbnail path does not exist: {}", path);
                    (false, Image::default(), self.width, self.height)
                } else {
                    // For videos, we need to extract a thumbnail if it hasn't been done yet
                    // But Image::load_from_path only works for images.
                    // If it's a video and we are here, it means we don't have thumbnail_bytes.
                    if self.is_video {
                        tracing::info!("Skipping local video thumbnail loading for {} (not implemented)", self.filename);
                        (false, Image::default(), self.width, self.height)
                    } else {
                        match Image::load_from_path(path_obj) {
                            Ok(img) => {
                                // Extract actual dimensions from loaded image
                                let size = img.size();
                                let w = size.width as i32;
                                let h = size.height as i32;
                                tracing::info!(
                                    "Loaded thumbnail from local path for {}: {}x{}",
                                    self.filename,
                                    w,
                                    h
                                );
                                (true, img, w, h)
                            }
                            Err(e) => {
                                tracing::warn!("Failed to load thumbnail for {}: {:?}", self.filename, e);
                                (false, Image::default(), self.width, self.height)
                            }
                        }
                    }
                }
            } else {
                (false, Image::default(), self.width, self.height)
            }
        } else if self.is_image || self.is_video {
            // Only log when a thumbnail was actually expected (image/video);
            // audio and generic files never have one, so suppress the noise.
            tracing::info!("No thumbnail source available for {}", self.filename);
            (false, Image::default(), self.width, self.height)
        } else {
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
            progress: self.progress,
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
            let converted: Vec<i32> = samples.iter().map(|&s| waveform_bar(i32::from(s))).collect();
            if meta.is_audio() {
                let max = converted.iter().max().copied().unwrap_or(0);
                tracing::debug!(
                    "attachment_to_display_data: waveform has {} samples, max={}, first_5={:?}",
                    converted.len(), max, &converted[..5.min(converted.len())]
                );
            }
            converted
        })
        .unwrap_or_else(|| {
            if meta.is_audio() {
                tracing::warn!("attachment_to_display_data: no waveform_samples for audio, using flat default");
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

    AttachmentDisplayData {
        id: id_str,
        filename: meta.name.clone(),
        size_display: format_file_size(meta.size),
        mime_type: meta.mime_type.clone(),
        category: category.to_string(),
        thumbnail_url: String::new(), // Populated by caller if available
        download_url: attachment.remote_url.clone().unwrap_or_default(),
        local_path: attachment.local_path.clone(),
        is_image: meta.is_image(),
        is_video: meta.is_video(),
        width: meta.width.unwrap_or(0) as i32,
        height: meta.height.unwrap_or(0) as i32,
        is_playing,
        icon: icon.to_string(),
        icon_color,
        waveform,
        duration_display,
        is_uploading,
        is_downloading,
        progress: 0.0, // Will be updated by progress callback
        // Use stored thumbnail from sender (for instant preview)
        thumbnail_bytes: {
            let bytes = meta.thumbnail.clone().unwrap_or_default();
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

    // Try to load thumbnail for images
    let (has_thumbnail, thumbnail) = if is_image && attachment.path.exists() {
        match Image::load_from_path(&attachment.path) {
            Ok(img) => (true, img),
            Err(e) => {
                tracing::warn!("Failed to load thumbnail for {}: {:?}", attachment.filename, e);
                (false, Image::default())
            }
        }
    } else {
        (false, Image::default())
    };

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
