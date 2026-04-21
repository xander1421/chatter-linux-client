//! Android platform integration (JNI).

pub mod keyboard;
pub mod notifications;

pub use keyboard::{query_display_insets, DisplayInsets};
pub use notifications::AndroidNotifications;
