//! GIF animation state.
//!
//! Manages animated GIF playback state for the UI thread.
//! Supports both the image viewer lightbox and inline message thumbnails.
//!
//! # Memory Bounds
//!
//! - Viewer: MAX_VIEWER_FRAMES (100) per animation
//! - Inline: MAX_INLINE_GIFS (20) with LRU eviction

mod inline;
mod picker;
mod viewer;

pub use inline::*;
pub use picker::*;
pub use viewer::*;

/// Maximum frames to store per GIF animation (viewer).
pub(crate) const MAX_VIEWER_FRAMES: usize = 100;

/// Maximum inline GIF animations to keep in memory.
pub(crate) const MAX_INLINE_GIFS: usize = 20;
pub(crate) const MAX_INLINE_GIF_BYTES: usize = 24 << 20;
pub(crate) const INLINE_ANIMATION_BYTES: usize = 16 << 20;
pub(crate) const VIEWER_ANIMATION_BYTES: usize = 32 << 20;
// A GIF bubble is up to ~300 dp wide (~1000 px at 3.5x); 640 px keeps it
// sharp enough while a 16 MiB loop still holds ~10 frames at 4:3.
pub(crate) const INLINE_FRAME_PX: u32 = 640;
pub(crate) const VIEWER_FRAME_PX: u32 = 1280;

/// Resize a composited RGBA frame before it enters any UI animation state.
pub(crate) fn fit_frame(data: Vec<u8>, width: u32, height: u32, max_width: u32, max_height: u32) -> (Vec<u8>, u32, u32) {
    if width <= max_width && height <= max_height {
        return (data, width, height);
    }
    let Some(source) = image::RgbaImage::from_raw(width, height, data) else {
        return (Vec::new(), 0, 0);
    };
    let resized = image::imageops::thumbnail(&source, max_width.max(1), max_height.max(1));
    let (width, height) = resized.dimensions();
    (resized.into_raw(), width, height)
}

/// Raw frame data that can be sent between threads.
pub(crate) type RawFrame = (Vec<u8>, u32, u32, u64); // (data, width, height, delay_ms)

#[cfg(test)]
mod tests {
    #[test]
    fn frame_fit_preserves_aspect_and_caps_pixels() {
        let (pixels, width, height) = super::fit_frame(vec![7; 400 * 200 * 4], 400, 200, 100, 100);
        assert_eq!((width, height), (100, 50));
        assert_eq!(pixels.len(), 100 * 50 * 4);
    }
}
