//! Animated GIF decoder and frame extraction.
//!
//! Decodes GIF files into individual frames for animation playback.

use image::codecs::gif::GifDecoder;
use image::{AnimationDecoder, Frame, ImageFormat};
use std::io::Cursor;
use std::time::Duration;

use crate::error::MediaError;

/// A single frame from an animated GIF.
#[derive(Clone)]
pub struct GifFrame {
    /// RGBA pixel data.
    pub data: Vec<u8>,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Duration this frame should be displayed.
    pub delay: Duration,
}

/// Decoded animated GIF with all frames.
pub struct AnimatedGif {
    /// All frames in the animation.
    pub frames: Vec<GifFrame>,
    /// Total duration of one loop.
    pub total_duration: Duration,
    /// Whether the GIF should loop.
    pub loops: bool,
}

impl AnimatedGif {
    /// Decode an animated GIF from bytes.
    ///
    /// Returns `None` if the image is not an animated GIF or has only one frame.
    pub fn decode(bytes: &[u8]) -> Result<Option<Self>, MediaError> {
        // Check if it's a GIF
        let format = image::guess_format(bytes)
            .map_err(|e| MediaError::InvalidFormat(e.to_string()))?;

        if format != ImageFormat::Gif {
            return Ok(None);
        }

        let cursor = Cursor::new(bytes);
        let decoder = GifDecoder::new(cursor)
            .map_err(|e| MediaError::InvalidFormat(format!("GIF decode error: {}", e)))?;

        let frames_iter = decoder.into_frames();
        let mut frames = Vec::new();
        let mut total_duration = Duration::ZERO;

        for frame_result in frames_iter {
            let frame = frame_result
                .map_err(|e| MediaError::InvalidFormat(format!("GIF frame error: {}", e)))?;

            let delay = frame_delay(&frame);
            total_duration += delay;

            let buffer = frame.into_buffer();
            let (width, height) = buffer.dimensions();

            frames.push(GifFrame {
                data: buffer.into_raw(),
                width,
                height,
                delay,
            });
        }

        // Not animated if only one frame
        if frames.len() <= 1 {
            return Ok(None);
        }

        Ok(Some(Self {
            frames,
            total_duration,
            loops: true,
        }))
    }

    /// Get the number of frames.
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Get the average frame duration (for simple playback).
    pub fn average_frame_duration(&self) -> Duration {
        if self.frames.is_empty() {
            Duration::from_millis(100)
        } else {
            self.total_duration / self.frames.len() as u32
        }
    }
}

/// Extract delay from a frame, defaulting to 100ms if not specified.
fn frame_delay(frame: &Frame) -> Duration {
    let delay = frame.delay();
    let (numer, denom) = delay.numer_denom_ms();
    if denom == 0 {
        Duration::from_millis(100)
    } else {
        // numer_denom_ms returns the delay as a fraction in milliseconds
        // e.g., 40ms = numer=40, denom=1
        Duration::from_millis((numer as u64) / (denom as u64))
    }
}

/// Convert a GIF frame to a Slint Image.
pub fn frame_to_slint_image(frame: &GifFrame) -> slint::Image {
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        &frame.data,
        frame.width,
        frame.height,
    );
    slint::Image::from_rgba8(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_non_gif_returns_none() {
        // PNG header
        let png_bytes = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let result = AnimatedGif::decode(&png_bytes);
        assert!(result.is_err() || result.unwrap().is_none());
    }
}
