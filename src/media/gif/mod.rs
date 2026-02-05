//! GIF support.
//!
//! GIF search, selection, and caching via Tenor API.

pub mod cache;
pub mod client;
pub mod picker;

// Re-exports for convenient access
pub use cache::GifCache;
pub use client::{GifClient, GifResult};
pub use picker::GifPicker;
