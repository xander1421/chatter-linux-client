//! GIF frame decode.

pub mod animator;
#[cfg(feature = "desktop-gstreamer")]
pub mod gstreamer_decoder;
pub mod progressive;

pub use animator::GifFrame;
pub use progressive::decode_first_frame_only;

#[cfg(feature = "desktop-gstreamer")]
pub use gstreamer_decoder::{decode_all_frames_cpu, decode_frames_cpu_bounded, is_gl_available, is_gstreamer_gif_available};

/// Whether a GPU GIF decode path is available on this build.
pub fn is_gpu_acceleration_available() -> bool {
    #[cfg(feature = "desktop-gstreamer")]
    {
        is_gstreamer_gif_available() && is_gl_available()
    }
    #[cfg(not(feature = "desktop-gstreamer"))]
    {
        false
    }
}

/// Memory budget for one decoded UI GIF animation.
pub const MAX_UI_GIF_BYTES: usize = 48 * 1024 * 1024;

/// Whether another RGBA frame fits the animation's frame and byte budget.
pub fn frame_fits_budget(
    frame_count: usize,
    used_bytes: usize,
    frame_bytes: usize,
    max_frames: usize,
    max_bytes: usize,
) -> bool {
    frame_count < max_frames && frame_bytes <= max_bytes.saturating_sub(used_bytes)
}

#[cfg(test)]
mod budget_tests {
    use super::frame_fits_budget;

    #[test]
    fn stops_before_crossing_either_budget() {
        assert!(frame_fits_budget(1, 20, 30, 2, 50));
        assert!(!frame_fits_budget(2, 20, 1, 2, 50));
        assert!(!frame_fits_budget(1, 20, 31, 2, 50));
        assert!(!frame_fits_budget(0, usize::MAX, 1, 2, 50));
    }
}
