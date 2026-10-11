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

/// Name of the opt-out marker, looked for beside the database file.
pub const NO_KEYRING_MARKER: &str = "no-keyring";

/// Whether this machine has opted out of the OS keyring entirely.
///
/// True when `CHATTER_NO_KEYRING` is set, or when a `no-keyring` file sits beside
/// the database. The marker exists because the environment variable only helps
/// when whoever launched the process remembered to set it: a desktop entry, a bare
/// `cargo run`, or a debugger all start the app without it and land back on a
/// Secret Service prompt that nobody is present to answer. The marker travels with
/// the data directory instead, so the choice holds however the app is started.
///
/// # This is a deliberate weakening
///
/// With the keyring out of the picture the database passphrase and the mesh seal
/// key are plaintext files next to the database, mode 0600 — protected by file
/// permissions and nothing else. Anything that can read the user's home directory
/// can decrypt their message history. It is here for development and headless
/// testing on a box with no unlockable Secret Service. Delete the marker (and the
/// key files, if the material should be gone) to go back to the keyring.
#[must_use]
pub fn keyring_disabled(db_path: &std::path::Path) -> bool {
    if std::env::var("CHATTER_NO_KEYRING").is_ok() {
        return true;
    }
    db_path
        .parent()
        .is_some_and(|dir| dir.join(NO_KEYRING_MARKER).exists())
}

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
    /// Kept so the `no-keyring` marker can be looked for beside the database.
    db_path: std::path::PathBuf,
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

        Self {
            username,
            local_path,
            db_path,
        }
    }

    /// Creates a new keyring Entry for the database passphrase.
    fn entry(&self) -> Result<keyring::Entry, SystemError> {
        keyring::Entry::new(SERVICE, &self.username).map_err(map_keyring_error)
    }

    /// Retrieves the database passphrase.
    ///
    /// Priority:
    /// 1. `CHATTER_DB_PASSPHRASE` env var (explicit override, e.g. CI/headless)
    /// 2. Local `.passphrase` file IFF `CHATTER_DB_PASSPHRASE_FILE` is set
    /// 3. System Keyring (default — Secret Service / Keychain / Cred Manager)
    ///
    /// SECURITY: the keyring is the secure default. The local-file mode
    /// (`CHATTER_DB_PASSPHRASE_FILE=1`) is naked — the passphrase sits at
    /// `<db>.passphrase` mode 0600, so anything that can read the user's home
    /// (compromised browser, sync agent, backup tool, second user on a
    /// misconfigured box) recovers the SQLCipher key. Only opt into it on
    /// boxes with no Secret Service daemon (some minimal WMs, headless CI),
    /// and prefer `CHATTER_DB_PASSPHRASE` there instead.
    pub fn get_passphrase(&self) -> Result<String, SystemError> {
        if let Ok(passphrase) = std::env::var("CHATTER_DB_PASSPHRASE") {
            return Ok(passphrase);
        }

        if self.use_local_file() {
            return self.read_local_file();
        }

        tracing::debug!("accessing system keyring for DB passphrase");
        self.entry()?.get_password().map_err(map_keyring_error)
    }

    /// Whether the naked local-file passphrase mode is explicitly opted into.
    ///
    /// Either by `CHATTER_DB_PASSPHRASE_FILE` for a single run, or by the
    /// [`keyring_disabled`] marker, which persists the same choice for every
    /// launch however it happens.
    fn use_local_file(&self) -> bool {
        std::env::var("CHATTER_DB_PASSPHRASE_FILE").is_ok() || keyring_disabled(&self.db_path)
    }

    /// Read the on-disk default passphrase.
    fn read_local_file(&self) -> Result<String, SystemError> {
        if !self.local_path.exists() {
            return Err(SystemError::KeyringNoEntry);
        }
        let pass = std::fs::read_to_string(&self.local_path)
            .map_err(|e| SystemError::Keyring(format!("Failed to read local passphrase: {}", e)))?;
        Ok(pass.trim().to_string())
    }

    pub fn get_passphrase_with_retry(
        &self,
        max_retries: u32,
        retry_delay_secs: u64,
    ) -> Result<String, SystemError> {
        // Retry only matters for the keyring path (Secret Service unlock
        // prompt). Env var + local file are synchronous.
        if std::env::var("CHATTER_DB_PASSPHRASE").is_ok() || self.use_local_file() {
            return self.get_passphrase();
        }

        tracing::info!("Accessing system keyring... (please unlock if prompted)");
        for attempt in 0..=max_retries {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_secs(retry_delay_secs));
            }

            match self.entry() {
                Ok(entry) => match entry.get_password() {
                    Ok(p) => return Ok(p),
                    Err(keyring::Error::NoEntry) => return Err(SystemError::KeyringNoEntry),
                    // NoStorageAccess means the OS actually showed the
                    // interactive unlock prompt and the user denied/canceled
                    // it (or auth failed) — retrying re-triggers a brand new
                    // prompt instead of backing off, so a single "no" turns
                    // into up to `max_retries` more popups. Only retry the
                    // transient case this loop exists for: the backend
                    // itself being unavailable (e.g. Secret Service not up
                    // yet at login on Linux).
                    Err(e @ keyring::Error::NoStorageAccess(_)) => {
                        return Err(map_keyring_error(e));
                    }
                    Err(_) => continue,
                },
                Err(e) => return Err(e),
            }
        }
        Err(SystemError::Keyring("Timeout".into()))
    }

    /// Where [`get_passphrase`](Self::get_passphrase) would read from.
    ///
    /// Reported at startup because "retrieved existing passphrase" alone is the
    /// same sentence whether the keyring answered, an environment variable did, or
    /// a plaintext file beside the database did — and those have very different
    /// security properties. A log line that cannot distinguish them is how a
    /// machine silently running without its keyring looks identical to one using
    /// it.
    #[must_use]
    pub fn passphrase_source(&self) -> &'static str {
        if std::env::var("CHATTER_DB_PASSPHRASE").is_ok() {
            "CHATTER_DB_PASSPHRASE"
        } else if self.use_local_file() {
            "a plaintext file beside the database (keyring opted out)"
        } else {
            "the OS keyring"
        }
    }

    pub fn set_passphrase(&self, passphrase: &str) -> Result<(), SystemError> {
        if std::env::var("CHATTER_DB_PASSPHRASE").is_ok() {
            return Ok(());
        }

        if self.use_local_file() {
            return self.write_local_file(passphrase);
        }

        self.entry()?
            .set_password(passphrase)
            .map_err(map_keyring_error)
    }

    fn write_local_file(&self, passphrase: &str) -> Result<(), SystemError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
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
        // Retry only matters for the keyring path. Env var + local file
        // are synchronous.
        if std::env::var("CHATTER_DB_PASSPHRASE").is_ok() || self.use_local_file() {
            return self.set_passphrase(passphrase);
        }

        for attempt in 0..=max_retries {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_secs(retry_delay_secs));
            }
            if let Ok(entry) = self.entry() {
                match entry.set_password(passphrase) {
                    Ok(()) => return Ok(()),
                    // Same reasoning as get_passphrase_with_retry: a denied/
                    // canceled interactive prompt must not retry, or one
                    // "no" becomes several more popups.
                    Err(e @ keyring::Error::NoStorageAccess(_)) => {
                        return Err(map_keyring_error(e));
                    }
                    Err(_) => {}
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
        self.entry()?.delete_credential().map_err(map_keyring_error)
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

/// Test-only keyring backend that persists for the whole test process.
///
/// `keyring::mock` builds a fresh, empty credential for every `Entry::new`, so a
/// store written through one entry is invisible to the next: first-run passphrase
/// verification logs "FAILED to verify passphrase storage" and any fixture that
/// reopens its database gets `NoEntry`. Entries are keyed by service and user,
/// and the user is derived from the database path, so tests in distinct temp
/// directories never see each other's secrets.
#[cfg(test)]
pub(crate) fn test_credential_builder() -> Box<keyring::credential::CredentialBuilder> {
    use keyring::credential::{Credential, CredentialApi, CredentialBuilderApi};
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};

    type Store = Mutex<HashMap<(String, String), Vec<u8>>>;
    static STORE: LazyLock<Store> = LazyLock::new(Store::default);

    #[derive(Debug)]
    struct Cred(String, String);
    impl CredentialApi for Cred {
        fn set_secret(&self, secret: &[u8]) -> keyring::Result<()> {
            let key = (self.0.clone(), self.1.clone());
            STORE.lock().unwrap().insert(key, secret.to_vec());
            Ok(())
        }
        fn get_secret(&self) -> keyring::Result<Vec<u8>> {
            let key = (self.0.clone(), self.1.clone());
            STORE.lock().unwrap().get(&key).cloned().ok_or(keyring::Error::NoEntry)
        }
        fn delete_credential(&self) -> keyring::Result<()> {
            let key = (self.0.clone(), self.1.clone());
            STORE.lock().unwrap().remove(&key).map(drop).ok_or(keyring::Error::NoEntry)
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[derive(Debug)]
    struct Builder;
    impl CredentialBuilderApi for Builder {
        fn build(
            &self,
            _target: Option<&str>,
            service: &str,
            user: &str,
        ) -> keyring::Result<Box<Credential>> {
            Ok(Box::new(Cred(service.to_owned(), user.to_owned())))
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    Box::new(Builder)
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

        // Exercise the local-file path directly (private methods) so the test
        // is independent of the CHATTER_DB_PASSPHRASE_FILE env toggle and can't
        // race other tests mutating process env, nor touch the real keyring.
        assert!(keyring.read_local_file().is_err());

        keyring.write_local_file(test_pass).expect("Should write");
        let retrieved = keyring.read_local_file().expect("Should read");
        assert_eq!(retrieved, test_pass);

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

    #[test]
    fn test_credential_builder_persists_across_entries() {
        let builder = test_credential_builder();
        let user = format!("persist-{}", std::process::id());
        let write = builder.build(None, SERVICE, &user).unwrap();
        write.set_password("p").unwrap();
        let reopened = builder.build(None, SERVICE, &user).unwrap();
        assert_eq!(reopened.get_password().unwrap(), "p");
        reopened.delete_credential().unwrap();
        assert!(matches!(write.get_password(), Err(keyring::Error::NoEntry)));
    }
}
