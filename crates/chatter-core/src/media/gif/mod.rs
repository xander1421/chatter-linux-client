//! GIF frame decode.

pub mod animator;
#[cfg(feature = "desktop-gstreamer")]
pub mod gstreamer_decoder;
pub mod progressive;

pub use animator::GifFrame;
pub use progressive::decode_first_frame_only;

#[cfg(feature = "desktop-gstreamer")]
pub use gstreamer_decoder::{
    decode_all_frames_cpu, is_gl_available, is_gstreamer_gif_available,
};

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
