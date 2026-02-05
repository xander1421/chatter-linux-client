//! XDG directories.
//!
//! XDG-compliant directory management for the Chatter client.
//! Provides standardized paths for config, data, cache, and runtime files.

use std::path::{Path, PathBuf};

use directories::ProjectDirs;

use crate::error::SystemError;

/// XDG-compliant directories for the Chatter client.
///
/// Provides access to standard directory locations:
/// - Config: `~/.config/chatter-linux/`
/// - Data: `~/.local/share/chatter-linux/`
/// - Cache: `~/.cache/chatter-linux/`
/// - Runtime: `/run/user/{uid}/chatter-linux/` (optional)
pub struct Directories {
    config_dir: PathBuf,
    data_dir: PathBuf,
    cache_dir: PathBuf,
    runtime_dir: Option<PathBuf>,
}

impl Directories {
    /// Creates a new Directories instance and ensures all directories exist.
    ///
    /// # Errors
    ///
    /// Returns `SystemError::DirectoryInit` if XDG directories cannot be determined.
    /// Returns `SystemError::DirectoryCreate` if directories cannot be created.
    pub fn new() -> Result<Self, SystemError> {
        let project_dirs = ProjectDirs::from("com", "chatter", "chatter-linux")
            .ok_or(SystemError::DirectoryInit)?;

        let config_dir = project_dirs.config_dir().to_path_buf();
        let data_dir = project_dirs.data_dir().to_path_buf();
        let cache_dir = project_dirs.cache_dir().to_path_buf();
        let runtime_dir = project_dirs.runtime_dir().map(|p| p.to_path_buf());

        // Create directories if they don't exist
        std::fs::create_dir_all(&config_dir).map_err(SystemError::DirectoryCreate)?;
        std::fs::create_dir_all(&data_dir).map_err(SystemError::DirectoryCreate)?;
        std::fs::create_dir_all(&cache_dir).map_err(SystemError::DirectoryCreate)?;

        // Runtime directory is optional (may not exist on all systems)
        if let Some(ref runtime) = runtime_dir {
            // Ignore errors for runtime dir - it's optional
            let _ = std::fs::create_dir_all(runtime);
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
    /// Creates the directory if it doesn't exist.
    pub fn logs_dir(&self) -> PathBuf {
        let logs = self.data_dir.join("logs");
        // Best effort to create logs directory
        let _ = std::fs::create_dir_all(&logs);
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
