//! OS integration layer.
//!
//! Linux system integration features.

pub mod autostart;
pub mod clipboard;
pub mod dbus;
pub mod directories;
pub mod keyring;
pub mod notifications;
pub mod open;
pub mod tray;

pub use directories::Directories;
pub use keyring::Keyring;
