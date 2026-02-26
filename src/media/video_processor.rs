//! Video processor for thumbnail extraction.
//!
//! Uses platform-native video decoders to extract frames from video files.
//! On Linux, uses GStreamer. On mobile, uses platform-specific APIs.

use crate::error::MediaError;
use crate::media::decoder::{self, VideoInfo};
use image::ImageEncoder;
use std::path::Path;

/// Maximum thumbnail dimension (width or height).
/// 400px gives good quality while UI displays at max 300px (slight downscale is fine).
const THUMBNAIL_MAX_SIZE: u32 = 400;

/// Extract a thumbnail from a video file.
///
/// Seeks to 1 second (or first frame if video is shorter) and extracts a frame,
/// then scales it down and compresses as WebP for efficient storage.
///
/// # Arguments
///
/// * `source` - Path to the video file
///
/// # Returns
///
/// Tuple of (webp_bytes, width, height, duration_ms) or error.
pub fn extract_video_thumbnail(
    source: &Path,
) -> Result<(Vec<u8>, u32, u32, u64), MediaError> {
    let mut dec = decoder::create_decoder()?;
    let info = dec.open(source)?;

    extract_thumbnail_impl(&mut *dec, &info)
}

/// Extract a thumbnail from video data in memory.
pub fn extract_video_thumbnail_from_memory(
    data: &[u8],
) -> Result<(Vec<u8>, u32, u32, u64), MediaError> {
    if data.is_empty() {
        return Err(MediaError::InvalidFormat("empty video data".to_string()));
    }

    let mut dec = decoder::create_decoder()?;
    let info = dec.open_memory(data.to_vec())?;

    extract_thumbnail_impl(&mut *dec, &info)
}

/// Internal implementation for thumbnail extraction.
fn extract_thumbnail_impl(
    dec: &mut dyn decoder::VideoDecoder,
    info: &VideoInfo,
) -> Result<(Vec<u8>, u32, u32, u64), MediaError> {
    let src_width = info.width;
    let src_height = info.height;
    let duration_ms = info.duration_ms;

    // For thumbnail extraction, we need a single frame from the video.
    // The pipeline is in PAUSED state after open(), so we use the preroll sample.
    // For longer videos, seek to 1s to skip intro/black frames.
    let seek_pos = if duration_ms > 1000 { 1000 } else { 0 };

    if seek_pos > 0 {
        dec.seek(seek_pos)?;
    }

    // Briefly start the pipeline to PLAYING to allow frame delivery with sync(true),
    // then immediately pause to get the frame. This is more reliable than trying to
    // use preroll samples after seeking, since the preroll mechanism can be tricky.
    dec.play()?;

    // Give the pipeline time to start producing frames after state change
    // Some codecs need extra time to decode their first frame
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Decode a frame with retries - after play(), frames should arrive quickly.
    let frame = {
        let mut attempts = 0;
        const MAX_ATTEMPTS: u32 = 40; // 40 * 50ms = 2 seconds max wait
        loop {
            match dec.decode_video_frame() {
                Ok(Some(frame)) => {
                    // Got a frame, pause the pipeline and return
                    let _ = dec.pause();
                    break frame;
                }
                Ok(None) => {
                    let _ = dec.pause();
                    return Err(MediaError::InvalidFormat(
                        "could not decode any frame (EOS)".to_string(),
                    ));
                }
                Err(e) if e.to_string().contains("no_sample") && attempts < MAX_ATTEMPTS => {
                    // Pipeline warming up, wait and retry (shorter delay since clock is running)
                    attempts += 1;
                    if attempts % 10 == 0 {
                        tracing::trace!("Thumbnail extraction: {} attempts, still waiting for frame", attempts);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    continue;
                }
                Err(e) => {
                    let _ = dec.pause();
                    return Err(e);
                }
            }
        }
    };

    // Log if frame timestamp is unexpected (helps debug issues)
    // If we got a frame from near the end of the video, something went wrong
    if duration_ms > 1000 && frame.timestamp_ms > duration_ms.saturating_sub(500) {
        tracing::warn!(
            "Thumbnail extracted from near end of video ({}ms / {}ms) - seek may have failed",
            frame.timestamp_ms, duration_ms
        );
    }

    // Calculate thumbnail dimensions (maintain aspect ratio)
    let (thumb_width, thumb_height) = if src_width > src_height {
        let w = THUMBNAIL_MAX_SIZE.min(src_width);
        let h = (src_height as f64 * w as f64 / src_width as f64) as u32;
        (w, h.max(1))
    } else {
        let h = THUMBNAIL_MAX_SIZE.min(src_height);
        let w = (src_width as f64 * h as f64 / src_height as f64) as u32;
        (w.max(1), h)
    };

    // Create image from RGB data
    let img = image::RgbImage::from_raw(frame.width, frame.height, frame.data)
        .ok_or_else(|| MediaError::ThumbnailGeneration("invalid frame data".to_string()))?;

    // Resize to thumbnail dimensions
    let resized = image::imageops::resize(
        &img,
        thumb_width,
        thumb_height,
        image::imageops::FilterType::Triangle,
    );

    // Convert to DynamicImage for WebP encoding
    let dyn_img = image::DynamicImage::ImageRgb8(resized);

    // Encode to WebP
    let mut webp_bytes: Vec<u8> = Vec::new();
    let encoder = image::codecs::webp::WebPEncoder::new_lossless(&mut webp_bytes);
    encoder
        .write_image(
            dyn_img.as_bytes(),
            thumb_width,
            thumb_height,
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|e| MediaError::ThumbnailGeneration(format!("WebP encoding failed: {}", e)))?;

    tracing::debug!(
        "Extracted video thumbnail (WebP): {}x{} -> {}x{}, {} bytes, duration {}ms",
        src_width,
        src_height,
        thumb_width,
        thumb_height,
        webp_bytes.len(),
        duration_ms
    );

    Ok((webp_bytes, thumb_width, thumb_height, duration_ms))
}

/// Get video duration in milliseconds.
pub fn get_video_duration(source: &Path) -> Result<u64, MediaError> {
    let info = decoder::get_video_info(source)?;
    Ok(info.duration_ms)
}

/// Get video duration from memory data.
pub fn get_video_duration_from_memory(data: &[u8]) -> Result<u64, MediaError> {
    if data.is_empty() {
        return Ok(0);
    }

    let info = decoder::get_video_info_from_memory(data.to_vec())?;
    Ok(info.duration_ms)
}

/// Get video dimensions without decoding frames.
pub fn get_video_dimensions(source: &Path) -> Result<(u32, u32), MediaError> {
    let info = decoder::get_video_info(source)?;
    Ok((info.width, info.height))
}

/// Get video dimensions from memory data.
pub fn get_video_dimensions_from_memory(data: &[u8]) -> Result<(u32, u32), MediaError> {
    if data.is_empty() {
        return Err(MediaError::InvalidFormat("empty video data".to_string()));
    }

    let info = decoder::get_video_info_from_memory(data.to_vec())?;
    Ok((info.width, info.height))
}
