//! XDG directories.
//!
//! XDG-compliant directory management for the Chatter client.
//! Provides standardized paths for config, data, cache, and runtime files.
//!
//! # Security
//!
//! All directories are created with mode 0700 (owner read/write/execute only)
//! to prevent other users on shared systems from accessing sensitive data.

use std::path::{Path, PathBuf};

use directories::ProjectDirs;

use crate::error::SystemError;
use super::platform::APP_ID;

/// Create a directory with secure permissions (0700).
///
/// On Unix systems, this sets the directory to be readable/writable/executable
/// only by the owner. On other platforms, this just creates the directory.
fn create_secure_dir(path: &Path) -> Result<(), SystemError> {
    std::fs::create_dir_all(path).map_err(SystemError::DirectoryCreate)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(path, permissions).map_err(SystemError::DirectoryCreate)?;
    }

    Ok(())
}

/// XDG-compliant directories for the Chatter client.
///
/// Provides access to standard directory locations:
/// - Config: `~/.config/chatter-linux/`
/// - Data: `~/.local/share/chatter-linux/`
/// - Cache: `~/.cache/chatter-linux/`
/// - Runtime: `/run/user/{uid}/chatter-linux/` (optional)
#[derive(Clone)]
pub struct Directories {
    config_dir: PathBuf,
    data_dir: PathBuf,
    cache_dir: PathBuf,
    runtime_dir: Option<PathBuf>,
}

impl Directories {
    /// Creates a new Directories instance and ensures all directories exist.
    ///
    /// On Android, use [`Directories::with_base_dir`] instead — XDG directories
    /// are not available and the app must provide its files directory.
    ///
    /// # Errors
    ///
    /// Returns `SystemError::DirectoryInit` if XDG directories cannot be determined.
    /// Returns `SystemError::DirectoryCreate` if directories cannot be created.
    pub fn new() -> Result<Self, SystemError> {
        let project_dirs = ProjectDirs::from("com", "chatter", APP_ID)
            .ok_or(SystemError::DirectoryInit)?;

        // `directories` only reads XDG_DATA_HOME on its Linux backend — macOS
        // and Windows ignore it and always resolve the platform-native path.
        // Honor it explicitly so `XDG_DATA_HOME=/tmp/chatter-userN` (used for
        // multi-user local testing) works the same way on every platform.
        let data_dir = std::env::var_os("XDG_DATA_HOME")
            .map(|dir| PathBuf::from(dir).join(APP_ID))
            .unwrap_or_else(|| project_dirs.data_dir().to_path_buf());

        Self::init(
            project_dirs.config_dir().to_path_buf(),
            data_dir,
            project_dirs.cache_dir().to_path_buf(),
            project_dirs.runtime_dir().map(|p| p.to_path_buf()),
        )
    }

    /// Creates a Directories instance rooted at `base_dir`.
    ///
    /// Used on Android where `base_dir` is `context.filesDir`. Subdirectories
    /// `config/`, `data/`, and `cache/` are created under it.
    pub fn with_base_dir(base_dir: PathBuf) -> Result<Self, SystemError> {
        Self::init(
            base_dir.join("config"),
            base_dir.join("data"),
            base_dir.join("cache"),
            None,
        )
    }

    fn init(
        config_dir: PathBuf,
        data_dir: PathBuf,
        cache_dir: PathBuf,
        runtime_dir: Option<PathBuf>,
    ) -> Result<Self, SystemError> {
        create_secure_dir(&config_dir)?;
        create_secure_dir(&data_dir)?;
        create_secure_dir(&cache_dir)?;

        if let Some(ref runtime) = runtime_dir {
            let _ = create_secure_dir(runtime);
        }

        Ok(Self {
            config_dir,
            data_dir,
            cache_dir,
            runtime_dir,
        })
    }

    /// Returns the configuration directory path.
    ///
    /// Typically `~/.config/chatter-linux/` on Linux.
    /// Used for user preferences and settings files.
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// Returns the data directory path.
    ///
    /// Typically `~/.local/share/chatter-linux/` on Linux.
    /// Used for persistent data like the database.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Returns the cache directory path.
    ///
    /// Typically `~/.cache/chatter-linux/` on Linux.
    /// Used for temporary cached data that can be regenerated.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Returns the runtime directory path, if available.
    ///
    /// Typically `/run/user/{uid}/chatter-linux/` on Linux.
    /// Used for runtime files (sockets, pid files).
    /// May not be available on all systems.
    pub fn runtime_dir(&self) -> Option<&Path> {
        self.runtime_dir.as_deref()
    }

    /// Returns the database file path.
    ///
    /// Convenience method returning `{data_dir}/chatter.db`.
    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join("chatter.db")
    }

    /// Returns the logs directory path.
    ///
    /// Convenience method returning `{data_dir}/logs/`.
    /// Creates the directory with secure permissions (0700) if it doesn't exist.
    pub fn logs_dir(&self) -> PathBuf {
        let logs = self.data_dir.join("logs");
        // Best effort to create logs directory with secure permissions
        let _ = create_secure_dir(&logs);
        logs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_directories_new() {
        let dirs = Directories::new().expect("Should create directories");
        assert!(dirs.config_dir().exists());
        assert!(dirs.data_dir().exists());
        assert!(dirs.cache_dir().exists());
    }

    #[test]
    fn test_database_path() {
        let dirs = Directories::new().expect("Should create directories");
        let db_path = dirs.database_path();
        assert!(db_path.ends_with("chatter.db"));
    }

    #[test]
    fn test_logs_dir() {
        let dirs = Directories::new().expect("Should create directories");
        let logs = dirs.logs_dir();
        assert!(logs.exists());
        assert!(logs.ends_with("logs"));
    }
}
