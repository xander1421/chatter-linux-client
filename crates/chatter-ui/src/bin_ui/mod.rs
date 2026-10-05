//! Binary-only UI modules.
//!
//! These modules are part of the binary crate and have access to
//! Slint-generated types from `slint::include_modules!()`.

pub mod audio_state;
pub mod converters;
pub mod decode_cache;
pub mod features;
pub mod gif_state;
pub mod haptics;
pub mod preview_state;
pub mod slint_conversions;
pub mod ui_helpers;
pub mod video;
pub mod video_state;
