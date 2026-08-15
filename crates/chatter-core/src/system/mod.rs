//! OS integration layer.
//!
//! Cross-platform system integration features.

pub mod antidebug;
pub mod directories;
pub mod notification_sink;
pub mod platform;

// Desktop-only modules (not available on Android)
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod autostart;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod clipboard;
pub mod keyring;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod notifications;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod open;

// Linux-only modules
#[cfg(target_os = "linux")]
pub mod dbus;
#[cfg(target_os = "linux")]
pub mod tray;
#[cfg(target_os = "windows")]
pub mod tray_windows;

pub use antidebug::{check_debugger, init_protection as init_antidebug, periodic_check as antidebug_check, DebuggerStatus};
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub use autostart::AutostartManager;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub use clipboard::{Clipboard, ImageData};
pub use directories::Directories;
pub use keyring::Keyring;
pub use notification_sink::{NoopNotificationSink, NotificationSink};
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub use notifications::DesktopNotifications;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub use open::ExternalOpener;

#[cfg(target_os = "linux")]
pub use dbus::DbusClient;
#[cfg(target_os = "linux")]
pub use tray::{SystemTray, TrayHandle, TrayMessage, TrayStatus};

#[cfg(target_os = "windows")]
pub use tray_windows::{SystemTray, TrayHandle, TrayMessage, TrayStatus};
