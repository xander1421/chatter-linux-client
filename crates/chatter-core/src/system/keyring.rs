//! Keyring.
//!
//! Secure credential storage using system keyring (Secret Service on Linux).
//! Provides secure storage for sensitive data like database passphrases.
//!
//! The keyring entry is keyed by the database path, allowing multiple instances
//! (e.g., different XDG_DATA_HOME for testing) to have separate passphrases.

use crate::error::SystemError;

use super::platform::APP_ID;

/// Service name for keyring entries (platform-specific).
const SERVICE: &str = APP_ID;

/// Secure credential storage using the system keyring.
///
/// Uses the Secret Service API (GNOME Keyring, KWallet) on Linux
/// to securely store credentials like the database passphrase.
///
/// Each keyring instance is tied to a specific database path, allowing
/// multiple database instances to have separate passphrases (useful for
/// testing with different XDG_DATA_HOME settings).
///
/// # Example
///
/// ```ignore
/// use chatter_core::system::Keyring;
/// use std::path::PathBuf;
///
/// let keyring = Keyring::new(PathBuf::from("/path/to/chatter.db"));
/// keyring.set_passphrase("my-secure-passphrase")?;
/// let passphrase = keyring.get_passphrase()?;
/// ```
pub struct Keyring {
    /// Username identifier derived from database path.
    username: String,
    /// Path to a local fallback file for the passphrase.
    local_path: std::path::PathBuf,
}

impl Keyring {
    pub fn new(db_path: std::path::PathBuf) -> Self {
        use sha2::{Digest, Sha256};

        // 1. Unique ID for this DB
        let path_str = db_path.to_string_lossy();
        let mut hasher = Sha256::new();
        hasher.update(path_str.as_bytes());
        let hash = hasher.finalize();
        let hash_hex = hex::encode(&hash[..8]);
        let username = format!("db:{}", hash_hex);

        // 2. Local fallback path (adjacent to DB but hidden)
        let mut local_path = db_path.clone();
        local_path.set_extension("passphrase");

        Self { username, local_path }
    }

    /// Creates a new keyring Entry for the database passphrase.
    fn entry(&self) -> Result<keyring::Entry, SystemError> {
        keyring::Entry::new(SERVICE, &self.username).map_err(map_keyring_error)
    }

    /// Retrieves the database passphrase.
    /// 
    /// Priority:
    /// 1. CHATTER_DB_PASSPHRASE env var
    /// 2. Local .passphrase file (Default)
    /// 3. System Keyring (Only if CHATTER_USE_KEYRING is set)
    pub fn get_passphrase(&self) -> Result<String, SystemError> {
        // 1. Env Var override
        if let Ok(passphrase) = std::env::var("CHATTER_DB_PASSPHRASE") {
            return Ok(passphrase);
        }

        // 2. Check for System Keyring Opt-in
        if std::env::var("CHATTER_USE_KEYRING").is_ok() {
            tracing::debug!("CHATTER_USE_KEYRING is set, accessing system keyring");
            return self.entry()?.get_password().map_err(map_keyring_error);
        }

        // 3. Default: Local file
        if self.local_path.exists() {
            let pass = std::fs::read_to_string(&self.local_path)
                .map_err(|e| SystemError::Keyring(format!("Failed to read local passphrase: {}", e)))?;
            return Ok(pass.trim().to_string());
        }

        Err(SystemError::Keyring("No passphrase found locally".to_string()))
    }

    pub fn get_passphrase_with_retry(
        &self,
        max_retries: u32,
        retry_delay_secs: u64,
    ) -> Result<String, SystemError> {
        // If not using keyring, retry is irrelevant
        if std::env::var("CHATTER_USE_KEYRING").is_err() {
            return self.get_passphrase();
        }

        // Keyring-specific retry logic
        tracing::info!("Accessing system keyring... (please unlock if prompted)");
        for attempt in 0..=max_retries {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_secs(retry_delay_secs));
            }

            match self.entry() {
                Ok(entry) => match entry.get_password() {
                    Ok(p) => return Ok(p),
                    Err(keyring::Error::NoEntry) => return Err(SystemError::Keyring("Not found".into())),
                    Err(_) => continue,
                },
                Err(e) => return Err(e),
            }
        }
        Err(SystemError::Keyring("Timeout".into()))
    }

    pub fn set_passphrase(&self, passphrase: &str) -> Result<(), SystemError> {
        if std::env::var("CHATTER_DB_PASSPHRASE").is_ok() {
            return Ok(());
        }

        // 1. If Opt-in to Keyring
        if std::env::var("CHATTER_USE_KEYRING").is_ok() {
            return self.entry()?
                .set_password(passphrase)
                .map_err(map_keyring_error);
        }

        // 2. Default: Store in local file with restricted permissions
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600) // Owner Read/Write only
                .open(&self.local_path)
                .map_err(|e| SystemError::Keyring(e.to_string()))?;
            
            use std::io::Write;
            file.write_all(passphrase.as_bytes())
                .map_err(|e| SystemError::Keyring(e.to_string()))?;
        }

        #[cfg(not(unix))]
        {
            std::fs::write(&self.local_path, passphrase)
                .map_err(|e| SystemError::Keyring(e.to_string()))?;
        }

        Ok(())
    }

    pub fn set_passphrase_with_retry(
        &self,
        passphrase: &str,
        max_retries: u32,
        retry_delay_secs: u64,
    ) -> Result<(), SystemError> {
        if std::env::var("CHATTER_USE_KEYRING").is_err() {
            return self.set_passphrase(passphrase);
        }

        // Keyring-specific retry logic
        for attempt in 0..=max_retries {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_secs(retry_delay_secs));
            }
            if let Ok(entry) = self.entry() {
                if entry.set_password(passphrase).is_ok() {
                    return Ok(());
                }
            }
        }
        Err(SystemError::Keyring("Failed to store in keyring".into()))
    }

    /// Removes the database passphrase from the keyring.
    ///
    /// Use this during logout or account reset.
    ///
    /// # Errors
    ///
    /// Returns `SystemError::Keyring` if:
    /// - The passphrase doesn't exist
    /// - The keyring service is unavailable
    /// - Access is denied
    pub fn delete_passphrase(&self) -> Result<(), SystemError> {
        self.entry()?
            .delete_credential()
            .map_err(map_keyring_error)
    }

    /// Checks if a passphrase exists in the keyring or environment.
    ///
    /// Returns `true` if a passphrase is stored or CHATTER_DB_PASSPHRASE is set.
    pub fn has_passphrase(&self) -> bool {
        // Check env var first
        if std::env::var("CHATTER_DB_PASSPHRASE").is_ok() {
            return true;
        }
        self.get_passphrase().is_ok()
    }
}

// Note: No Default impl - Keyring requires a database path

/// Maps keyring crate errors to SystemError.
fn map_keyring_error(e: keyring::Error) -> SystemError {
    SystemError::Keyring(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: These tests require a running Secret Service (GNOME Keyring/KWallet).
    // They may fail in CI environments without a keyring daemon.
    // Mark with #[ignore] if running in headless CI.

    #[test]
    fn test_keyring_roundtrip_local() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let keyring = Keyring::new(db_path);
        let test_pass = "test-passphrase-local";

        // Should start empty
        assert!(keyring.get_passphrase().is_err());

        // Set and get (defaults to local file)
        keyring.set_passphrase(test_pass).expect("Should set");
        let retrieved = keyring.get_passphrase().expect("Should get");
        assert_eq!(retrieved, test_pass);
        
        // Verify file exists
        assert!(keyring.local_path.exists());
    }

    #[test]
    fn test_keyring_unique_per_path() {
        // Different paths should produce different usernames and local_paths
        let keyring1 = Keyring::new(std::path::PathBuf::from("/tmp/user1/chatter.db"));
        let keyring2 = Keyring::new(std::path::PathBuf::from("/tmp/user2/chatter.db"));

        assert_ne!(keyring1.username, keyring2.username);
        assert_ne!(keyring1.local_path, keyring2.local_path);
    }
}
