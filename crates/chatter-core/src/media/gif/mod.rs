//! GIF support.
//!
//! GIF search, selection, caching via Tenor API, and animation playback.
//!
//! ## Decoding
//!
//! - [`decode_first_frame_only`] — instant static preview.
//! - [`decode_all_frames_cpu`] — full decode via a plain CPU GStreamer pipeline
//!   (fast, stride-correct), with a pure-Rust `image` crate fallback in callers.

pub mod animator;
pub mod cache;
pub mod client;
#[cfg(feature = "desktop-media")]
pub mod gstreamer_decoder;
pub mod picker;
pub mod progressive;

// Re-exports for convenient access
pub use animator::{AnimatedGif, GifFrame};
pub use cache::GifCache;
pub use client::{GifClient, GifResult};
pub use picker::GifPicker;
pub use progressive::decode_first_frame_only;

// GStreamer decoder exports
#[cfg(feature = "desktop-media")]
pub use gstreamer_decoder::{
    decode_all_frames_cpu,
    is_gstreamer_gif_available,
    is_gl_available,
};

/// Check if GPU-accelerated GIF decoding is available.
///
/// Returns `true` if GStreamer with GL support is available for
/// hardware-accelerated GIF processing.
pub fn is_gpu_acceleration_available() -> bool {
    #[cfg(feature = "desktop-media")]
    { is_gstreamer_gif_available() && is_gl_available() }
    #[cfg(not(feature = "desktop-media"))]
    { false }
}
