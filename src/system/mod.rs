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

pub use autostart::AutostartManager;
pub use clipboard::{Clipboard, ImageData};
pub use dbus::DbusClient;
pub use directories::Directories;
pub use keyring::Keyring;
pub use notifications::DesktopNotifications;
pub use open::ExternalOpener;
pub use tray::{SystemTray, TrayHandle, TrayMessage, TrayStatus};
