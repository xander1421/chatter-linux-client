//! At-rest secrets wrapped by a hardware-backed platform key (Android Keystore).
//!
//! Android has no OS keyring the `keyring` crate can reach, so the SQLCipher
//! passphrase (`app/infrastructure.rs`) and the mesh seal key
//! (`storage/mesh_store.rs`) live in files in the app data directory. Those
//! files hold only a blob sealed under a non-exportable AES-256-GCM Keystore
//! key (Kotlin `KeyStoreHelper`), so a copy of the app's files does not contain
//! the key that opens the database — Signal-Android's `DatabaseSecretProvider`
//! design.
//!
//! Core cannot reach JNI; chatter-ui installs the [`KeyWrap`] at startup
//! (`platform/android/keystore.rs`), as it does the platform camera. With none
//! installed every read and write fails: there is no plaintext fallback.
//!
//! File format: `ks1:` + base64(platform blob). The one other content accepted
//! is the pre-Keystore plaintext (64 hex chars): it is sealed and the file
//! replaced in place on first read, as Signal migrated its own unencrypted
//! secret (`DatabaseSecretProvider.getUnencryptedDatabaseSecret`).

use std::path::Path;
use std::sync::OnceLock;

use base64::Engine as _;
use zeroize::Zeroizing;

/// Keystore alias for the SQLCipher passphrase.
pub const DB_ALIAS: &str = "chatter-db";
/// Keystore alias for the mesh seal key.
pub const MESH_SEAL_ALIAS: &str = "chatter-mesh-seal";

const PREFIX: &str = "ks1:";

/// Seals small secrets under a key the platform will not export.
pub trait KeyWrap: Send + Sync {
    fn seal(&self, alias: &str, plaintext: &[u8]) -> Result<Vec<u8>, String>;
    /// Must fail — never return garbage — when the blob does not authenticate.
    fn unseal(&self, alias: &str, blob: &[u8]) -> Result<Vec<u8>, String>;
}

static KEY_WRAP: OnceLock<Box<dyn KeyWrap>> = OnceLock::new();

/// Register the platform key wrapper. Called once by chatter-ui at startup.
pub fn install_key_wrap(wrap: Box<dyn KeyWrap>) {
    if KEY_WRAP.set(wrap).is_err() {
        tracing::warn!("key wrap already installed");
    }
}

/// The installed wrapper, or why there is none.
pub fn installed_key_wrap() -> Result<&'static dyn KeyWrap, String> {
    KEY_WRAP
        .get()
        .map(|b| b.as_ref())
        .ok_or_else(|| "no platform key wrapper is installed; refusing to keep a secret in plaintext".to_string())
}

/// Read and unseal the secret in `path`. `Ok(None)` only when the file does
/// not exist. Any other failure — unreadable file, unknown format, a blob the
/// Keystore will not open — is an error, never an absence: treating it as
/// absent would generate a replacement over data the real key still opens.
///
/// A legacy plaintext file is sealed and replaced before returning.
pub fn read(path: &Path, alias: &str, wrap: &dyn KeyWrap) -> Result<Option<Zeroizing<String>>, String> {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => Zeroizing::new(c),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{} could not be read: {e}", path.display())),
    };
    let trimmed = contents.trim();

    if let Some(encoded) = trimmed.strip_prefix(PREFIX) {
        let blob = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| format!("{} is not a valid sealed secret: {e}", path.display()))?;
        let plain = Zeroizing::new(
            wrap.unseal(alias, &blob)
                .map_err(|e| format!("the Android Keystore could not unseal {}: {e}", path.display()))?,
        );
        let secret = std::str::from_utf8(&plain)
            .map_err(|_| format!("{} unsealed to non-text", path.display()))?;
        if !is_hex_secret(secret) {
            return Err(format!("{} unsealed to an unexpected secret shape", path.display()));
        }
        return Ok(Some(Zeroizing::new(secret.to_string())));
    }

    if is_hex_secret(trimmed) {
        // Pre-Keystore plaintext. Seal it and atomically replace the file.
        let secret = Zeroizing::new(trimmed.to_string());
        write(path, alias, wrap, &secret)?;
        tracing::warn!(path = %path.display(), "sealed a plaintext secret file under the Android Keystore");
        return Ok(Some(secret));
    }

    Err(format!("{} is neither a sealed secret nor a legacy key", path.display()))
}

/// Seal `secret` and write it to `path` atomically (temp file 0600, fsync,
/// rename), so a crash leaves either the old file or the new one.
pub fn write(path: &Path, alias: &str, wrap: &dyn KeyWrap, secret: &str) -> Result<(), String> {
    let blob = wrap
        .seal(alias, secret.as_bytes())
        .map_err(|e| format!("the Android Keystore could not seal a new secret: {e}"))?;
    let encoded = format!("{PREFIX}{}", base64::engine::general_purpose::STANDARD.encode(&blob));

    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{} could not be created: {e}", parent.display()))?;
    }
    write_private(&tmp, encoded.as_bytes()).map_err(|e| format!("{} could not be written: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{} could not be replaced: {e}", path.display()))?;
    if let Some(parent) = path.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

/// 32 bytes as 64 lowercase hex characters — the only secret shape stored.
fn is_hex_secret(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Test wrapper: XOR "encryption" plus an alias tag, enough to exercise the
    /// file format and the failure paths. Not a cipher.
    pub(crate) struct FakeWrap {
        pub fail_unseal: bool,
    }

    impl KeyWrap for FakeWrap {
        fn seal(&self, alias: &str, plaintext: &[u8]) -> Result<Vec<u8>, String> {
            let mut out = alias.as_bytes().to_vec();
            out.push(0);
            out.extend(plaintext.iter().map(|b| b ^ 0x5a));
            Ok(out)
        }
        fn unseal(&self, alias: &str, blob: &[u8]) -> Result<Vec<u8>, String> {
            if self.fail_unseal {
                return Err("keystore key invalidated".into());
            }
            let tag_len = alias.len() + 1;
            if blob.len() < tag_len || &blob[..alias.len()] != alias.as_bytes() || blob[alias.len()] != 0 {
                return Err("wrong key".into());
            }
            Ok(blob[tag_len..].iter().map(|b| b ^ 0x5a).collect())
        }
    }

    const OK: FakeWrap = FakeWrap { fail_unseal: false };
    const SECRET: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    #[test]
    fn missing_file_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(&dir.path().join("k"), DB_ALIAS, &OK).unwrap().is_none());
    }

    #[test]
    fn write_then_read_round_trips_and_file_holds_no_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k");
        write(&path, DB_ALIAS, &OK, SECRET).unwrap();
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.starts_with("ks1:"));
        assert!(!on_disk.contains(SECRET));
        assert_eq!(read(&path, DB_ALIAS, &OK).unwrap().unwrap().as_str(), SECRET);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(!dir.path().join("k.tmp").exists());
    }

    #[test]
    fn legacy_plaintext_is_sealed_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k");
        std::fs::write(&path, format!("{SECRET}\n")).unwrap();
        assert_eq!(read(&path, DB_ALIAS, &OK).unwrap().unwrap().as_str(), SECRET);
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.starts_with("ks1:") && !on_disk.contains(SECRET));
        assert_eq!(read(&path, DB_ALIAS, &OK).unwrap().unwrap().as_str(), SECRET);
    }

    #[test]
    fn unseal_failure_is_an_error_not_an_absence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k");
        write(&path, DB_ALIAS, &OK, SECRET).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(read(&path, DB_ALIAS, &FakeWrap { fail_unseal: true }).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before, "a failed unseal must not touch the file");
    }

    #[test]
    fn a_blob_for_another_alias_does_not_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k");
        write(&path, MESH_SEAL_ALIAS, &OK, SECRET).unwrap();
        assert!(read(&path, DB_ALIAS, &OK).is_err());
    }

    #[test]
    fn malformed_contents_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k");
        for bad in ["ks1:!!notbase64", "ks2:AAAA", "short", "", &SECRET.to_uppercase(), &SECRET[..62]] {
            std::fs::write(&path, bad).unwrap();
            assert!(read(&path, DB_ALIAS, &OK).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn an_unsealed_value_of_the_wrong_shape_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k");
        write(&path, DB_ALIAS, &OK, "not-a-key").unwrap();
        assert!(read(&path, DB_ALIAS, &OK).is_err());
    }
}
