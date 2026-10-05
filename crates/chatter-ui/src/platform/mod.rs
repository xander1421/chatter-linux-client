//! Platform-specific integration code.
//!
//! Platform glue (JNI on Android, D-Bus on Linux for UI concerns, etc.) lives
//! here. `chatter-core` stays platform-neutral and pulls platform behaviour in
//! via trait objects this module injects at startup.

#[cfg(target_os = "android")]
pub mod android;

#[cfg(target_os = "macos")]
pub mod macos;

pub(crate) mod video_source;
