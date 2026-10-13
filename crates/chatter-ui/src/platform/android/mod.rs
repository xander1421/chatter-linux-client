//! Android platform integration (JNI).
//!
//! Safe-area / keyboard insets used to live here under `keyboard.rs`, but
//! Slint's Android backend already feeds WindowInsets into the built-in
//! `Window.safe-area-insets` + `virtual-keyboard-size` out properties. The
//! .slint code binds those directly — no Rust polling required.

pub mod activity;
#[cfg(feature = "call-ui")]
pub mod audio_route;
pub mod call_actions;
pub mod camera;
pub mod clipboard;
pub mod file_picker;
pub mod haptics;
pub mod keystore;
pub mod notification_route;
pub mod notifications;
pub mod permissions;
pub mod qr_scanner;
pub mod system_settings;
pub mod text_input;

pub use notifications::AndroidNotifications;

pub mod media;

pub mod media_export;

pub mod video_thumbnail;
