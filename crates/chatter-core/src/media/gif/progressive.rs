//! GIF first-frame decode for instant static preview.

use image::codecs::gif::GifDecoder;
use image::{AnimationDecoder, ImageFormat};
use std::io::Cursor;
use std::time::Duration;

use crate::error::MediaError;

use super::animator::GifFrame;

/// Decode first frame only (for quick preview).
///
/// Much faster than full decode - only processes first frame.
pub fn decode_first_frame_only(bytes: &[u8]) -> Result<Option<GifFrame>, MediaError> {
    let format =
        image::guess_format(bytes).map_err(|e| MediaError::InvalidFormat(e.to_string()))?;

    if format != ImageFormat::Gif {
        return Ok(None);
    }

    let cursor = Cursor::new(bytes);
    let decoder = GifDecoder::new(cursor)
        .map_err(|e| MediaError::InvalidFormat(format!("GIF decode error: {}", e)))?;

    let mut frames_iter = decoder.into_frames();

    match frames_iter.next() {
        Some(Ok(frame)) => {
            let delay = frame_delay(&frame);
            let buffer = frame.into_buffer();
            let (width, height) = buffer.dimensions();

            Ok(Some(GifFrame {
                data: buffer.into_raw(),
                width,
                height,
                delay,
            }))
        }
        Some(Err(e)) => Err(MediaError::InvalidFormat(format!("GIF frame error: {}", e))),
        None => Ok(None),
    }
}

/// Extract delay from an image frame.
fn frame_delay(frame: &image::Frame) -> Duration {
    let delay = frame.delay();
    let (numer, denom) = delay.numer_denom_ms();
    if denom == 0 {
        Duration::from_millis(100)
    } else {
        Duration::from_millis((numer as u64) / (denom as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: These tests require actual GIF files
    // They're marked as ignored for CI

    #[test]
    fn test_decode_first_frame_non_gif() {
        // PNG header
        let png_bytes = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let result = decode_first_frame_only(&png_bytes);
        assert!(result.is_err() || result.unwrap().is_none());
    }
}
