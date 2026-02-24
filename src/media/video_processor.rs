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

    // Seek to 1 second if video is long enough
    if duration_ms > 1000 {
        dec.seek(1000)?;
    }

    // Decode a frame
    let frame = dec.decode_video_frame()?.ok_or_else(|| {
        MediaError::InvalidFormat("could not decode any frame".to_string())
    })?;

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
