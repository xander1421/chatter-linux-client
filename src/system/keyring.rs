//! Keyring.
//!
//! Secure credential storage using system keyring (Secret Service on Linux).
//! Provides secure storage for sensitive data like database passphrases.
//!
//! The keyring entry is keyed by the database path, allowing multiple instances
//! (e.g., different XDG_DATA_HOME for testing) to have separate passphrases.

use std::path::PathBuf;

use crate::error::SystemError;

/// Service name for keyring entries.
const SERVICE: &str = "chatter-linux";

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
/// use chatter_linux::system::Keyring;
/// use std::path::PathBuf;
///
/// let keyring = Keyring::new(PathBuf::from("/path/to/chatter.db"));
/// keyring.set_passphrase("my-secure-passphrase")?;
/// let passphrase = keyring.get_passphrase()?;
/// ```
pub struct Keyring {
    /// Username identifier derived from database path.
    /// Format: "db:{hash}" where hash is first 16 chars of SHA256(path).
    username: String,
}

impl Keyring {
    /// Creates a new Keyring instance for a specific database path.
    ///
    /// The database path is hashed to create a unique keyring entry,
    /// allowing multiple database instances to coexist with separate passphrases.
    pub fn new(db_path: PathBuf) -> Self {
        use sha2::{Digest, Sha256};

        // Hash the database path to create a unique, stable identifier
        let path_str = db_path.to_string_lossy();
        let mut hasher = Sha256::new();
        hasher.update(path_str.as_bytes());
        let hash = hasher.finalize();
        let hash_hex = hex::encode(&hash[..8]); // First 8 bytes = 16 hex chars

        let username = format!("db:{}", hash_hex);
        tracing::debug!(
            db_path = %path_str,
            keyring_username = %username,
            "Created keyring for database"
        );

        Self { username }
    }

    /// Creates a new keyring Entry for the database passphrase.
    fn entry(&self) -> Result<keyring::Entry, SystemError> {
        keyring::Entry::new(SERVICE, &self.username).map_err(map_keyring_error)
    }

    /// Retrieves the database passphrase from the keyring.
    ///
    /// # Errors
    ///
    /// Returns `SystemError::Keyring` if:
    /// - The passphrase doesn't exist
    /// - The keyring service is unavailable
    /// - Access is denied
    pub fn get_passphrase(&self) -> Result<String, SystemError> {
        tracing::debug!(username = %self.username, "Keyring: getting passphrase");
        self.entry()?.get_password().map_err(map_keyring_error)
    }

    /// Retrieves the database passphrase with retry logic.
    ///
    /// Retries up to `max_retries` times with `retry_delay_secs` between attempts,
    /// giving users time to unlock their password manager (e.g., KeePassXC).
    ///
    /// # Errors
    ///
    /// Returns the last error if all retries fail.
    pub fn get_passphrase_with_retry(
        &self,
        max_retries: u32,
        retry_delay_secs: u64,
    ) -> Result<String, SystemError> {
        let mut last_error = None;

        for attempt in 0..=max_retries {
            if attempt > 0 {
                tracing::info!(
                    attempt = attempt,
                    max_retries = max_retries,
                    "Keyring: retrying get_passphrase, please unlock your password manager..."
                );
                std::thread::sleep(std::time::Duration::from_secs(retry_delay_secs));
            }

            match self.get_passphrase() {
                Ok(passphrase) => return Ok(passphrase),
                Err(e) => {
                    tracing::debug!(
                        attempt = attempt,
                        error = %e,
                        "Keyring: get_passphrase attempt failed"
                    );
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| SystemError::Keyring("No attempts made".to_string())))
    }

    /// Stores the database passphrase in the keyring.
    ///
    /// # Errors
    ///
    /// Returns `SystemError::Keyring` if:
    /// - The keyring service is unavailable
    /// - Access is denied
    /// - Storage fails
    pub fn set_passphrase(&self, passphrase: &str) -> Result<(), SystemError> {
        tracing::info!(username = %self.username, "Keyring: storing passphrase");
        self.entry()?
            .set_password(passphrase)
            .map_err(map_keyring_error)?;
        tracing::info!(username = %self.username, "Keyring: passphrase stored successfully");
        Ok(())
    }

    /// Stores the database passphrase with retry logic.
    ///
    /// Retries up to `max_retries` times with `retry_delay_secs` between attempts,
    /// giving users time to unlock their password manager (e.g., KeePassXC).
    ///
    /// # Errors
    ///
    /// Returns the last error if all retries fail.
    pub fn set_passphrase_with_retry(
        &self,
        passphrase: &str,
        max_retries: u32,
        retry_delay_secs: u64,
    ) -> Result<(), SystemError> {
        let mut last_error = None;

        for attempt in 0..=max_retries {
            if attempt > 0 {
                tracing::info!(
                    attempt = attempt,
                    max_retries = max_retries,
                    "Keyring: retrying set_passphrase, please unlock your password manager..."
                );
                std::thread::sleep(std::time::Duration::from_secs(retry_delay_secs));
            }

            match self.set_passphrase(passphrase) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    tracing::debug!(
                        attempt = attempt,
                        error = %e,
                        "Keyring: set_passphrase attempt failed"
                    );
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| SystemError::Keyring("No attempts made".to_string())))
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

    /// Checks if a passphrase exists in the keyring.
    ///
    /// Returns `true` if a passphrase is stored, `false` otherwise.
    /// This is a convenience method that catches errors from `get_passphrase`.
    pub fn has_passphrase(&self) -> bool {
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
    #[ignore = "Requires running Secret Service"]
    fn test_keyring_roundtrip() {
        let keyring = Keyring::new(PathBuf::from("/tmp/test-chatter.db"));
        let test_pass = "test-passphrase-12345";

        // Clean up any existing test data
        let _ = keyring.delete_passphrase();

        // Set and get
        keyring
            .set_passphrase(test_pass)
            .expect("Should set passphrase");
        let retrieved = keyring.get_passphrase().expect("Should get passphrase");
        assert_eq!(retrieved, test_pass);

        // Cleanup
        keyring
            .delete_passphrase()
            .expect("Should delete passphrase");
        assert!(!keyring.has_passphrase());
    }

    #[test]
    #[ignore = "Requires running Secret Service"]
    fn test_keyring_has_passphrase() {
        let keyring = Keyring::new(PathBuf::from("/tmp/test-chatter-2.db"));

        // Clean up any existing test data
        let _ = keyring.delete_passphrase();

        // Should not have passphrase initially
        assert!(!keyring.has_passphrase());

        // Set passphrase
        keyring
            .set_passphrase("test-pass")
            .expect("Should set passphrase");
        assert!(keyring.has_passphrase());

        // Cleanup
        let _ = keyring.delete_passphrase();
    }

    #[test]
    fn test_keyring_unique_per_path() {
        // Different paths should produce different usernames
        let keyring1 = Keyring::new(PathBuf::from("/tmp/user1/chatter.db"));
        let keyring2 = Keyring::new(PathBuf::from("/tmp/user2/chatter.db"));

        assert_ne!(keyring1.username, keyring2.username);
    }

    #[test]
    fn test_keyring_same_path_same_username() {
        // Same path should produce same username
        let keyring1 = Keyring::new(PathBuf::from("/tmp/test/chatter.db"));
        let keyring2 = Keyring::new(PathBuf::from("/tmp/test/chatter.db"));

        assert_eq!(keyring1.username, keyring2.username);
    }
}
