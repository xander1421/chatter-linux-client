//! OS integration layer.
//!
//! Cross-platform system integration features.

pub mod antidebug;
pub mod autostart;
pub mod clipboard;
pub mod directories;
pub mod keyring;
pub mod notifications;
pub mod open;
pub mod platform;

// Linux-only modules
#[cfg(target_os = "linux")]
pub mod dbus;
#[cfg(target_os = "linux")]
pub mod tray;
#[cfg(target_os = "windows")]
pub mod tray_windows;

pub use antidebug::{check_debugger, init_protection as init_antidebug, periodic_check as antidebug_check, DebuggerStatus};
pub use autostart::AutostartManager;
pub use clipboard::{Clipboard, ImageData};
pub use directories::Directories;
pub use keyring::Keyring;
pub use notifications::DesktopNotifications;
pub use open::ExternalOpener;

#[cfg(target_os = "linux")]
pub use dbus::DbusClient;
#[cfg(target_os = "linux")]
pub use tray::{SystemTray, TrayHandle, TrayMessage, TrayStatus};

#[cfg(target_os = "windows")]
pub use tray_windows::{SystemTray, TrayHandle, TrayMessage, TrayStatus};
