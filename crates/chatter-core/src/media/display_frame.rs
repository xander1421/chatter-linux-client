//! Presentation-sized pixels; capture/encoder and transport quality are untouched.
use std::sync::Arc;

use std::sync::atomic::{AtomicU64, Ordering};
static VIDEO_BOUNDS: AtomicU64 = AtomicU64::new(0);
/// Physical presentation surface, updated by the renderer when its window resizes.
pub fn set_video_display_bounds(width: u32, height: u32) {
    VIDEO_BOUNDS.store(((width as u64) << 32) | height as u64, Ordering::Relaxed);
}
pub fn video_display_bounds() -> (u32, u32) {
    let size = VIDEO_BOUNDS.load(Ordering::Relaxed);
    ((size >> 32) as u32, size as u32)
}
pub fn fit_dimensions(
    width: u32,
    height: u32,
    target_width: u32,
    target_height: u32,
    contain: bool,
) -> (u32, u32) {
    if width == 0 || height == 0 || target_width == 0 || target_height == 0 {
        return (width, height);
    }
    let x = target_width as f64 / width as f64;
    let y = target_height as f64 / height as f64;
    let scale = if contain { x.min(y) } else { x.max(y) }.min(1.0);
    (
        ((width as f64 * scale).ceil() as u32).max(1),
        ((height as f64 * scale).ceil() as u32).max(1),
    )
}

/// Fit a frame to physical display pixels without enlarging it. Cameras cover
/// their tile; screens contain the entire shared surface. Unknown layout keeps
/// the native frame, so opening/maximizing never selects an arbitrary quality cap.
pub fn rgba_for_display(
    rgba: Arc<Vec<u8>>,
    width: u32,
    height: u32,
    target_width: u32,
    target_height: u32,
    contain: bool,
) -> (Arc<Vec<u8>>, u32, u32) {
    let (w, h) = fit_dimensions(width, height, target_width, target_height, contain);
    if w >= width || h >= height {
        return (rgba, width, height);
    }
    let Some(source) =
        image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(width, height, rgba.as_slice())
    else {
        return (rgba, width, height);
    };
    let resized = if contain {
        image::imageops::resize(&source, w, h, image::imageops::FilterType::Triangle)
    } else {
        image::imageops::thumbnail(&source, w, h)
    };
    (Arc::new(resized.into_raw()), w, h)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn display_fit_preserves_cover_contain_and_native_fallback() {
        let data = Arc::new(vec![20; 400 * 200 * 4]);
        let (same, w, h) = rgba_for_display(data.clone(), 400, 200, 0, 0, true);
        assert!(Arc::ptr_eq(&data, &same));
        assert_eq!((w, h), (400, 200));
        let (pixels, w, h) = rgba_for_display(data.clone(), 400, 200, 100, 100, true);
        assert_eq!((w, h, pixels.len()), (100, 50, 100 * 50 * 4));
        let (_, w, h) = rgba_for_display(data, 400, 200, 100, 100, false);
        assert_eq!((w, h), (200, 100));
    }
}
