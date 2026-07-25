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
    cipher: XChaCha20Poly1305,
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
            cipher: XChaCha20Poly1305::new(&key),
        }
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
    pub fn from_keyring(db_path: &Path) -> Result<Self, StoreError> {
        let entry = keyring::Entry::new(APP_ID, &Self::entry_username(db_path))
            .map_err(|e| storage_error("keyring open", SEAL_KEY_LABEL, e))?;

        match entry.get_password() {
            Ok(encoded) => {
                let encoded = Zeroizing::new(encoded);
                let raw = Zeroizing::new(hex::decode(encoded.as_bytes()).map_err(|e| {
                    // A corrupt entry is not "absent". Regenerating here would
                    // throw away the key that the stored blobs were sealed with.
                    storage_error("keyring decode", SEAL_KEY_LABEL, e)
                })?);
                let key: [u8; KEY_SIZE] = raw.as_slice().try_into().map_err(|_| {
                    storage_error(
                        "keyring decode",
                        SEAL_KEY_LABEL,
                        format!("expected {KEY_SIZE} key bytes, found {}", raw.len()),
                    )
                })?;
                Ok(Self::from_key(&key))
            }
            Err(keyring::Error::NoEntry) => {
                let key = Zeroizing::new(random_key()?);
                let encoded = Zeroizing::new(hex::encode(key.as_slice()));
                // Store before use. A key we sealed with but failed to persist
                // would read back as garbage after the next restart.
                entry
                    .set_password(&encoded)
                    .map_err(|e| storage_error("keyring store", SEAL_KEY_LABEL, e))?;
                Ok(Self::from_key(&key))
            }
            Err(e) => Err(storage_error("keyring unlock", SEAL_KEY_LABEL, e)),
        }
    }

    /// Remove the seal key from the keyring.
    ///
    /// Logout and account reset. Everything in `mesh_store` becomes permanently
    /// unreadable, which is the point — call [`Storage::wipe_all`] alongside it
    /// so the unreadable rows do not linger.
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
            .cipher
            .encrypt(&nonce, plaintext)
            .map_err(|e| storage_error("seal", SEAL_KEY_LABEL, e))?;

        let mut sealed = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
        sealed.extend_from_slice(&nonce_bytes);
        sealed.extend_from_slice(&ciphertext);
        Ok(sealed)
    }

    fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, StoreError> {
        // Every failure below is `Malformed`, which the store reads as "this can
        // never be read, start empty and overwrite". That is the right call
        // *here* and only here: this sealer already holds its key by the time it
        // exists, so a failure is a corrupt blob or one sealed under a key that
        // is definitively gone (a restore onto a new device), not a lock that
        // will open later. The "not readable yet" case is handled one level up,
        // in `from_keyring`, which returns an error instead of a sealer.
        let (nonce_bytes, body) = sealed
            .split_at_checked(NONCE_SIZE)
            .ok_or(StoreError::Malformed("mesh seal"))?;
        let nonce: [u8; NONCE_SIZE] = nonce_bytes
            .try_into()
            .map_err(|_| StoreError::Malformed("mesh seal"))?;

        self.cipher
            .decrypt(&XNonce::from(nonce), body)
            .map_err(|_| StoreError::Malformed("mesh seal"))
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A real SQLCipher database in a temp dir, matching the pattern used by the
    /// repository tests. SQLCipher wants a real path, so this is as close to
    /// in-memory as the existing `Database` allows.
    fn test_db() -> Arc<Database> {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(
            Database::open(&dir.path().join("mesh_store_test.db"), "testpass").expect("open"),
        );
        db.run_migrations().expect("migrations");
        // Keep the directory alive for the whole test.
        Box::leak(Box::new(dir));
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
        storage.save(StorageKey::Metrics, b"counters").expect("save");

        storage.wipe(StorageKey::Outbox).expect("wipe");

        assert_eq!(storage.load(StorageKey::Outbox).expect("load"), None);
        assert_eq!(
            storage.load(StorageKey::Metrics).expect("load"),
            Some(b"counters".to_vec())
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
