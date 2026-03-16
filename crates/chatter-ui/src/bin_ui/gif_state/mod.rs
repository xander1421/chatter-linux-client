//! GIF animation state.
//!
//! Manages animated GIF playback state for the UI thread.
//! Supports both the image viewer lightbox and inline message thumbnails.
//!
//! # Memory Bounds
//!
//! - Viewer: MAX_VIEWER_FRAMES (100) per animation
//! - Inline: MAX_INLINE_GIFS (20) with LRU eviction

mod picker;
mod viewer;
mod inline;

pub use picker::*;
pub use viewer::*;
pub use inline::*;

/// Maximum frames to store per GIF animation (viewer).
pub(crate) const MAX_VIEWER_FRAMES: usize = 100;

/// Maximum inline GIF animations to keep in memory.
pub(crate) const MAX_INLINE_GIFS: usize = 20;

/// Raw frame data that can be sent between threads.
pub(crate) type RawFrame = (Vec<u8>, u32, u32, u64); // (data, width, height, delay_ms)
