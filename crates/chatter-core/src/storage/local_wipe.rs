//! The duress wipe's storage steps: destroy every key that makes local data
//! readable, then delete the data. `ChatterClient::full_local_wipe` is the
//! one entry point and runs them in order around the mesh's own panic wipe.
//!
//! # What holds local data (every store this wipes)
//!
//! | Store | Where | Step |
//! |---|---|---|
//! | Android Keystore keys `chatter-db`, `chatter-mesh-seal` | TEE / StrongBox | [`destroy_keys`] (`KeyWrap::destroy`) |
//! | SQLCipher passphrase | OS keyring `db:<hash>` entry (desktop), `<db>.passphrase` (keyring opt-out), `<data>/.db_key` (Android, Keystore-sealed) | [`destroy_keys`] |
//! | Mesh seal key | `mesh-seal:<hash>` keyring entry / `<db>.mesh-seal-key` | [`destroy_keys`] (`MeshSealer::destroy_key`) |
//! | Database rows (account, Signal keys and sessions, messages, settings incl. the app lock, mesh tables) | `<data>/chatter.db` + `-wal`/`-shm` | [`wipe_rows_and_files`] (rows, then files) |
//! | Attachments | `<data>/attachments` | [`wipe_rows_and_files`] |
//! | Composer staging, Tor state, logs, moved-aside unreadable databases | under `<data>` | [`wipe_rows_and_files`] |
//! | Caches (previews, thumbnails, Tor directory cache) | `<cache>` | [`wipe_rows_and_files`] |
//! | Config files | `<config>` | [`wipe_rows_and_files`] |
//!
//! Outside this module: Android SharedPreferences and the posted
//! notifications are removed by the platform layer before the
//! relaunch (`MainActivity.wipeAndRestart`); in-memory state goes with the
//! process.
//!
//! # Order
//!
//! Keys first. Once the database passphrase and the mesh seal key are gone,
//! every byte the deletes leave behind on flash (WAL frames, free pages, an FTL
//! remap) is ciphertext under a key that no longer exists. Each step runs even
//! if an earlier one failed and the first error is returned, as the mesh panic
//! wipe does: stopping early would leave the rest on disk.
//!
//! # Not covered
//!
//! A passphrase given through `CHATTER_DB_PASSPHRASE` is not stored anywhere
//! to destroy. On Windows an open file cannot be deleted, so the database file
//! itself may survive — emptied of rows, its key destroyed; the next start
//! moves it aside as unreadable and begins fresh.

use std::path::Path;

use crate::error::StorageError;
use crate::storage::Database;
use crate::system::Directories;

fn io_err(what: &str, path: &Path, e: std::io::Error) -> StorageError {
    StorageError::InvalidData(format!("wipe: {what} {}: {e}", path.display()))
}

fn remove_file(path: &Path) -> Result<(), StorageError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err("delete", path, e)),
    }
}

fn remove_dir(path: &Path) -> Result<(), StorageError> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err("delete", path, e)),
    }
}

/// Make a directory's entry removals durable (Unix). Best effort elsewhere.
fn sync_dir(path: &Path) {
    #[cfg(unix)]
    if let Ok(dir) = std::fs::File::open(path) {
        let _ = dir.sync_all();
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn keep_first(slot: &mut Option<StorageError>, result: Result<(), StorageError>) {
    if let Err(error) = result {
        tracing::error!(%error, "local wipe step failed; continuing");
        slot.get_or_insert(error);
    }
}

/// Step 1: destroy the keys every local store is encrypted under.
pub(crate) fn destroy_keys(directories: &Directories) -> Result<(), StorageError> {
    let db_path = directories.database_path();
    let mut first = None;

    // Android: the Keystore keys the passphrase and mesh seal key files are
    // sealed under. Hardware-held, so deleting them is a real erase even
    // though flash keeps the deleted file bytes.
    if let Ok(wrap) = crate::system::sealed_secret::installed_key_wrap() {
        for alias in [
            crate::system::sealed_secret::DB_ALIAS,
            crate::system::sealed_secret::MESH_SEAL_ALIAS,
        ] {
            keep_first(
                &mut first,
                wrap.destroy(alias)
                    .map_err(|e| StorageError::InvalidData(format!("wipe: keystore {alias}: {e}"))),
            );
        }
    }

    // The passphrase file of a keyring opt-out, and Android's sealed key file.
    let mut passphrase_file = db_path.clone();
    passphrase_file.set_extension("passphrase");
    keep_first(&mut first, remove_file(&passphrase_file));
    keep_first(&mut first, remove_file(&directories.data_dir().join(".db_key")));

    // The OS keyring entry, wherever the keyring is in use.
    if !cfg!(target_os = "android")
        && std::env::var_os("CHATTER_DB_PASSPHRASE").is_none()
        && !crate::system::keyring::keyring_disabled(&db_path)
    {
        let deleted = crate::system::Keyring::new(db_path.clone())
            .delete_passphrase()
            .map_err(|e| StorageError::InvalidData(format!("wipe: keyring passphrase: {e}")));
        keep_first(&mut first, deleted);
    }

    #[cfg(feature = "mesh")]
    keep_first(
        &mut first,
        crate::storage::mesh_store::MeshSealer::destroy_key(&db_path)
            .map_err(|e| StorageError::InvalidData(format!("wipe: mesh seal key: {e}"))),
    );

    sync_dir(directories.data_dir());
    first.map_or(Ok(()), Err)
}

/// Step 2: empty the database, then delete every directory the app writes.
pub(crate) fn wipe_rows_and_files(
    database: &Database,
    directories: &Directories,
) -> Result<(), StorageError> {
    let mut first = None;
    keep_first(&mut first, crate::storage::unlink::wipe_all_rows(database));
    // Fold the WAL back so the emptied pages are what the file holds.
    keep_first(
        &mut first,
        database
            .connection()
            .and_then(|conn| {
                conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                    .map_err(StorageError::from)
            }),
    );

    let attachments = crate::services::attachment::attachments_dir();
    if !attachments.starts_with(directories.data_dir()) {
        keep_first(&mut first, remove_dir(&attachments));
    }
    for dir in [
        directories.data_dir(),
        directories.cache_dir(),
        directories.config_dir(),
    ] {
        keep_first(&mut first, remove_dir(dir));
        if let Some(parent) = dir.parent() {
            sync_dir(parent);
        }
    }
    crate::media::static_preview::wipe();
    first.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::repositories::settings_repo::SettingsRepository;
    use std::sync::Arc;

    /// A fresh install rooted in a temp dir, using the test keyring store.
    fn install() -> (tempfile::TempDir, Directories, Arc<Database>) {
        keyring::set_default_store(crate::system::keyring::test_credential_store());
        let base = tempfile::tempdir().unwrap();
        let dirs = Directories::with_base_dir(base.path().to_path_buf()).unwrap();
        let db = Arc::new(Database::open(&dirs.database_path(), "pass").unwrap());
        db.run_migrations().unwrap();
        (base, dirs, db)
    }

    #[test]
    fn destroy_keys_removes_every_key_copy() {
        let (_base, dirs, _db) = install();
        let db_path = dirs.database_path();
        let keyring = crate::system::Keyring::new(db_path.clone());
        // Keyring entry (fake store) and the two key files.
        keyring.set_passphrase("secret").unwrap();
        let mut passphrase_file = db_path.clone();
        passphrase_file.set_extension("passphrase");
        std::fs::write(&passphrase_file, "secret").unwrap();
        std::fs::write(dirs.data_dir().join(".db_key"), "sealed").unwrap();
        let mut mesh_key = db_path.clone();
        mesh_key.set_extension("mesh-seal-key");
        std::fs::write(&mesh_key, "00").unwrap();

        destroy_keys(&dirs).unwrap();

        assert!(!passphrase_file.exists());
        assert!(!dirs.data_dir().join(".db_key").exists());
        // The keyring step is skipped (and the entry kept) only when the
        // environment opts out of the keyring.
        if std::env::var_os("CHATTER_NO_KEYRING").is_none()
            && std::env::var_os("CHATTER_DB_PASSPHRASE").is_none()
            && std::env::var_os("CHATTER_DB_PASSPHRASE_FILE").is_none()
        {
            assert!(keyring.get_passphrase().is_err());
        }
        #[cfg(feature = "mesh")]
        assert!(!mesh_key.exists());
        // Idempotent: nothing left is not an error.
        destroy_keys(&dirs).unwrap();
    }

    #[test]
    fn wipe_rows_and_files_leaves_nothing() {
        let (_base, dirs, db) = install();
        let settings = SettingsRepository::new(db.clone());
        settings.set_string("mesh.nickname", "bob").unwrap();
        settings.set_string("profile.display_name", "Bob").unwrap();
        for dir in [dirs.data_dir(), dirs.cache_dir(), dirs.config_dir()] {
            std::fs::create_dir_all(dir.join("sub")).unwrap();
            std::fs::write(dir.join("sub/file"), "x").unwrap();
        }

        wipe_rows_and_files(&db, &dirs).unwrap();

        let rows: i64 = db
            .connection()
            .unwrap()
            .query_row("SELECT count(*) FROM settings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0);
        for dir in [dirs.data_dir(), dirs.cache_dir(), dirs.config_dir()] {
            assert!(!dir.exists(), "{} survived", dir.display());
        }
        assert!(!dirs.database_path().exists());
    }
}
