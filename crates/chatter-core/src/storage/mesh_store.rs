//! Persistence for the bitchat mesh store-and-forward stack.
//!
//! Two impls of seams `chatter-mesh-store` deliberately left open:
//!
//! - [`MeshStorage`] — [`Storage`] over the existing SQLCipher [`Database`],
//!   one row per [`StorageKey`] in the `mesh_store` table (migration V057).
//! - [`MeshSealer`] — [`Sealer`], XChaCha20-Poly1305 under a key that lives in
//!   its **own** OS-keyring entry, separate from the database passphrase.
//!
//! # Why seal at all, when the database is already encrypted at rest
//!
//! This looks redundant and is not. Both reasons are recorded here because the
//! obvious "simplification" is to delete the sealer:
//!
//! - **The database key is unlocked for as long as the app runs.** A memory
//!   dump, a debug build, or a backup tool that captures the database file
//!   while it is open all get plaintext out of SQLCipher. The seal key is a
//!   separate keyring entry and is not in that capture — retained private mail
//!   stays ciphertext even when the database does not.
//! - **It keeps the guarantee structural rather than behavioural.** [`Storage`]
//!   takes `&[u8]` that has already been sealed and has no way to seal anything
//!   itself, so no bug in this file can write plaintext even by accident. That
//!   property survives someone later swapping SQLCipher for another engine,
//!   which a "the database encrypts it for us" argument does not.
//!
//! # `Ok(None)` is not "could not read"
//!
//! The [`Storage`] contract draws a line this module has to hold: `Ok(None)`
//! means *there is genuinely nothing durable here*, and a caller acts on it by
//! starting empty and persisting over the top. If a row that exists but cannot
//! be read reported the same thing, the caller would cheerfully overwrite mail
//! it never saw — messages lost, with no error anywhere. So [`MeshStorage::load`]
//! produces `Ok(None)` from exactly one place: the statement returning no row.
//! Every other outcome — a database error, a column that is not a blob — is an
//! `Err`, and the caller stays un-loaded and retries later.

use std::path::Path;
use std::sync::Arc;

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use chatter_mesh_store::outbox::Sealer;
use chatter_mesh_store::{StoreError, Storage, StorageKey};
use chrono::Utc;
use rusqlite::params;
use zeroize::Zeroizing;

use crate::storage::Database;
use crate::system::platform::APP_ID;

/// XChaCha20-Poly1305 nonce width.
const NONCE_SIZE: usize = 24;

/// Seal key width.
const KEY_SIZE: usize = 32;

/// Platforms whose app-private directory is the key store: no reachable OS
/// keyring, and a per-app sandbox protecting the data directory.
const APP_SANDBOXED: bool = cfg!(target_os = "android");

/// Label for the keyring entry, used in error messages.
///
/// `StoreError::Storage`'s `key` field is `&'static str`, and the seal key is
/// not one of the [`StorageKey`] blobs, so it names itself.
const SEAL_KEY_LABEL: &str = "mesh-seal-key";

// ===========================================================================
// Storage
// ===========================================================================

/// [`Storage`] backed by the app's SQLCipher database.
///
/// Every value written here is already sealed by [`MeshSealer`]; see the module
/// documentation for why that is not redundant with SQLCipher.
pub struct MeshStorage {
    db: Arc<Database>,
}

impl MeshStorage {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }
}

/// Turn any database failure into the trait's error, preserving which operation
/// and which blob it was.
fn storage_error(
    operation: &'static str,
    key: &'static str,
    reason: impl std::fmt::Display,
) -> StoreError {
    StoreError::Storage {
        operation,
        key,
        reason: reason.to_string(),
    }
}

impl Storage for MeshStorage {
    fn load(&self, key: StorageKey) -> Result<Option<Vec<u8>>, StoreError> {
        let name = key.as_str();
        let conn = self
            .db
            .read_connection()
            .map_err(|e| storage_error("load", name, e))?;

        let mut stmt = conn
            .prepare("SELECT sealed FROM mesh_store WHERE store_key = ?1")
            .map_err(|e| storage_error("load", name, e))?;
        let mut rows = stmt
            .query(params![name])
            .map_err(|e| storage_error("load", name, e))?;

        // The row cursor is stepped explicitly rather than going through
        // `query_row`, so that "no row" and "a row we could not turn into
        // bytes" cannot share a return path. This is the whole point of the
        // module's `Ok(None)` note: absence is one arm and one arm only.
        let Some(row) = rows.next().map_err(|e| storage_error("load", name, e))? else {
            // Genuinely absent — a first run, or a run after a panic wipe.
            return Ok(None);
        };

        // The row exists. From here on, failing to read it is an error, never
        // an absence: the blob is there and the caller must not overwrite it.
        let sealed: Vec<u8> = row.get(0).map_err(|e| storage_error("load", name, e))?;
        Ok(Some(sealed))
    }

    fn save(&self, key: StorageKey, sealed: &[u8]) -> Result<(), StoreError> {
        let name = key.as_str();
        let now = Utc::now().to_rfc3339();
        let conn = self
            .db
            .connection()
            .map_err(|e| storage_error("save", name, e))?;

        conn.execute(
            "INSERT INTO mesh_store (store_key, sealed, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(store_key) DO UPDATE SET
                 sealed = excluded.sealed,
                 updated_at = excluded.updated_at",
            params![name, sealed, now],
        )
        .map_err(|e| storage_error("save", name, e))?;

        Ok(())
    }

    fn wipe(&self, key: StorageKey) -> Result<(), StoreError> {
        let name = key.as_str();
        let conn = self
            .db
            .connection()
            .map_err(|e| storage_error("wipe", name, e))?;

        conn.execute("DELETE FROM mesh_store WHERE store_key = ?1", params![name])
            .map_err(|e| storage_error("wipe", name, e))?;

        Ok(())
    }

    /// Panic wipe, as one statement.
    ///
    /// The default implementation loops over [`StorageKey::ALL`] and keeps going
    /// after a failure, because a wipe that stops halfway is the worst outcome
    /// for the one operation whose purpose is leaving nothing behind. A single
    /// `DELETE` is strictly better here: it cannot half-succeed, and it also
    /// removes rows for keys this build does not know about — which the per-key
    /// loop would silently leave on disk after a downgrade.
    fn wipe_all(&self) -> Result<(), StoreError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| storage_error("wipe_all", "all", e))?;

        conn.execute("DELETE FROM mesh_store", [])
            .map_err(|e| storage_error("wipe_all", "all", e))?;

        Ok(())
    }
}

/// The mesh's storage and sealer for the database at `db_path`, built together
/// because one decision needs both.
///
/// When the sealer had to generate its key, nothing that existed before it can
/// ever be opened: the key those rows were sealed under is gone — the Android
/// build kept it only in process memory — not merely locked. Left in place they
/// make the mesh refuse to start on every launch ("persisted mesh seal is
/// malformed") with no way out short of clearing app data, so they are wiped
/// here. Only mesh state goes: peer pins, outbox, courier pool, archive, bridge
/// dedup and prekeys. Mesh messages and the mesh identity live in other tables.
///
/// A key that exists and still fails to open a blob is untouched by this, and
/// fails closed where the blob is read.
pub fn sealed_store(db: Arc<Database>, db_path: &Path) -> (MeshStorage, MeshSealer) {
    let storage = MeshStorage::new(db);
    let sealer = MeshSealer::from_keyring(db_path);
    if sealer.key_is_new() {
        match storage.wipe_all() {
            Ok(()) => tracing::warn!(
                target: "mesh::store",
                "new mesh seal key: cleared mesh state sealed under a key that no longer exists"
            ),
            Err(error) => tracing::warn!(
                target: "mesh::store",
                %error,
                "new mesh seal key, but the unreadable mesh state could not be cleared"
            ),
        }
    }
    (storage, sealer)
}

// ===========================================================================
// Sealer
// ===========================================================================

/// [`Sealer`] using XChaCha20-Poly1305 under a key from the OS keyring.
///
/// # Nonces
///
/// A fresh 192-bit nonce is drawn from the OS CSPRNG for every seal and stored
/// in front of the ciphertext. Nothing derives a nonce from a counter, the
/// storage key name, or a clock: a counter resets when the process restarts or
/// the database is restored from backup, a key name recurs by definition, and a
/// coarse clock repeats — any of which would reuse a nonce under the same key
/// and hand an observer the XOR of two outboxes plus a forgery.
///
/// The extended-nonce variant is what makes "just draw a random one" safe.
/// Plain ChaCha20-Poly1305 has a 96-bit nonce, where random selection carries a
/// birthday bound that has to be argued about; at 192 bits there is nothing to
/// argue about, and the codebase already uses `XChaCha20Poly1305` elsewhere
/// (`crypto/dm_crypto.rs`) for the same reason.
pub struct MeshSealer {
    /// `Err` carries the reason the key could not be read.
    ///
    /// A sealer that exists but cannot seal is deliberate. The mesh needs its
    /// key before it can retain anything, but a locked keyring must not stop the
    /// *app* from starting — the mesh is one optional feature, and failing its
    /// construction used to abort initialization for the whole client, so a
    /// Secret Service that did not answer meant nobody could log in.
    ///
    /// Refusing here rather than at construction keeps the invariant that
    /// matters: an unreadable key never leads to a *new* key being generated
    /// over sealed data that is still perfectly good.
    cipher: Result<XChaCha20Poly1305, String>,
    /// True when no key existed and this run generated one. Anything already
    /// in `mesh_store` was then sealed under a key that exists nowhere and can
    /// never be opened; see [`Self::key_is_new`].
    key_is_new: bool,
}

impl MeshSealer {
    /// Build a sealer from raw key material.
    ///
    /// Public so a platform that holds the key some other way (an Android
    /// keystore bridge, a test) can inject it. [`Self::from_keyring`] is the
    /// path the desktop app takes.
    pub fn from_key(key: &[u8; KEY_SIZE]) -> Self {
        let key = Key::from(*key);
        Self {
            cipher: Ok(XChaCha20Poly1305::new(&key)),
            key_is_new: false,
        }
    }

    /// A sealer that cannot seal, carrying why.
    ///
    /// [`MeshService::start`](crate::services::mesh::MeshService::start) checks
    /// this before it touches the radio, so the mesh reports the reason instead
    /// of coming up and failing every write.
    #[must_use]
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            cipher: Err(reason.into()),
            key_is_new: false,
        }
    }

    /// Whether this sealer holds a key.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.cipher.is_ok()
    }

    /// Whether this run generated the key because none existed.
    ///
    /// Then every row already in `mesh_store` is unreadable by construction —
    /// its key is gone, not locked — and the caller wipes them rather than let
    /// the mesh refuse to start over data nobody can ever open. A key that
    /// exists and fails to open a blob is a different case (tampering or
    /// corruption) and still fails closed.
    #[must_use]
    pub fn key_is_new(&self) -> bool {
        self.key_is_new
    }

    /// A sealer under a key generated just now.
    fn generated(key: &[u8; KEY_SIZE]) -> Self {
        Self {
            key_is_new: true,
            ..Self::from_key(key)
        }
    }

    /// Why the key is missing, if it is.
    #[must_use]
    pub fn unavailable_reason(&self) -> Option<&str> {
        self.cipher.as_ref().err().map(String::as_str)
    }

    /// The cipher, or the storage error naming why there is not one.
    fn cipher(&self, operation: &'static str) -> Result<&XChaCha20Poly1305, StoreError> {
        self.cipher
            .as_ref()
            .map_err(|reason| storage_error(operation, SEAL_KEY_LABEL, reason))
    }

    /// Load the seal key from the OS keyring, generating it on first use.
    ///
    /// The entry is keyed by the database path, the same way
    /// [`crate::system::Keyring`] keys the SQLCipher passphrase, so a second
    /// database (a test profile under its own `XDG_DATA_HOME`, say) gets its own
    /// seal key instead of quietly sharing one.
    ///
    /// # Only a *missing* entry generates a key
    ///
    /// `NoEntry` means first run: generate, store, proceed. Any other keyring
    /// failure — the Secret Service is not running, the collection is locked,
    /// access was denied — is propagated. It must not fall through to
    /// generating a fresh key: the old sealed outbox would then be undecryptable
    /// forever, and the caller, seeing a working sealer, would overwrite it. A
    /// locked keyring is temporary; destroying the key is not.
    pub fn from_keyring(db_path: &Path) -> Self {
        // Infallible by design. Every way this can fail yields a sealer that
        // reports *why* it has no key, because the alternative — propagating an
        // error — aborted initialization for the whole client over one optional
        // feature. What must never happen is generating a *replacement* key: the
        // existing sealed outbox would become permanently unreadable while
        // everything looked like it had worked.
        match Self::acquire(db_path) {
            Ok(sealer) => sealer,
            Err(reason) => Self::unavailable(reason),
        }
    }

    /// The fallible half of [`from_keyring`](Self::from_keyring). `Err` carries a
    /// sentence for the user, not a type: every caller turns it into the same
    /// unavailable sealer.
    fn acquire(db_path: &Path) -> Result<Self, String> {
        Self::acquire_with(db_path, APP_SANDBOXED)
    }

    /// [`acquire`](Self::acquire) with the platform decision as a parameter, so
    /// the Android path is testable on the host.
    fn acquire_with(db_path: &Path, app_sandboxed: bool) -> Result<Self, String> {
        // Development / headless bypass, mirroring `CHATTER_DB_PASSPHRASE`.
        //
        // Without this the mesh cannot start on a box with no Secret Service, and
        // every dev run prompts for a keyring unlock. The key is a 64-char hex
        // string; anything else is refused rather than silently replaced, because
        // quietly generating a different key would make existing sealed blobs
        // undecryptable while looking like it worked.
        //
        // This is a plaintext key in the environment. It exists for development and
        // CI, exactly as the database passphrase override does, and must not be used
        // to hold a real user's key.
        if let Ok(hex_key) = std::env::var("CHATTER_MESH_SEAL_KEY") {
            let raw = Zeroizing::new(
                hex::decode(hex_key.trim())
                    .map_err(|e| format!("CHATTER_MESH_SEAL_KEY is not hex: {e}"))?,
            );
            let key: [u8; KEY_SIZE] = raw.as_slice().try_into().map_err(|_| {
                format!(
                    "CHATTER_MESH_SEAL_KEY must be {KEY_SIZE} bytes of hex, found {}",
                    raw.len()
                )
            })?;
            return Ok(Self::from_key(&key));
        }

        // The same opt-out the database passphrase honours, for the same reason:
        // an environment variable only helps when whoever started the process set
        // it, and an unattended run started any other way lands on a Secret
        // Service prompt with nobody there to answer it. See
        // `system::keyring::keyring_disabled` for what this gives up.
        //
        // Android has no OS keyring the `keyring` crate can reach: it silently
        // hands back its in-memory mock (keyring 3.6.3 lib.rs:301-309), so a key
        // generated there died with the process and the next launch read every
        // seal as malformed. The key lives in the app-private data directory
        // instead, protected by the per-app UID sandbox — the same place and the
        // same protection the database passphrase already has there
        // (`app/infrastructure.rs`, `.db_key`).
        if app_sandboxed || crate::system::keyring::keyring_disabled(db_path) {
            return Self::from_local_key_file(db_path);
        }

        let entry = keyring::Entry::new(APP_ID, &Self::entry_username(db_path))
            .map_err(|e| format!("the OS keyring could not be opened: {e}"))?;

        // Anywhere else, a keyring that keeps nothing past the process (a build
        // without a native backend) is refused outright. Sealing under a key it
        // would forget is exactly how the Android build lost its own pins.
        if entry
            .get_credential()
            .downcast_ref::<keyring::mock::MockCredential>()
            .is_some()
        {
            return Err("this build has no OS keyring that outlives the process, so a \
                        mesh seal key could not be kept. Set CHATTER_MESH_SEAL_KEY or \
                        CHATTER_NO_KEYRING for a development run."
                .to_string());
        }

        match entry.get_password() {
            Ok(encoded) => {
                let encoded = Zeroizing::new(encoded);
                let raw = Zeroizing::new(hex::decode(encoded.as_bytes()).map_err(|e| {
                    // A corrupt entry is not "absent". Regenerating here would
                    // throw away the key that the stored blobs were sealed with.
                    format!("the stored mesh seal key is not hex: {e}")
                })?);
                let key: [u8; KEY_SIZE] = raw.as_slice().try_into().map_err(|_| {
                    format!("expected {KEY_SIZE} key bytes, found {}", raw.len())
                })?;
                Ok(Self::from_key(&key))
            }
            Err(keyring::Error::NoEntry) => {
                // The one case that may generate: there is genuinely nothing
                // stored, so there is nothing to orphan.
                let key = Zeroizing::new(
                    random_key().map_err(|e| format!("no randomness for a new key: {e}"))?,
                );
                let encoded = Zeroizing::new(hex::encode(key.as_slice()));
                // Store before use. A key we sealed with but failed to persist
                // would read back as garbage after the next restart, so a failure
                // to write it has to mean "no sealer", not "sealer we cannot keep".
                entry
                    .set_password(&encoded)
                    .map_err(|e| format!("a new mesh seal key could not be stored: {e}"))?;
                Ok(Self::generated(&key))
            }
            // The keyring is there but would not answer: locked, denied, or no
            // Secret Service running. Emphatically not a reason to generate a
            // fresh key.
            Err(e) => Err(format!(
                "the mesh seal key could not be read from the OS keyring: {e}. \
                 The mesh stays off rather than generate a new key over sealed data. \
                 Unlock the keyring and restart, or set CHATTER_MESH_SEAL_KEY for a \
                 development run."
            )),
        }
    }

    /// Remove the seal key from the keyring.
    ///
    /// Logout and account reset. Everything in `mesh_store` becomes permanently
    /// unreadable, which is the point — call [`Storage::wipe_all`] alongside it
    /// so the unreadable rows do not linger.
    /// The seal key from a plaintext file beside the database, generating it on
    /// first use.
    ///
    /// Mirrors `acquire`'s keyring branch exactly in the one respect that matters:
    /// a key that exists is never replaced. A file that is present but unreadable
    /// or malformed is an error, not an absence — regenerating there would leave
    /// every already-sealed blob permanently undecryptable while appearing to
    /// have worked.
    ///
    /// Written 0600, and plaintext. That is the trade the marker file opts into.
    fn from_local_key_file(db_path: &Path) -> Result<Self, String> {
        let path = Self::local_key_path(db_path);

        match std::fs::read_to_string(&path) {
            Ok(encoded) => {
                let encoded = Zeroizing::new(encoded);
                let raw = Zeroizing::new(
                    hex::decode(encoded.trim().as_bytes())
                        .map_err(|e| format!("{} is not hex: {e}", path.display()))?,
                );
                let key: [u8; KEY_SIZE] = raw.as_slice().try_into().map_err(|_| {
                    format!(
                        "{} must hold {KEY_SIZE} key bytes, found {}",
                        path.display(),
                        raw.len()
                    )
                })?;
                Ok(Self::from_key(&key))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let key = Zeroizing::new(
                    random_key().map_err(|e| format!("no randomness for a new key: {e}"))?,
                );
                let encoded = Zeroizing::new(hex::encode(key.as_slice()));
                // Persisted before use, as the keyring branch does: a key sealed
                // with but never stored reads back as garbage after a restart.
                write_private(&path, encoded.as_bytes())
                    .map_err(|e| format!("the mesh seal key could not be saved: {e}"))?;
                Ok(Self::generated(&key))
            }
            Err(error) => Err(format!("{} could not be read: {error}", path.display())),
        }
    }

    /// Where the seal key lives when the keyring is opted out of.
    fn local_key_path(db_path: &Path) -> std::path::PathBuf {
        let mut path = db_path.to_path_buf();
        path.set_extension("mesh-seal-key");
        path
    }

    pub fn delete_from_keyring(db_path: &Path) -> Result<(), StoreError> {
        let entry = keyring::Entry::new(APP_ID, &Self::entry_username(db_path))
            .map_err(|e| storage_error("keyring open", SEAL_KEY_LABEL, e))?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(storage_error("keyring delete", SEAL_KEY_LABEL, e)),
        }
    }

    /// Keyring username for this database's seal key.
    ///
    /// The `mesh-seal:` prefix keeps it a distinct entry from the SQLCipher
    /// passphrase (`db:`) rather than a second reader of the same secret — a
    /// separate entry is exactly what makes the seal survive a capture of the
    /// open database.
    fn entry_username(db_path: &Path) -> String {
        use sha2::{Digest, Sha256};

        let mut hasher = Sha256::new();
        hasher.update(db_path.to_string_lossy().as_bytes());
        let digest = hasher.finalize();
        format!("mesh-seal:{}", hex::encode(&digest[..8]))
    }
}

/// 32 bytes from the OS CSPRNG.
fn random_key() -> Result<[u8; KEY_SIZE], StoreError> {
    let mut key = [0u8; KEY_SIZE];
    fill_random(&mut key)?;
    Ok(key)
}

/// Fill a buffer from the OS CSPRNG, reporting failure rather than panicking.
fn fill_random(buffer: &mut [u8]) -> Result<(), StoreError> {
    use rand::TryRngCore;

    rand::rngs::OsRng
        .try_fill_bytes(buffer)
        .map_err(|e| storage_error("csprng", SEAL_KEY_LABEL, e))
}

impl Sealer for MeshSealer {
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, StoreError> {
        let mut nonce_bytes = [0u8; NONCE_SIZE];
        fill_random(&mut nonce_bytes)?;
        let nonce = XNonce::from(nonce_bytes);

        let ciphertext = self
            .cipher("seal")?
            .encrypt(&nonce, plaintext)
            .map_err(|e| storage_error("seal", SEAL_KEY_LABEL, e))?;

        let mut sealed = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
        sealed.extend_from_slice(&nonce_bytes);
        sealed.extend_from_slice(&ciphertext);
        Ok(sealed)
    }

    fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, StoreError> {
        // A missing key is checked first and reported as a storage error, never
        // as `Malformed`. The distinction is load-bearing: the store reads
        // `Malformed` as "this can never be read, start empty and overwrite",
        // so returning it for a locked keyring would destroy a perfectly good
        // outbox the moment the Secret Service happened to be unavailable. A
        // lock opens later; the data does not come back.
        let cipher = self.cipher("open")?;

        // From here every failure *is* `Malformed`, and only here: the key is in
        // hand, so what remains is a corrupt blob or one sealed under a key that
        // is definitively gone (a restore onto a new device).
        let (nonce_bytes, body) = sealed
            .split_at_checked(NONCE_SIZE)
            .ok_or(StoreError::Malformed("mesh seal"))?;
        let nonce: [u8; NONCE_SIZE] = nonce_bytes
            .try_into()
            .map_err(|_| StoreError::Malformed("mesh seal"))?;

        cipher
            .decrypt(&XNonce::from(nonce), body)
            .map_err(|_| StoreError::Malformed("mesh seal"))
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {

    #[test]
    fn the_marker_keeps_an_existing_local_seal_key_instead_of_replacing_it() {
        // The whole point of the opt-out is that it is safe to leave on. A second
        // start must find the same key: generating a fresh one would leave every
        // blob already sealed permanently undecryptable, which is the exact failure
        // `from_keyring` refuses to allow on the keyring path.
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("chatter.db");
        std::fs::write(dir.path().join(super::super::super::system::keyring::NO_KEYRING_MARKER), "x")
            .expect("marker");

        let first = MeshSealer::from_keyring(&db_path);
        assert!(first.is_ready(), "the key should be generated on first use: {:?}", first.unavailable_reason());

        let sealed = first.seal(b"outbox").expect("seal");
        let second = MeshSealer::from_keyring(&db_path);
        assert!(second.is_ready(), "the stored key should be read back");
        assert_eq!(
            second.open(&sealed).expect("open").as_slice(),
            b"outbox",
            "a restart must open what the previous run sealed"
        );
    }
    use super::*;

    /// The keyring Android actually gets: the crate has no backend there and
    /// falls back to its in-memory mock (keyring 3.6.3 lib.rs:301-309), whose
    /// entries forget their password the moment the `Entry` is dropped.
    fn use_the_platformless_keyring() {
        static MOCK: std::sync::Once = std::sync::Once::new();
        MOCK.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
    }

    #[test]
    fn a_seal_written_by_one_run_opens_in_the_next_when_the_keyring_cannot_keep_it() {
        // The S9 bug: session one generated a seal key into a keyring that kept
        // it only in memory, sealed the peer pins with it, and exited. Session
        // two found no entry, generated a different key, and read its own pins
        // back as "persisted mesh seal is malformed" — so the mesh never started.
        use_the_platformless_keyring();
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("chatter.db");

        // An app-sandboxed platform (Android) keeps the key beside the database,
        // as it already keeps the database passphrase.
        let first = MeshSealer::acquire_with(&db_path, true).expect("first run");
        let sealed = first.seal(b"peer pins").expect("seal");
        drop(first);
        let second = MeshSealer::acquire_with(&db_path, true).expect("second run");
        assert_eq!(
            second.open(&sealed).expect("a seal this device wrote must open after restart"),
            b"peer pins"
        );
    }

    #[test]
    fn a_keyring_that_forgets_is_refused_rather_than_sealed_under() {
        // Off a sandboxed platform there is no safe place to put the key, so the
        // sealer must say so up front instead of sealing under a key that dies
        // with the process.
        use_the_platformless_keyring();
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("chatter.db");

        let first = MeshSealer::from_keyring(&db_path);
        if let Ok(sealed) = first.seal(b"peer pins") {
            let second = MeshSealer::from_keyring(&db_path);
            assert_eq!(
                second.open(&sealed).ok().as_deref(),
                Some(&b"peer pins"[..]),
                "a ready sealer's key must survive a restart"
            );
        }
    }

    #[test]
    fn a_new_seal_key_clears_rows_no_key_can_open_and_an_existing_key_keeps_them() {
        // The one-time recovery for devices that ran the broken build: their
        // rows were sealed under a key that is gone, so the first run with a
        // durable key must not trip over them forever.
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join(crate::system::keyring::NO_KEYRING_MARKER), "")
            .expect("marker");
        let db_path = dir.path().join("chatter.db");
        let db = Arc::new(Database::open(&db_path, "testpass").expect("open"));
        db.run_migrations().expect("migrations");

        let orphan = MeshSealer::from_key(&[9u8; KEY_SIZE]).seal(b"lost").expect("seal");
        MeshStorage::new(db.clone()).save(StorageKey::PeerPins, &orphan).expect("save");

        let (storage, sealer) = sealed_store(db.clone(), &db_path);
        assert!(sealer.key_is_new());
        assert_eq!(storage.load(StorageKey::PeerPins).expect("load"), None);

        let kept = sealer.seal(b"pins").expect("seal");
        storage.save(StorageKey::PeerPins, &kept).expect("save");
        let (storage, sealer) = sealed_store(db, &db_path);
        assert!(!sealer.key_is_new());
        let row = storage.load(StorageKey::PeerPins).expect("load").expect("row kept");
        assert_eq!(sealer.open(&row).expect("open"), b"pins");
    }

    // MARK: an unavailable sealer

    #[test]
    fn an_unavailable_sealer_refuses_rather_than_pretending() {
        let sealer = MeshSealer::unavailable("the keyring is locked");
        assert!(!sealer.is_ready());
        assert_eq!(sealer.unavailable_reason(), Some("the keyring is locked"));
        assert!(sealer.seal(b"anything").is_err());
    }

    #[test]
    fn an_unavailable_sealer_never_reports_malformed() {
        // This is the whole reason the distinction exists. `StoreError::Malformed`
        // means "this can never be read, start empty and overwrite it", so a
        // locked keyring answering with `Malformed` would destroy a perfectly
        // good outbox the moment the Secret Service happened to be down.
        let ready = MeshSealer::from_key(&[0x11; KEY_SIZE]);
        let sealed = ready.seal(b"retained mail").expect("seals");

        let locked = MeshSealer::unavailable("the keyring is locked");
        match locked.open(&sealed) {
            Err(StoreError::Malformed(_)) => {
                panic!("a locked keyring must not read as unrecoverable data")
            }
            Err(_) => {}
            Ok(_) => panic!("a sealer with no key cannot open anything"),
        }

        // And the data really was fine all along.
        assert_eq!(ready.open(&sealed).expect("opens"), b"retained mail");
    }

    #[test]
    fn a_corrupt_blob_still_reports_malformed_when_the_key_is_present() {
        // The other side of the same coin: with a key in hand, an unreadable
        // blob *is* unrecoverable and must say so, or the store keeps a row it
        // can never use.
        let sealer = MeshSealer::from_key(&[0x22; KEY_SIZE]);
        assert!(matches!(
            sealer.open(&[0u8; NONCE_SIZE + 4]),
            Err(StoreError::Malformed(_))
        ));
        assert!(matches!(sealer.open(&[]), Err(StoreError::Malformed(_))));
    }

    /// A real SQLCipher database in a temp dir, matching the pattern used by the
    /// repository tests. SQLCipher wants a real path, so this is as close to
    /// in-memory as the existing `Database` allows.
    fn test_db() -> Arc<Database> {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(
            Database::open(&dir.path().join("mesh_store_test.db"), "testpass").expect("open"),
        );
        db.run_migrations().expect("migrations");
        // Keep the directory alive until the test ends.
        crate::test_tempdir::hold(dir);
        db
    }

    /// Never touches the real keyring.
    fn sealer() -> MeshSealer {
        MeshSealer::from_key(&[7u8; KEY_SIZE])
    }

    /// The raw bytes as they sit in the row, bypassing the `Storage` impl.
    fn raw_row(db: &Database, key: StorageKey) -> Option<Vec<u8>> {
        let conn = db.connection().expect("conn");
        conn.query_row(
            "SELECT sealed FROM mesh_store WHERE store_key = ?1",
            params![key.as_str()],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .ok()
    }

    #[test]
    fn seal_save_load_open_round_trips() {
        let storage = MeshStorage::new(test_db());
        let sealer = sealer();
        let plaintext = b"OBX\x01 retained mail for a peer".to_vec();

        let sealed = sealer.seal(&plaintext).expect("seal");
        storage.save(StorageKey::Outbox, &sealed).expect("save");

        let loaded = storage
            .load(StorageKey::Outbox)
            .expect("load")
            .expect("the blob was just written");
        assert_eq!(loaded, sealed);
        assert_eq!(sealer.open(&loaded).expect("open"), plaintext);
    }

    #[test]
    fn saving_twice_replaces_rather_than_duplicating() {
        let storage = MeshStorage::new(test_db());
        storage.save(StorageKey::Outbox, b"first").expect("save");
        storage.save(StorageKey::Outbox, b"second").expect("save");

        assert_eq!(
            storage.load(StorageKey::Outbox).expect("load"),
            Some(b"second".to_vec())
        );
    }

    #[test]
    fn an_absent_key_loads_as_none() {
        let storage = MeshStorage::new(test_db());
        for key in StorageKey::ALL {
            assert_eq!(
                storage.load(*key).expect("load"),
                None,
                "{key:?} should be absent on a fresh database"
            );
        }
    }

    #[test]
    fn keys_do_not_collide() {
        let storage = MeshStorage::new(test_db());
        for key in StorageKey::ALL {
            storage.save(*key, key.as_str().as_bytes()).expect("save");
        }
        for key in StorageKey::ALL {
            assert_eq!(
                storage.load(*key).expect("load"),
                Some(key.as_str().as_bytes().to_vec()),
                "{key:?} was overwritten by another key"
            );
        }
    }

    #[test]
    fn a_corrupted_ciphertext_is_an_error_not_an_absence() {
        let storage = MeshStorage::new(test_db());
        let sealer = sealer();

        let mut sealed = sealer.seal(b"retained mail").expect("seal");
        let last = sealed.len() - 1;
        sealed[last] ^= 0xff; // break the Poly1305 tag
        storage.save(StorageKey::Outbox, &sealed).expect("save");

        // The row exists, so storage must say so — reporting `Ok(None)` here is
        // what would make a caller start empty and overwrite live mail.
        let loaded = storage.load(StorageKey::Outbox).expect("load");
        assert!(
            loaded.is_some(),
            "a corrupt row must never be reported as absent"
        );

        let error = sealer
            .open(&loaded.expect("present"))
            .expect_err("a broken tag must not open");
        assert!(matches!(error, StoreError::Malformed(_)), "got {error:?}");
    }

    #[test]
    fn a_blob_from_another_key_is_an_error_not_an_absence() {
        let storage = MeshStorage::new(test_db());
        let theirs = MeshSealer::from_key(&[9u8; KEY_SIZE]);

        let sealed = theirs.seal(b"someone else's outbox").expect("seal");
        storage.save(StorageKey::Outbox, &sealed).expect("save");

        let loaded = storage
            .load(StorageKey::Outbox)
            .expect("load")
            .expect("present");
        assert!(matches!(
            sealer().open(&loaded),
            Err(StoreError::Malformed(_))
        ));
    }

    #[test]
    fn a_row_that_cannot_be_decoded_is_an_error_not_an_absence() {
        let db = test_db();
        let storage = MeshStorage::new(db.clone());

        // SQLite is dynamically typed, so a TEXT value fits happily in a BLOB
        // column. Reading it back as bytes fails — and that failure must reach
        // the caller as `Err`, not as "nothing stored here".
        {
            let conn = db.connection().expect("conn");
            conn.execute(
                "INSERT INTO mesh_store (store_key, sealed, updated_at) VALUES (?1, ?2, ?3)",
                params![StorageKey::Outbox.as_str(), "not a blob", "now"],
            )
            .expect("insert");
        }

        let result = storage.load(StorageKey::Outbox);
        assert!(
            matches!(result, Err(StoreError::Storage { .. })),
            "an unreadable row must be an error, got {result:?}"
        );
    }

    #[test]
    fn two_seals_of_the_same_plaintext_differ() {
        let sealer = sealer();
        let plaintext = b"identical every time";

        let first = sealer.seal(plaintext).expect("seal");
        let second = sealer.seal(plaintext).expect("seal");

        assert_ne!(
            first, second,
            "a repeated ciphertext means the nonce was not fresh"
        );
        // Be specific about *where* they differ: the nonce prefix itself.
        assert_ne!(
            first.get(..NONCE_SIZE),
            second.get(..NONCE_SIZE),
            "nonces must never repeat under the same key"
        );
        // Both still open to the same plaintext.
        assert_eq!(sealer.open(&first).expect("open"), plaintext);
        assert_eq!(sealer.open(&second).expect("open"), plaintext);
    }

    #[test]
    fn the_persisted_bytes_do_not_contain_the_plaintext() {
        let db = test_db();
        let storage = MeshStorage::new(db.clone());
        let sealer = sealer();
        let plaintext = b"meet me at the usual place";

        let sealed = sealer.seal(plaintext).expect("seal");
        storage.save(StorageKey::Outbox, &sealed).expect("save");

        // Read the row directly rather than through `Storage`, so this asserts
        // about what is actually on the page — not about the API's return type.
        let stored = raw_row(&db, StorageKey::Outbox).expect("row");
        assert!(
            !stored
                .windows(plaintext.len())
                .any(|window| window == plaintext),
            "the sealed blob still contains the plaintext"
        );
    }

    #[test]
    fn wipe_removes_only_that_key() {
        let storage = MeshStorage::new(test_db());
        storage.save(StorageKey::Outbox, b"mail").expect("save");
        storage.save(StorageKey::PublicArchive, b"history").expect("save");

        storage.wipe(StorageKey::Outbox).expect("wipe");

        assert_eq!(storage.load(StorageKey::Outbox).expect("load"), None);
        assert_eq!(
            storage.load(StorageKey::PublicArchive).expect("load"),
            Some(b"history".to_vec())
        );
    }

    #[test]
    fn wiping_an_absent_key_is_not_an_error() {
        let storage = MeshStorage::new(test_db());
        storage.wipe(StorageKey::CourierPool).expect("wipe");
    }

    #[test]
    fn wipe_all_clears_every_key() {
        let db = test_db();
        let storage = MeshStorage::new(db.clone());
        for key in StorageKey::ALL {
            storage.save(*key, b"residue").expect("save");
        }

        storage.wipe_all().expect("wipe all");

        for key in StorageKey::ALL {
            assert_eq!(
                storage.load(*key).expect("load"),
                None,
                "{key:?} survived the panic wipe"
            );
        }
        let remaining: i64 = {
            let conn = db.connection().expect("conn");
            conn.query_row("SELECT COUNT(*) FROM mesh_store", [], |row| row.get(0))
                .expect("count")
        };
        assert_eq!(remaining, 0, "the panic wipe must leave no rows at all");
    }

    #[test]
    fn a_truncated_seal_does_not_open() {
        let sealer = sealer();
        assert!(matches!(
            sealer.open(&[0u8; NONCE_SIZE - 1]),
            Err(StoreError::Malformed(_))
        ));
        assert!(matches!(sealer.open(&[]), Err(StoreError::Malformed(_))));
    }

    #[test]
    fn the_seal_key_entry_is_distinct_per_database() {
        let a = MeshSealer::entry_username(Path::new("/tmp/a/chatter.db"));
        let b = MeshSealer::entry_username(Path::new("/tmp/b/chatter.db"));

        assert_ne!(a, b);
        assert!(a.starts_with("mesh-seal:"), "{a}");
        // Must not collide with the SQLCipher passphrase entry, which uses the
        // `db:` prefix — a shared entry would defeat the whole separation.
        assert!(!a.starts_with("db:"), "{a}");
    }
}

/// Write a file only its owner can read.
///
/// The mode is set as part of opening it, not afterwards: creating it world
/// readable and then narrowing leaves a window in which the key was readable.
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
    // Durable before anything is sealed under it: a key lost to a power cut
    // after use is every sealed blob lost with it.
    file.sync_all()
}
