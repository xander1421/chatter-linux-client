//! Media processing for attachments.
//!
//! Decryption + decoding for GIFs and images. Keeps crypto/media work in service layer.

use std::sync::mpsc;
use std::time::Duration;

use crate::domain::ids::AttachmentId;
use crate::media::gif::{decode_first_frame_only, GifFrame};
use crate::media::ImageProcessor;

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
        let (width, height) = ImageProcessor::get_dimensions_from_memory(&image_bytes)
            .map_err(|e| {
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

    /// Decode all frames from GIF bytes (blocking, for background thread).
    ///
    /// Call this from a background thread with the `gif_bytes` from `GifDecodeResult`.
    /// Returns all frames ready for animation registration.
    pub fn decode_gif_all_frames(gif_bytes: &[u8]) -> Result<GifFullDecodeResult, AttachmentError> {
        use image::codecs::gif::GifDecoder;
        use image::AnimationDecoder;
        use std::io::Cursor;

        let cursor = Cursor::new(gif_bytes);
        let decoder = GifDecoder::new(cursor).map_err(|e| {
            AttachmentError::DecryptionError(format!("GIF decoder error: {}", e))
        })?;

        let frames_iter = decoder.into_frames();
        let mut frames = Vec::new();

        for frame_result in frames_iter {
            let frame = match frame_result {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("GIF frame decode error: {}", e);
                    continue;
                }
            };

            let delay = frame.delay();
            let (numer, denom) = delay.numer_denom_ms();
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

    /// Decode GIF frames progressively, sending to a channel.
    ///
    /// Call this from a background thread. Frames are sent as they're decoded,
    /// allowing progressive animation updates.
    pub fn decode_gif_progressive(
        gif_bytes: Vec<u8>,
        tx: mpsc::Sender<Vec<GifFrame>>,
        batch_size: usize,
    ) {
        use image::codecs::gif::GifDecoder;
        use image::AnimationDecoder;
        use std::io::Cursor;

        let cursor = Cursor::new(&gif_bytes);
        let decoder = match GifDecoder::new(cursor) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("Failed to create GIF decoder: {}", e);
                return;
            }
        };

        let frames_iter = decoder.into_frames();
        let mut all_frames: Vec<GifFrame> = Vec::new();
        let mut animation_started = false;

        for frame_result in frames_iter {
            let frame = match frame_result {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("GIF frame decode error: {}", e);
                    continue;
                }
            };

            let delay = frame.delay();
            let (numer, denom) = delay.numer_denom_ms();
            let delay_ms = if denom == 0 {
                100
            } else {
                ((numer as u64) / (denom as u64)).max(20)
            };

            let buffer = frame.into_buffer();
            let (width, height) = buffer.dimensions();

            all_frames.push(GifFrame {
                data: buffer.into_raw(),
                width,
                height,
                delay: Duration::from_millis(delay_ms),
            });

            // Start animation early once we have enough frames
            if !animation_started && all_frames.len() >= 2 {
                animation_started = true;
                if tx.send(all_frames.clone()).is_err() {
                    tracing::debug!("GIF receiver dropped, stopping decode");
                    return;
                }
            } else if animation_started && all_frames.len() % batch_size == 0 {
                if tx.send(all_frames.clone()).is_err() {
                    tracing::debug!("GIF receiver dropped, stopping decode");
                    return;
                }
            }
        }

        // Final batch with all frames
        if !all_frames.is_empty() {
            let _ = tx.send(all_frames);
        }
    }
}
