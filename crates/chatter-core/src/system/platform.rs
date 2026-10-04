//! Platform constants.
//!
//! Centralized app identity strings that vary by target OS.

/// Application ID used for system integration (keyring service, directories).
///
/// - Linux: `chatter-linux`
/// - Windows: `chatter-windows`
/// - macOS: `chatter-macos`
#[cfg(target_os = "linux")]
pub const APP_ID: &str = "chatter-linux";

#[cfg(target_os = "windows")]
pub const APP_ID: &str = "chatter-windows";

#[cfg(target_os = "macos")]
pub const APP_ID: &str = "chatter-macos";

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
pub const APP_ID: &str = "chatter";

/// Human-readable display name shown in notifications, tray tooltips, etc.
pub const APP_DISPLAY_NAME: &str = "Chatter";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constants_not_empty() {
        assert!(!APP_ID.is_empty());
        assert!(!APP_DISPLAY_NAME.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_linux_identity() {
        assert_eq!(APP_ID, "chatter-linux");
    }
}
