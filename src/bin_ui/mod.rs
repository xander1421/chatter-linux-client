//! Binary-only UI modules.
//!
//! These modules are part of the binary crate and have access to
//! Slint-generated types from `slint::include_modules!()`.

pub mod audio_state;
pub mod converters;
pub mod features;
pub mod gif_state;
#[cfg(target_os = "linux")]
pub mod gl_video;
pub mod preview_state;
pub mod video_state;
