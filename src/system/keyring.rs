//! Keyring.
//!
//! Secure credential storage using system keyring (Secret Service on Linux).
//! Provides secure storage for sensitive data like database passphrases.

use crate::error::SystemError;

/// Service name for keyring entries.
const SERVICE: &str = "chatter-linux";

/// Username identifier for the database passphrase credential.
const USERNAME: &str = "database";

/// Secure credential storage using the system keyring.
///
/// Uses the Secret Service API (GNOME Keyring, KWallet) on Linux
/// to securely store credentials like the database passphrase.
///
/// # Example
///
/// ```ignore
/// use chatter_linux::system::Keyring;
///
/// let keyring = Keyring::new();
/// keyring.set_passphrase("my-secure-passphrase")?;
/// let passphrase = keyring.get_passphrase()?;
/// ```
pub struct Keyring;

impl Keyring {
    /// Creates a new Keyring instance.
    ///
    /// Note: This does not interact with the system keyring yet.
    /// Keyring operations are performed lazily when methods are called.
    pub fn new() -> Self {
        Self
    }

    /// Creates a new keyring Entry for the database passphrase.
    fn entry(&self) -> Result<keyring::Entry, SystemError> {
        keyring::Entry::new(SERVICE, USERNAME).map_err(map_keyring_error)
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
        self.entry()?.get_password().map_err(map_keyring_error)
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
        self.entry()?
            .set_password(passphrase)
            .map_err(map_keyring_error)
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

impl Default for Keyring {
    fn default() -> Self {
        Self::new()
    }
}

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
        let keyring = Keyring::new();
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
        let keyring = Keyring::new();

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
}
