//! Media processing for attachments.
//!
//! Decryption + decoding for GIFs and images. Keeps crypto/media work in service layer.

use std::time::Duration;

use crate::domain::ids::AttachmentId;
use crate::media::ImageProcessor;
use crate::media::gif::{GifFrame, decode_first_frame_only};

use super::{AttachmentError, AttachmentService};

/// Result of GIF decryption and first-frame decode.
pub struct GifDecodeResult {
    /// First frame - available immediately.
    pub first_frame: GifFrame,
    /// Decrypted GIF bytes for full decode (owned, will be moved to background thread).
    pub gif_bytes: Vec<u8>,
}

/// Result of image decryption with dimensions.
pub struct ImageDecodeResult {
    /// Decrypted image bytes.
    pub image_bytes: Vec<u8>,
    /// Image width.
    pub width: u32,
    /// Image height.
    pub height: u32,
}

/// All frames from a fully decoded GIF.
pub struct GifFullDecodeResult {
    /// All decoded frames with RGBA data and delays.
    pub frames: Vec<GifFrame>,
}

impl AttachmentService {
    /// Decrypt attachment and decode first GIF frame.
    ///
    /// Returns the first frame immediately for instant display, plus the raw
    /// bytes for background decoding of remaining frames.
    ///
    /// This keeps crypto in the service layer - UI only receives decoded pixels.
    pub fn decrypt_and_decode_gif_first_frame(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<Option<GifDecodeResult>, AttachmentError> {
        // Decrypt attachment
        let secure_buffer = self.decrypt_auto(attachment_id)?;
        let data = secure_buffer.as_slice().to_vec();

        // Decode first frame only (fast, ~10-20ms)
        match decode_first_frame_only(&data) {
            Ok(Some(first_frame)) => {
                tracing::debug!(
                    attachment_id = %attachment_id,
                    width = first_frame.width,
                    height = first_frame.height,
                    "Decoded first GIF frame"
                );
                Ok(Some(GifDecodeResult {
                    first_frame,
                    gif_bytes: data,
                }))
            }
            Ok(None) => {
                tracing::debug!(attachment_id = %attachment_id, "Not a GIF");
                Ok(None)
            }
            Err(e) => {
                tracing::error!(
                    attachment_id = %attachment_id,
                    error = ?e,
                    "Failed to decode GIF first frame"
                );
                Err(AttachmentError::DecryptionError(format!(
                    "GIF decode failed: {}",
                    e
                )))
            }
        }
    }

    /// Decrypt attachment and get image dimensions.
    ///
    /// Returns decrypted image bytes and dimensions for thumbnail generation.
    /// This keeps crypto in the service layer.
    pub fn decrypt_and_get_image_info(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<ImageDecodeResult, AttachmentError> {
        // Decrypt attachment
        let secure_buffer = self.decrypt_auto(attachment_id)?;
        let image_bytes = secure_buffer.as_slice().to_vec();

        // Get dimensions
        let (width, height) =
            ImageProcessor::get_dimensions_from_memory(&image_bytes).map_err(|e| {
                AttachmentError::DecryptionError(format!("Failed to get image dimensions: {:?}", e))
            })?;

        tracing::debug!(
            attachment_id = %attachment_id,
            width = width,
            height = height,
            "Got image dimensions"
        );

        Ok(ImageDecodeResult {
            image_bytes,
            width,
            height,
        })
    }

    /// Decrypt a video attachment and extract a thumbnail frame.
    ///
    /// Server-managed DMs strip the inline thumbnail before sealing (sealed
    /// envelope is padded to ≤4096-byte buckets for metadata privacy), so the
    /// receiver derives one from the downloaded video. Returns the encoded
    /// thumbnail bytes + dims, reusing [`ImageDecodeResult`] (`image_bytes` =
    /// the thumbnail).
    ///
    /// Blocking (GStreamer decode/seek) — call from a background thread.
    pub fn decrypt_and_extract_video_thumbnail(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<ImageDecodeResult, AttachmentError> {
        let secure_buffer = self.decrypt_auto(attachment_id)?;
        #[cfg(feature = "desktop-gstreamer")]
        {
            let (thumbnail, width, height, _duration_ms) =
                crate::media::extract_video_thumbnail_from_memory(secure_buffer.as_slice())
                    .map_err(|e| {
                        AttachmentError::DecryptionError(format!(
                            "Video thumbnail extraction failed: {}",
                            e
                        ))
                    })?;

            tracing::debug!(
                attachment_id = %attachment_id,
                width = width,
                height = height,
                "Extracted video thumbnail on receive"
            );

            Ok(ImageDecodeResult {
                image_bytes: thumbnail,
                width,
                height,
            })
        }
        // GStreamer video processing is desktop-gstreamer only; mobile has no
        // frame extractor, so the receiver simply renders without a thumbnail.
        #[cfg(not(feature = "desktop-gstreamer"))]
        {
            let _ = secure_buffer;
            Err(AttachmentError::DecryptionError(
                "video thumbnail extraction unavailable on this platform".into(),
            ))
        }
    }

    /// Decode all frames from GIF bytes (blocking, for background thread).
    ///
    /// Call this from a background thread with the `gif_bytes` from `GifDecodeResult`.
    /// Returns all frames ready for animation registration.
    ///
    /// Fast path: a plain CPU GStreamer decode ([`decode_all_frames_cpu`]) —
    /// pulls every frame immediately and honors the buffer stride. Measured
    /// ~0.15s vs ~2.3s for the pure-Rust `image` crate on a 93-frame GIF, and
    /// avoids the flaky GL pipeline that produced black frames. Falls back to
    /// the `image` crate if GStreamer is unavailable or fails.
    pub fn decode_gif_all_frames(gif_bytes: &[u8]) -> Result<GifFullDecodeResult, AttachmentError> {
        #[cfg(feature = "desktop-gstreamer")]
        {
            match crate::media::gif::decode_all_frames_cpu(gif_bytes) {
                Ok(frames) if frames.len() > 1 => return Ok(GifFullDecodeResult { frames }),
                Ok(_) => {} // 0/1 frame — fall through to the image crate
                Err(e) => {
                    tracing::warn!("GStreamer GIF decode failed ({e}), using image crate");
                }
            }
        }

        Self::decode_gif_all_frames_image(gif_bytes)
    }

    /// Pure-Rust `image` crate GIF decode — fallback for `decode_gif_all_frames`.
    fn decode_gif_all_frames_image(
        gif_bytes: &[u8],
    ) -> Result<GifFullDecodeResult, AttachmentError> {
        use image::AnimationDecoder;
        use image::codecs::gif::GifDecoder;
        use std::io::Cursor;

        let cursor = Cursor::new(gif_bytes);
        let decoder = GifDecoder::new(cursor)
            .map_err(|e| AttachmentError::DecryptionError(format!("GIF decoder error: {}", e)))?;

        let mut frames = Vec::new();
        for frame_result in decoder.into_frames() {
            let frame = match frame_result {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("GIF frame decode error: {}", e);
                    continue;
                }
            };

            let (numer, denom) = frame.delay().numer_denom_ms();
            let delay_ms = if denom == 0 {
                100
            } else {
                ((numer as u64) / (denom as u64)).max(20)
            };

            let buffer = frame.into_buffer();
            let (width, height) = buffer.dimensions();
            frames.push(GifFrame {
                data: buffer.into_raw(),
                width,
                height,
                delay: Duration::from_millis(delay_ms),
            });
        }

        Ok(GifFullDecodeResult { frames })
    }
}
