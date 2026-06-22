//! Android platform integration (JNI).
//!
//! Safe-area / keyboard insets used to live here under `keyboard.rs`, but
//! Slint's Android backend already feeds WindowInsets into the built-in
//! `Window.safe-area-insets` + `virtual-keyboard-size` out properties. The
//! .slint code binds those directly — no Rust polling required.

pub mod activity;
pub mod call_actions;
pub mod camera;
pub mod file_picker;
pub mod notifications;
pub mod permissions;

pub use notifications::AndroidNotifications;
