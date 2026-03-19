//! GIF support.
//!
//! GIF search, selection, caching via Tenor API, and animation playback.
//!
//! ## Progressive Loading
//!
//! For large GIFs, use `progressive::StreamingGifDecoder` to show the first
//! frame immediately while remaining frames decode in background.
//!
//! ## GPU Acceleration
//!
//! When GStreamer is available with GL support, GIFs are decoded using
//! GPU-accelerated pipelines for better performance:
//!
//! - Linux: VA-API, NVIDIA NVDEC, or software fallback
//! - Android: MediaCodec via GStreamer
//! - iOS/macOS: VideoToolbox via GStreamer
//! - Windows: Direct3D or software fallback
//!
//! Use `is_gpu_acceleration_available()` to check at runtime.

pub mod animator;
pub mod cache;
pub mod client;
pub mod gstreamer_decoder;
pub mod picker;
pub mod progressive;

// Re-exports for convenient access
pub use animator::{AnimatedGif, GifFrame};
pub use cache::GifCache;
pub use client::{GifClient, GifResult};
pub use picker::GifPicker;
pub use progressive::{decode_first_frame_only, StreamingGifDecoder};

// GStreamer decoder exports
pub use gstreamer_decoder::{
    GStreamerGifDecoder,
    GifInfo,
    StreamingGStreamerDecoder,
    is_gstreamer_gif_available,
    is_gl_available,
};

/// Check if GPU-accelerated GIF decoding is available.
///
/// Returns `true` if GStreamer with GL support is available for
/// hardware-accelerated GIF processing.
pub fn is_gpu_acceleration_available() -> bool {
    is_gstreamer_gif_available() && is_gl_available()
}

/// Decode a GIF using the best available method.
///
/// Automatically selects between:
/// 1. GStreamer + GL (GPU accelerated) - if available
/// 2. GStreamer + CPU - if GL not available
/// 3. Pure Rust `image` crate - fallback
///
/// Returns frames progressively for immediate display.
pub fn decode_gif_auto(data: Vec<u8>) -> Result<Box<dyn GifDecoder>, crate::error::MediaError> {
    // Try GStreamer first (GPU accelerated when available)
    if is_gstreamer_gif_available() {
        match StreamingGStreamerDecoder::new(data.clone()) {
            Ok(Some(decoder)) => {
                let accel = if is_gl_available() { "GPU" } else { "CPU" };
                tracing::info!("Using GStreamer GIF decoder ({} accelerated)", accel);
                return Ok(Box::new(GStreamerGifDecoderWrapper(decoder)));
            }
            Ok(None) => {
                tracing::debug!("GStreamer returned no frames, falling back");
            }
            Err(e) => {
                tracing::debug!("GStreamer GIF decoder failed: {}, falling back", e);
            }
        }
    }

    // Fallback to pure Rust decoder
    tracing::info!("Using pure Rust GIF decoder (software)");
    match StreamingGifDecoder::new(data) {
        Ok(Some(decoder)) => Ok(Box::new(RustGifDecoderWrapper(decoder))),
        Ok(None) => Err(crate::error::MediaError::InvalidFormat("Not an animated GIF".to_string())),
        Err(e) => Err(e),
    }
}

/// Unified GIF decoder trait for abstraction over different backends.
pub trait GifDecoder: Send {
    /// Get the first frame (always available).
    fn first_frame(&self) -> &GifFrame;

    /// Get all currently decoded frames.
    fn frames(&self) -> &[GifFrame];

    /// Poll for newly decoded frames.
    fn poll_frames(&mut self) -> usize;

    /// Check if decode is complete.
    fn is_complete(&self) -> bool;

    /// Get frame count.
    fn frame_count(&self) -> usize;
}

// Wrapper for GStreamer decoder
struct GStreamerGifDecoderWrapper(StreamingGStreamerDecoder);

impl GifDecoder for GStreamerGifDecoderWrapper {
    fn first_frame(&self) -> &GifFrame {
        self.0.first_frame()
    }

    fn frames(&self) -> &[GifFrame] {
        self.0.frames()
    }

    fn poll_frames(&mut self) -> usize {
        self.0.poll_frames()
    }

    fn is_complete(&self) -> bool {
        self.0.is_complete()
    }

    fn frame_count(&self) -> usize {
        self.0.frame_count()
    }
}

// Wrapper for pure Rust decoder
struct RustGifDecoderWrapper(StreamingGifDecoder);

impl GifDecoder for RustGifDecoderWrapper {
    fn first_frame(&self) -> &GifFrame {
        self.0.first_frame()
    }

    fn frames(&self) -> &[GifFrame] {
        self.0.frames()
    }

    fn poll_frames(&mut self) -> usize {
        self.0.poll_frames()
    }

    fn is_complete(&self) -> bool {
        self.0.is_complete()
    }

    fn frame_count(&self) -> usize {
        self.0.frame_count()
    }
}
