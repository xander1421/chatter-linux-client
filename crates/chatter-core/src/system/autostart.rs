//! Autostart.
//!
//! Cross-platform autostart integration:
//! - Linux: XDG .desktop files in ~/.config/autostart/
//! - Windows: Registry entry in HKCU\Software\Microsoft\Windows\CurrentVersion\Run

#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::path::PathBuf;

use crate::error::SystemError;
#[cfg(target_os = "linux")]
use super::platform::APP_BINARY;
use super::platform::APP_ID;
#[cfg(target_os = "linux")]
use super::platform::APP_DISPLAY_NAME;

/// Manager for autostart entries.
///
/// Controls whether Chatter starts automatically when the user logs in.
///
/// # Platform behavior
///
/// - **Linux**: Creates/removes `.desktop` file in `~/.config/autostart/`
/// - **Windows**: Creates/removes registry entry in `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
pub struct AutostartManager {
    /// Path to the autostart directory (Linux only).
    #[cfg(target_os = "linux")]
    autostart_dir: PathBuf,
}

impl AutostartManager {
    /// Creates a new autostart manager.
    pub fn new() -> Self {
        #[cfg(target_os = "linux")]
        {
            let config_dir = dirs::config_dir().unwrap_or_else(|| {
                dirs::home_dir()
                    .map(|h| h.join(".config"))
                    .unwrap_or_else(|| PathBuf::from("/tmp"))
            });

            Self {
                autostart_dir: config_dir.join("autostart"),
            }
        }

        #[cfg(not(target_os = "linux"))]
        {
            Self {}
        }
    }

    /// Checks if autostart is currently enabled.
    pub fn is_enabled(&self) -> Result<bool, SystemError> {
        #[cfg(target_os = "linux")]
        {
            let path = self.autostart_dir.join(format!("{}.desktop", APP_ID));
            Ok(path.exists())
        }

        #[cfg(target_os = "windows")]
        {
            use winreg::enums::*;
            use winreg::RegKey;

            let hkcu = RegKey::predef(HKEY_CURRENT_USER);
            match hkcu.open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run") {
                Ok(key) => {
                    let result: Result<String, _> = key.get_value(APP_ID);
                    Ok(result.is_ok())
                }
                Err(_) => Ok(false),
            }
        }

        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            Ok(false)
        }
    }

    /// Enables autostart.
    pub fn enable(&self) -> Result<(), SystemError> {
        #[cfg(target_os = "linux")]
        {
            if !self.autostart_dir.exists() {
                fs::create_dir_all(&self.autostart_dir).map_err(|e| {
                    SystemError::Autostart(format!(
                        "Failed to create autostart directory {}: {}",
                        self.autostart_dir.display(),
                        e
                    ))
                })?;
            }

            let desktop_content = format!(
                "[Desktop Entry]\n\
                Type=Application\n\
                Name={}\n\
                Comment=Secure messaging client\n\
                Exec={}\n\
                Icon={}\n\
                Terminal=false\n\
                Categories=Network;InstantMessaging;\n\
                X-GNOME-Autostart-enabled=true\n",
                APP_DISPLAY_NAME, APP_BINARY, APP_ID
            );

            let path = self.autostart_dir.join(format!("{}.desktop", APP_ID));
            fs::write(&path, desktop_content).map_err(|e| {
                SystemError::Autostart(format!(
                    "Failed to write autostart file {}: {}",
                    path.display(),
                    e
                ))
            })?;

            Ok(())
        }

        #[cfg(target_os = "windows")]
        {
            use winreg::enums::*;
            use winreg::RegKey;

            let hkcu = RegKey::predef(HKEY_CURRENT_USER);
            let (key, _) = hkcu
                .create_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
                .map_err(|e| {
                    SystemError::Autostart(format!("Failed to open registry key: {}", e))
                })?;

            // Get the current executable path
            let exe_path = std::env::current_exe()
                .map_err(|e| {
                    SystemError::Autostart(format!("Failed to get executable path: {}", e))
                })?
                .to_string_lossy()
                .to_string();

            key.set_value(APP_ID, &exe_path).map_err(|e| {
                SystemError::Autostart(format!("Failed to set registry value: {}", e))
            })?;

            Ok(())
        }

        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            Err(SystemError::Autostart(
                "Autostart not supported on this platform".to_string(),
            ))
        }
    }

    /// Disables autostart.
    pub fn disable(&self) -> Result<(), SystemError> {
        #[cfg(target_os = "linux")]
        {
            let path = self.autostart_dir.join(format!("{}.desktop", APP_ID));
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

        #[cfg(target_os = "windows")]
        {
            use winreg::enums::*;
            use winreg::RegKey;

            let hkcu = RegKey::predef(HKEY_CURRENT_USER);
            if let Ok(key) =
                hkcu.open_subkey_with_flags("Software\\Microsoft\\Windows\\CurrentVersion\\Run", KEY_WRITE)
            {
                // Ignore error if value doesn't exist
                let _ = key.delete_value(APP_ID);
            }
            Ok(())
        }

        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            Ok(())
        }
    }

    /// Toggles autostart and returns the new state.
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

    #[cfg(target_os = "linux")]
    mod linux_tests {
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

            let path = manager.autostart_dir.join(format!("{}.desktop", APP_ID));
            assert!(path.exists());

            let content = fs::read_to_string(path).expect("Should read file");
            assert!(content.contains("[Desktop Entry]"));
            assert!(content.contains("Name=Chatter"));
        }

        #[test]
        fn test_disable_removes_desktop_file() {
            let (manager, _temp) = create_test_manager();

            manager.enable().expect("Should enable");
            assert!(manager.is_enabled().expect("Should be enabled"));

            manager.disable().expect("Should disable");
            assert!(!manager.is_enabled().expect("Should not be enabled"));
        }

        #[test]
        fn test_toggle() {
            let (manager, _temp) = create_test_manager();

            assert!(!manager.is_enabled().expect("Should not be enabled"));

            let state = manager.toggle().expect("Should toggle");
            assert!(state);
            assert!(manager.is_enabled().expect("Should be enabled"));

            let state = manager.toggle().expect("Should toggle");
            assert!(!state);
            assert!(!manager.is_enabled().expect("Should not be enabled"));
        }
    }
}
