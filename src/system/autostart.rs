//! Autostart.
//!
//! XDG autostart integration for launching Chatter on login.
//! Creates and manages .desktop files in ~/.config/autostart/.

use std::fs;
use std::path::PathBuf;

use crate::error::SystemError;

/// The autostart .desktop file name.
const DESKTOP_FILE_NAME: &str = "chatter-linux.desktop";

/// The .desktop file content for autostart.
const DESKTOP_FILE_CONTENT: &str = r#"[Desktop Entry]
Type=Application
Name=Chatter
Comment=Secure messaging client
Exec=chatter-linux
Icon=chatter-linux
Terminal=false
Categories=Network;InstantMessaging;
X-GNOME-Autostart-enabled=true
"#;

/// Manager for XDG autostart entries.
///
/// Controls whether Chatter starts automatically when the user logs in
/// by creating or removing a .desktop file in ~/.config/autostart/.
///
/// # XDG Autostart Specification
///
/// On Linux, applications can register for automatic startup by placing
/// a .desktop file in `$XDG_CONFIG_HOME/autostart/` (typically `~/.config/autostart/`).
///
/// # Example
///
/// ```no_run
/// use chatter_linux::system::AutostartManager;
///
/// let autostart = AutostartManager::new();
///
/// // Check if autostart is enabled
/// if !autostart.is_enabled()? {
///     autostart.enable()?;
/// }
///
/// // Toggle autostart
/// let new_state = autostart.toggle()?;
/// println!("Autostart is now: {}", if new_state { "enabled" } else { "disabled" });
/// # Ok::<(), chatter_linux::error::SystemError>(())
/// ```
pub struct AutostartManager {
    /// Path to the autostart directory.
    autostart_dir: PathBuf,
}

impl AutostartManager {
    /// Creates a new autostart manager.
    ///
    /// Uses the XDG config directory to determine the autostart location.
    /// Typically this is `~/.config/autostart/`.
    pub fn new() -> Self {
        // Use dirs crate for config directory
        let config_dir = dirs::config_dir().unwrap_or_else(|| {
            // Fallback to home/.config
            dirs::home_dir()
                .map(|h| h.join(".config"))
                .unwrap_or_else(|| PathBuf::from("/tmp"))
        });

        Self {
            autostart_dir: config_dir.join("autostart"),
        }
    }

    /// Returns the path to the autostart .desktop file.
    fn desktop_file_path(&self) -> PathBuf {
        self.autostart_dir.join(DESKTOP_FILE_NAME)
    }

    /// Checks if autostart is currently enabled.
    ///
    /// # Returns
    ///
    /// `true` if the autostart .desktop file exists, `false` otherwise.
    ///
    /// # Errors
    ///
    /// Returns `SystemError::Autostart` if the check fails (e.g., permission error).
    pub fn is_enabled(&self) -> Result<bool, SystemError> {
        let path = self.desktop_file_path();
        Ok(path.exists())
    }

    /// Enables autostart by creating the .desktop file.
    ///
    /// Creates the autostart directory if it doesn't exist, then writes
    /// the .desktop file with the application configuration.
    ///
    /// # Errors
    ///
    /// Returns `SystemError::Autostart` if the file cannot be created.
    pub fn enable(&self) -> Result<(), SystemError> {
        // Create autostart directory if it doesn't exist
        if !self.autostart_dir.exists() {
            fs::create_dir_all(&self.autostart_dir).map_err(|e| {
                SystemError::Autostart(format!(
                    "Failed to create autostart directory {}: {}",
                    self.autostart_dir.display(),
                    e
                ))
            })?;
        }

        // Write the .desktop file
        let path = self.desktop_file_path();
        fs::write(&path, DESKTOP_FILE_CONTENT).map_err(|e| {
            SystemError::Autostart(format!(
                "Failed to write autostart file {}: {}",
                path.display(),
                e
            ))
        })?;

        Ok(())
    }

    /// Disables autostart by removing the .desktop file.
    ///
    /// If the file doesn't exist, this is a no-op.
    ///
    /// # Errors
    ///
    /// Returns `SystemError::Autostart` if the file exists but cannot be removed.
    pub fn disable(&self) -> Result<(), SystemError> {
        let path = self.desktop_file_path();

        if path.exists() {
            fs::remove_file(&path).map_err(|e| {
                SystemError::Autostart(format!(
                    "Failed to remove autostart file {}: {}",
                    path.display(),
                    e
                ))
            })?;
        }

        Ok(())
    }

    /// Toggles autostart and returns the new state.
    ///
    /// If autostart is currently enabled, disables it.
    /// If autostart is currently disabled, enables it.
    ///
    /// # Returns
    ///
    /// `true` if autostart is now enabled, `false` if now disabled.
    ///
    /// # Errors
    ///
    /// Returns `SystemError::Autostart` if the toggle operation fails.
    pub fn toggle(&self) -> Result<bool, SystemError> {
        if self.is_enabled()? {
            self.disable()?;
            Ok(false)
        } else {
            self.enable()?;
            Ok(true)
        }
    }
}

impl Default for AutostartManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_manager() -> (AutostartManager, TempDir) {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let autostart_dir = temp_dir.path().join("autostart");

        let manager = AutostartManager { autostart_dir };
        (manager, temp_dir)
    }

    #[test]
    fn test_is_enabled_when_not_exists() {
        let (manager, _temp) = create_test_manager();
        assert!(!manager.is_enabled().expect("Should check enabled"));
    }

    #[test]
    fn test_enable_creates_desktop_file() {
        let (manager, _temp) = create_test_manager();

        manager.enable().expect("Should enable");
        assert!(manager.is_enabled().expect("Should be enabled"));
        assert!(manager.desktop_file_path().exists());

        // Verify content
        let content =
            fs::read_to_string(manager.desktop_file_path()).expect("Should read file");
        assert!(content.contains("[Desktop Entry]"));
        assert!(content.contains("Name=Chatter"));
        assert!(content.contains("Exec=chatter-linux"));
    }

    #[test]
    fn test_disable_removes_desktop_file() {
        let (manager, _temp) = create_test_manager();

        manager.enable().expect("Should enable");
        assert!(manager.is_enabled().expect("Should be enabled"));

        manager.disable().expect("Should disable");
        assert!(!manager.is_enabled().expect("Should not be enabled"));
        assert!(!manager.desktop_file_path().exists());
    }

    #[test]
    fn test_disable_when_not_exists() {
        let (manager, _temp) = create_test_manager();

        // Should not error when file doesn't exist
        manager.disable().expect("Should disable (no-op)");
        assert!(!manager.is_enabled().expect("Should not be enabled"));
    }

    #[test]
    fn test_toggle() {
        let (manager, _temp) = create_test_manager();

        // Start disabled
        assert!(!manager.is_enabled().expect("Should not be enabled"));

        // Toggle to enabled
        let state = manager.toggle().expect("Should toggle");
        assert!(state);
        assert!(manager.is_enabled().expect("Should be enabled"));

        // Toggle to disabled
        let state = manager.toggle().expect("Should toggle");
        assert!(!state);
        assert!(!manager.is_enabled().expect("Should not be enabled"));
    }
}
