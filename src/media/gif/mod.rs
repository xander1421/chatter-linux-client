//! GIF support.
//!
//! GIF search, selection, caching via Tenor API, and animation playback.
//!
//! ## Progressive Loading
//!
//! For large GIFs, use `progressive::StreamingGifDecoder` to show the first
//! frame immediately while remaining frames decode in background.

pub mod animator;
pub mod cache;
pub mod client;
pub mod picker;
pub mod progressive;

// Re-exports for convenient access
pub use animator::{AnimatedGif, GifFrame, frame_to_slint_image};
pub use cache::GifCache;
pub use client::{GifClient, GifResult};
pub use picker::GifPicker;
pub use progressive::{decode_first_frame_only, StreamingGifDecoder};
