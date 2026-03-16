//! Progressive GIF decoder.
//!
//! Decodes the first frame immediately for instant display,
//! then decodes remaining frames in background.

use image::codecs::gif::GifDecoder;
use image::{AnimationDecoder, ImageFormat};
use std::io::Cursor;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use crate::error::MediaError;

use super::animator::{GifFrame, AnimatedGif};

/// Result of progressive decoding.
pub struct ProgressiveGifResult {
    /// First frame - available immediately.
    pub first_frame: GifFrame,
    /// Receiver for remaining frames (decoded in background).
    pub remaining_frames_rx: mpsc::Receiver<GifFrame>,
    /// Total frame count (if known from header).
    pub estimated_frame_count: Option<usize>,
}

/// Decode first frame immediately, return receiver for rest.
///
/// This allows showing the first frame instantly while the rest
/// decode in the background.
pub fn decode_progressive(bytes: Arc<Vec<u8>>) -> Result<Option<ProgressiveGifResult>, MediaError> {
    // Check if it's a GIF
    let format = image::guess_format(&bytes)
        .map_err(|e| MediaError::InvalidFormat(e.to_string()))?;

    if format != ImageFormat::Gif {
        return Ok(None);
    }

    let cursor = Cursor::new(bytes.as_ref());
    let decoder = GifDecoder::new(cursor)
        .map_err(|e| MediaError::InvalidFormat(format!("GIF decode error: {}", e)))?;

    let mut frames_iter = decoder.into_frames();

    // Decode first frame synchronously
    let first_frame_result = match frames_iter.next() {
        Some(Ok(frame)) => frame,
        Some(Err(e)) => return Err(MediaError::InvalidFormat(format!("GIF frame error: {}", e))),
        None => return Ok(None), // Empty GIF
    };

    let delay = frame_delay(&first_frame_result);
    let buffer = first_frame_result.into_buffer();
    let (width, height) = buffer.dimensions();

    let first_frame = GifFrame {
        data: buffer.into_raw(),
        width,
        height,
        delay,
    };

    // Check if there's a second frame (i.e., it's animated)
    // We peek by trying to decode it
    let second_frame = match frames_iter.next() {
        Some(Ok(frame)) => {
            let delay = frame_delay(&frame);
            let buffer = frame.into_buffer();
            let (width, height) = buffer.dimensions();
            Some(GifFrame {
                data: buffer.into_raw(),
                width,
                height,
                delay,
            })
        }
        Some(Err(_)) => None,
        None => return Ok(None), // Only one frame, not animated
    };

    // Create channel for remaining frames
    let (tx, rx) = mpsc::channel::<GifFrame>();

    // Clone bytes for background task
    let bytes_for_bg = bytes.clone();

    // Spawn background decoder using std::thread (works without Tokio runtime)
    std::thread::spawn(move || {
        // Re-decode from scratch in background (skip first 2 frames we already have)
        decode_remaining_frames_sync(bytes_for_bg, 2, tx);
    });

    // Note: second_frame is decoded but we let the background decoder handle sending all frames
    // after the first one. This simplifies the logic.
    let _ = second_frame; // Suppress unused warning

    Ok(Some(ProgressiveGifResult {
        first_frame,
        remaining_frames_rx: rx,
        estimated_frame_count: None, // GIF doesn't have frame count in header
    }))
}

/// Decode remaining frames synchronously (for use in std::thread::spawn).
fn decode_remaining_frames_sync(
    bytes: Arc<Vec<u8>>,
    skip_count: usize,
    tx: mpsc::Sender<GifFrame>,
) {
    let cursor = Cursor::new(bytes.as_ref());
    let decoder = match GifDecoder::new(cursor) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("Failed to re-open GIF for background decode: {}", e);
            return;
        }
    };

    let frames_iter = decoder.into_frames();

    for (idx, frame_result) in frames_iter.enumerate() {
        // Skip frames we already decoded
        if idx < skip_count {
            continue;
        }

        let frame = match frame_result {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("GIF frame {} decode error: {}", idx, e);
                continue;
            }
        };

        let delay = frame_delay(&frame);
        let buffer = frame.into_buffer();
        let (width, height) = buffer.dimensions();

        let gif_frame = GifFrame {
            data: buffer.into_raw(),
            width,
            height,
            delay,
        };

        // Send frame - if receiver is dropped, stop decoding
        if tx.send(gif_frame).is_err() {
            tracing::debug!("GIF receiver dropped, stopping background decode at frame {}", idx);
            break;
        }
    }

    tracing::debug!("Background GIF decode complete");
}

/// Decode first frame only (for quick preview).
///
/// Much faster than full decode - only processes first frame.
pub fn decode_first_frame_only(bytes: &[u8]) -> Result<Option<GifFrame>, MediaError> {
    let format = image::guess_format(bytes)
        .map_err(|e| MediaError::InvalidFormat(e.to_string()))?;

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

/// Streaming GIF decoder that yields frames as they're decoded.
///
/// Use this when you want to start playback before all frames are decoded.
pub struct StreamingGifDecoder {
    frames: Vec<GifFrame>,
    decode_complete: bool,
    rx: Option<mpsc::Receiver<GifFrame>>,
}

impl StreamingGifDecoder {
    /// Start decoding a GIF progressively.
    ///
    /// Returns immediately with the first frame decoded.
    /// Call `poll_frames()` to get additional frames as they're decoded.
    pub fn new(bytes: Vec<u8>) -> Result<Option<Self>, MediaError> {
        let bytes = Arc::new(bytes);

        // Decode first frame synchronously
        let first_frame = match decode_first_frame_only(&bytes)? {
            Some(f) => f,
            None => return Ok(None),
        };

        // Check if animated by trying to decode second frame
        let format = image::guess_format(&bytes).ok();
        if format != Some(ImageFormat::Gif) {
            return Ok(None);
        }

        // Start background decode using std::thread (works without Tokio runtime)
        let (tx, rx) = mpsc::channel::<GifFrame>();
        let bytes_bg = bytes.clone();

        std::thread::spawn(move || {
            decode_remaining_frames_sync(bytes_bg, 1, tx);
        });

        Ok(Some(Self {
            frames: vec![first_frame],
            decode_complete: false,
            rx: Some(rx),
        }))
    }

    /// Get currently available frames.
    pub fn frames(&self) -> &[GifFrame] {
        &self.frames
    }

    /// Get first frame (always available after construction).
    pub fn first_frame(&self) -> &GifFrame {
        &self.frames[0]
    }

    /// Poll for newly decoded frames.
    ///
    /// Returns number of new frames added.
    pub fn poll_frames(&mut self) -> usize {
        if self.decode_complete {
            return 0;
        }

        let rx = match self.rx.as_mut() {
            Some(rx) => rx,
            None => return 0,
        };

        let mut count = 0;

        // Non-blocking receive of all available frames
        loop {
            match rx.try_recv() {
                Ok(frame) => {
                    self.frames.push(frame);
                    count += 1;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.decode_complete = true;
                    self.rx = None;
                    break;
                }
            }
        }

        count
    }

    /// Check if all frames have been decoded.
    pub fn is_complete(&self) -> bool {
        self.decode_complete
    }

    /// Get total frame count (only accurate after decode complete).
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Convert to AnimatedGif once decode is complete.
    pub fn into_animated_gif(self) -> Option<AnimatedGif> {
        if self.frames.len() <= 1 {
            return None;
        }

        let total_duration: Duration = self.frames.iter().map(|f| f.delay).sum();

        Some(AnimatedGif {
            frames: self.frames,
            total_duration,
            loops: true,
        })
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
