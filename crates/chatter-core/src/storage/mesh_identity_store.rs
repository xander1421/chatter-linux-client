//! Persistent storage for the bitchat mesh identity.
//!
//! The mesh needs a long-term Curve25519 static key (Noise key agreement) and a
//! long-term Ed25519 signing key (announces). `chatter_mesh_noise` deliberately
//! owns no key custody — every entry point there takes raw secret bytes — so
//! custody lands here, in the SQLCipher database, next to the Signal key
//! material and under the same OS-keyring passphrase.
//!
//! # The mesh identity is a separate keypair from the Signal identity. Always.
//!
//! `KeyStore` already holds a Curve25519 identity keypair in `local_identity`,
//! and the temptation to reuse it — "we already have a Curve25519 key right
//! there" — is exactly the simplification this module exists to prevent. Do not
//! do it, for two independent reasons:
//!
//! 1. **Cross-protocol key reuse.** The same secret would drive a Noise XX
//!    handshake and the Signal protocol: two constructions agreeing and signing
//!    over one key, with no analysis anywhere saying that composition is safe.
//!    Signal's security proof does not cover an attacker who can also feed the
//!    key into a Noise transcript, and Noise's does not cover X3DH.
//! 2. **Unlinkability.** A shared key permanently ties a user's mesh presence to
//!    their chatter account. Anyone who sees both a mesh announce and the
//!    account's public identity key could join them. The whole point of a local
//!    mesh identity is that it does not carry the account with it.
//!
//! So this module generates fresh key material from the OS CSPRNG and stores it
//! in its own table (`mesh_identity`, V056). It never reads from and never
//! writes to `local_identity`, `remote_identities`, or any other Signal table.

use std::sync::Arc;

use chatter_mesh_noise::MeshIdentity;
use chrono::Utc;
use rand_core_06::OsRng;
use rusqlite::{Connection, params};
use zeroize::Zeroize;

use crate::error::StorageError;
use crate::storage::Database;

/// Length of both mesh secrets, in bytes.
const SECRET_LEN: usize = 32;

/// Mesh identity store.
///
/// Owns the device's single long-term mesh keypair. Shares the encrypted
/// database with the rest of the storage layer via `Arc<Database>`, following
/// the `KeyStore` pattern.
pub struct MeshIdentityStore {
    db: Arc<Database>,
}

impl MeshIdentityStore {
    /// Create a new `MeshIdentityStore`.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Load the stored mesh identity.
    ///
    /// Returns `None` if this device has never generated one, or if it was
    /// wiped.
    pub fn get(&self) -> Result<Option<MeshIdentity>, StorageError> {
        let conn = self.db.read_connection()?;
        Self::read(&conn)
    }

    /// Load the mesh identity, generating and persisting one on first call.
    ///
    /// Every later call returns the same keypair, so the device's mesh
    /// fingerprint and peer ID are stable across restarts — peers verify each
    /// other by fingerprint, and a new key on every launch would make that
    /// verification worthless.
    pub fn get_or_create(&self) -> Result<MeshIdentity, StorageError> {
        // The whole read-then-maybe-insert runs on the writer connection: its
        // mutex serialises callers, so two threads cannot both observe an empty
        // table and generate two different identities. Doing the read through
        // `read_connection()` first would reopen exactly that race.
        let conn = self.db.connection()?;

        if let Some(identity) = Self::read(&conn)? {
            return Ok(identity);
        }

        // Fresh key material straight from the OS CSPRNG. NOT derived from, and
        // NOT shared with, the Signal identity key — see the module docs.
        let identity = MeshIdentity::generate(&mut OsRng);
        let now = Utc::now().to_rfc3339();

        conn.execute(
            "INSERT INTO mesh_identity (id, static_secret, signing_secret, created_at)
             VALUES (1, ?1, ?2, ?3)",
            params![
                &identity.static_secret()[..],
                &identity.signing_secret()[..],
                now
            ],
        )?;

        tracing::info!(
            fingerprint = %identity.fingerprint(),
            "Generated a new mesh identity"
        );

        Ok(identity)
    }

    /// Delete the stored mesh identity.
    ///
    /// Used by the panic wipe. After this the device has no mesh presence at
    /// all: `get` returns `None`, and the next `get_or_create` mints a fresh
    /// keypair with a different fingerprint, which is the point — the old
    /// identity must not be recoverable or re-derivable.
    ///
    /// Touches only `mesh_identity`. Signal key material is wiped separately by
    /// `KeyStore`, and neither is a substitute for the other.
    pub fn wipe(&self) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute("DELETE FROM mesh_identity", [])?;
        Ok(())
    }

    /// Read the identity row from an already-acquired connection.
    fn read(conn: &Connection) -> Result<Option<MeshIdentity>, StorageError> {
        let row: Result<(Vec<u8>, Vec<u8>), _> = conn.query_row(
            "SELECT static_secret, signing_secret FROM mesh_identity WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        );

        match row {
            Ok((mut static_secret, mut signing_secret)) => {
                let identity = Self::decode(&static_secret, &signing_secret);
                // The heap buffers rusqlite handed us hold raw secrets; clear
                // them rather than leaving copies for a later allocation (or a
                // core dump) to expose.
                static_secret.zeroize();
                signing_secret.zeroize();
                identity.map(Some)
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Rebuild a `MeshIdentity` from stored secret bytes.
    ///
    /// The public halves are re-derived rather than stored, so a corrupted row
    /// cannot produce an identity whose advertised public key does not match
    /// the secret we would actually handshake with.
    fn decode(static_secret: &[u8], signing_secret: &[u8]) -> Result<MeshIdentity, StorageError> {
        let static_secret = Self::to_secret_array(static_secret, "static_secret")?;
        let signing_secret = Self::to_secret_array(signing_secret, "signing_secret")?;
        Ok(MeshIdentity::from_keys(static_secret, signing_secret))
    }

    /// Convert a stored blob to a fixed-size secret, or report the corruption.
    ///
    /// The table has a `length(...) = 32` CHECK, so a wrong length here means
    /// the row was written by something other than this store.
    fn to_secret_array(bytes: &[u8], column: &str) -> Result<[u8; SECRET_LEN], StorageError> {
        let len = bytes.len();
        bytes.try_into().map_err(|_| {
            StorageError::InvalidData(format!(
                "mesh identity {column} is {len} bytes, expected {SECRET_LEN}"
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use libsignal_protocol::IdentityKeyPair;
    use rand::TryRngCore;

    use crate::storage::KeyStore;

    /// Temp-file SQLCipher database, following the `call_repo` test pattern:
    /// SQLCipher's `:memory:` handling needs a passphrase after attach and
    /// `Database::open` assumes a real path, so a temp file is the closest we
    /// get to in-memory. The temp dir lives until the test ends.
    fn setup_db() -> Arc<Database> {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("mesh_identity_test.db");
        let db = Arc::new(Database::open(&db_path, "testpass").unwrap());
        db.run_migrations().unwrap();
        crate::test_tempdir::hold(temp_dir);
        db
    }

    /// Store a real Signal identity keypair, returning its serialized form.
    fn store_signal_identity(db: &Arc<Database>) -> Vec<u8> {
        let mut rng = rand::rngs::OsRng.unwrap_err();
        let key_pair = IdentityKeyPair::generate(&mut rng);
        let serialized = key_pair.serialize().to_vec();
        KeyStore::new(db.clone())
            .set_local_identity(1234, &serialized)
            .unwrap();
        serialized
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn get_returns_none_before_anything_is_generated() {
        let store = MeshIdentityStore::new(setup_db());
        assert!(store.get().unwrap().is_none());
    }

    #[test]
    fn first_call_generates_and_persists() {
        let db = setup_db();
        let store = MeshIdentityStore::new(db.clone());

        let created = store.get_or_create().unwrap();

        // A separate store over the same database sees it, so it really landed
        // in the table rather than only in memory.
        let reloaded = MeshIdentityStore::new(db).get().unwrap().unwrap();
        assert_eq!(created.static_secret(), reloaded.static_secret());
        assert_eq!(created.signing_secret(), reloaded.signing_secret());
        assert_eq!(created.fingerprint(), reloaded.fingerprint());
    }

    #[test]
    fn second_call_returns_the_identical_keypair() {
        let store = MeshIdentityStore::new(setup_db());

        let first = store.get_or_create().unwrap();
        let second = store.get_or_create().unwrap();

        assert_eq!(first.static_secret(), second.static_secret());
        assert_eq!(first.signing_secret(), second.signing_secret());
        assert_eq!(first.static_public(), second.static_public());
        assert_eq!(first.signing_public(), second.signing_public());
    }

    #[test]
    fn the_two_mesh_secrets_are_independent_of_each_other() {
        let store = MeshIdentityStore::new(setup_db());
        let identity = store.get_or_create().unwrap();
        assert_ne!(identity.static_secret(), identity.signing_secret());
    }

    #[test]
    fn mesh_identity_shares_nothing_with_the_signal_identity() {
        let db = setup_db();
        let signal_key_pair = store_signal_identity(&db);

        let store = MeshIdentityStore::new(db.clone());
        let mesh = store.get_or_create().unwrap();

        // Neither mesh secret nor either mesh public key appears anywhere in
        // the serialized Signal identity keypair. If a future "simplification"
        // ever wires the two together, this is what fails.
        assert!(!contains(&signal_key_pair, &mesh.static_secret()[..]));
        assert!(!contains(&signal_key_pair, &mesh.signing_secret()[..]));
        assert!(!contains(&signal_key_pair, &mesh.static_public()));
        assert!(!contains(&signal_key_pair, &mesh.signing_public()));

        // The two live in different rows of different tables: wiping one leaves
        // the other completely intact, in both directions.
        let key_store = KeyStore::new(db.clone());
        store.wipe().unwrap();
        assert_eq!(
            key_store.get_local_identity().unwrap().unwrap().1,
            signal_key_pair
        );

        let mesh_again = store.get_or_create().unwrap();
        key_store.clear_local_identity().unwrap();
        assert_eq!(
            store.get().unwrap().unwrap().static_secret(),
            mesh_again.static_secret()
        );
    }

    #[test]
    fn wipe_clears_the_identity() {
        let db = setup_db();
        let store = MeshIdentityStore::new(db.clone());

        store.get_or_create().unwrap();
        store.wipe().unwrap();

        assert!(store.get().unwrap().is_none());
        // And it is gone from the database, not just from this handle.
        assert!(MeshIdentityStore::new(db).get().unwrap().is_none());
    }

    #[test]
    fn wipe_is_a_no_op_when_there_is_nothing_to_wipe() {
        let store = MeshIdentityStore::new(setup_db());
        store.wipe().unwrap();
        assert!(store.get().unwrap().is_none());
    }

    #[test]
    fn get_or_create_after_a_wipe_produces_a_different_identity() {
        let store = MeshIdentityStore::new(setup_db());

        let before = store.get_or_create().unwrap();
        store.wipe().unwrap();
        let after = store.get_or_create().unwrap();

        // A panic wipe that handed back the same key would leave the device
        // recognisable to every peer that had already seen it.
        assert_ne!(before.static_secret(), after.static_secret());
        assert_ne!(before.signing_secret(), after.signing_secret());
        assert_ne!(before.fingerprint(), after.fingerprint());
    }

    #[test]
    fn a_corrupt_row_is_reported_rather_than_silently_accepted() {
        let db = setup_db();
        let store = MeshIdentityStore::new(db.clone());
        store.get_or_create().unwrap();

        // Bypass the store and try to plant a short secret, as a corrupted or
        // foreign writer would. The column CHECK rejects it.
        let short = vec![0u8; 16];
        {
            let conn = db.connection().unwrap();
            conn.execute(
                "UPDATE mesh_identity SET static_secret = ?1 WHERE id = 1",
                params![short],
            )
            .unwrap_err();
        }

        // The CHECK held, so the good row survives and still reads back.
        assert!(store.get().unwrap().is_some());

        // And the decoder itself rejects a short secret outright.
        let err = MeshIdentityStore::decode(&[0u8; 16], &[0u8; 32]).unwrap_err();
        assert!(matches!(err, StorageError::InvalidData(_)));
    }
}
