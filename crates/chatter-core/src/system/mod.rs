//! OS integration layer.
//!
//! Cross-platform system integration features.

pub mod antidebug;
pub mod directories;
pub mod notification_sink;
pub mod platform;
pub mod sealed_secret;

// Desktop-only modules (not available on Android)
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod clipboard;
pub mod keyring;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod notifications;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod open;

// Linux-only modules
#[cfg(target_os = "linux")]
pub mod tray;
#[cfg(target_os = "windows")]
pub mod tray_windows;

pub use antidebug::{
    DebuggerStatus, check_debugger, init_protection as init_antidebug,
    periodic_check as antidebug_check,
};
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
pub use tray::{SystemTray, TrayHandle, TrayMessage, TrayStatus};

#[cfg(target_os = "windows")]
pub use tray_windows::{SystemTray, TrayHandle, TrayMessage, TrayStatus};

/// Name of the test-only opt-out from every audio device.
pub const NULL_AUDIO_ENV: &str = "CHATTER_NULL_AUDIO";

/// True when `CHATTER_NULL_AUDIO` is set: never open a microphone or speaker.
///
/// For test runs on a developer's machine. A call proof drives the real call
/// stack, and without this the capture path opened the owner's microphone and
/// the playback path their headphones (PipeWire showed the test binary as an
/// ALSA client). With it, capture hands LiveKit a source that is never fed,
/// playback mixes remote audio without an output stream, and voice recording,
/// the voice-note player and GStreamer audio refuse or go to a fakesink.
#[must_use]
pub fn null_audio() -> bool {
    std::env::var_os(NULL_AUDIO_ENV).is_some()
}
