//! Persistent custody for rotating mesh identities. Mesh and Signal keys are generated independently.

use std::sync::Arc;

use crate::crypto::os_rng;
use chatter_mesh_noise::MeshIdentity;
use rusqlite::{Connection, OptionalExtension, params};
use zeroize::Zeroizing;

use crate::error::StorageError;
use crate::storage::Database;

/// Length of both mesh secrets, in bytes.
const SECRET_LEN: usize = 32;

pub(crate) const ROTATION_SECS: u64 = 24 * 60 * 60;

pub(crate) struct MeshEpoch {
    pub identity: Arc<MeshIdentity>,
    pub started_at: u64,
}

/// Keeps independent Noise and signing keys in SQLCipher, separate from Signal.
pub struct MeshIdentityStore {
    db: Arc<Database>,
}

impl MeshIdentityStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Private group signing material never goes into a radio identity or
    /// advertisement. It is disclosed only inside a proven pairing.
    pub(crate) fn private_group_identity(&self) -> Result<Arc<MeshIdentity>, StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let stored = tx.query_row(
            "SELECT static_secret, signing_secret FROM mesh_private_group_identity WHERE id = 1",
            [],
            |row| Ok((Zeroizing::new(row.get::<_, Vec<u8>>(0)?), Zeroizing::new(row.get::<_, Vec<u8>>(1)?))),
        ).optional()?;
        let identity = if let Some((noise, signing)) = stored {
            Self::decode(&noise, &signing)?
        } else {
            let identity = MeshIdentity::generate(&mut os_rng());
            tx.execute(
                "INSERT INTO mesh_private_group_identity (id, static_secret, signing_secret) VALUES (1, ?1, ?2)",
                params![&identity.static_secret()[..], &identity.signing_secret()[..]],
            )?;
            identity
        };
        tx.commit()?;
        Ok(Arc::new(identity))
    }

    #[cfg(test)]
    pub fn get(&self) -> Result<Option<MeshIdentity>, StorageError> {
        let conn = self.db.read_connection()?;
        Ok(Self::read(&conn)?.map(|(identity, _)| identity))
    }

    /// An enable starts a fresh epoch. Periodic checks start one only when due.
    pub(crate) fn advance(&self, now: u64, enable: bool) -> Result<MeshEpoch, StorageError> {
        let mut conn = self.db.connection()?;
        // Scrub deleted keys from freed SQLCipher pages as well as the live table.
        conn.pragma_update(None, "secure_delete", true)?;
        let tx = conn.transaction()?;
        let stamp = i64::try_from(now).unwrap_or(i64::MAX);
        let (identity, started_at) = match Self::read(&tx)? {
            Some((identity, started_at))
                if !enable && now.saturating_sub(started_at) < ROTATION_SECS =>
            {
                (identity, started_at)
            }
            Some(_) => {
                tx.execute("DELETE FROM mesh_identity_epochs", [])?;
                (Self::insert(&tx, stamp)?, now)
            }
            None => (Self::insert(&tx, stamp)?, now),
        };
        tx.commit()?;
        Ok(MeshEpoch {
            identity: Arc::new(identity),
            started_at,
        })
    }

    /// Used by the panic wipe to erase the current identity.
    pub fn wipe(&self) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.pragma_update(None, "secure_delete", true)?;
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM mesh_identity_epochs", [])?;
        tx.execute("DELETE FROM mesh_private_group_identity", [])?;
        tx.commit()?;
        Ok(())
    }

    fn insert(conn: &Connection, started_at: i64) -> Result<MeshIdentity, StorageError> {
        let identity = MeshIdentity::generate(&mut os_rng());
        conn.execute(
            "INSERT INTO mesh_identity_epochs (id, static_secret, signing_secret, started_at)
             VALUES (1, ?1, ?2, ?3)",
            params![
                &identity.static_secret()[..],
                &identity.signing_secret()[..],
                started_at,
            ],
        )?;
        Ok(identity)
    }

    fn read(conn: &Connection) -> Result<Option<(MeshIdentity, u64)>, StorageError> {
        let row = conn
            .query_row(
                "SELECT static_secret, signing_secret, started_at FROM mesh_identity_epochs
             WHERE id = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get(2)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(static_secret, signing_secret, stamp)| {
            let static_secret = Zeroizing::new(static_secret);
            let signing_secret = Zeroizing::new(signing_secret);
            Ok((Self::decode(&static_secret, &signing_secret)?, stamp))
        })
        .transpose()
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
    fn private_group_identity_survives_radio_rotation_and_is_wiped_separately() {
        let db = setup_db();
        let store = MeshIdentityStore::new(db.clone());
        let group = store.private_group_identity().unwrap();
        let first = store.advance(1_000, true).unwrap().identity;
        let second = store.advance(2_000, true).unwrap().identity;
        assert_ne!(first.peer_id(), second.peer_id());
        assert_ne!(group.peer_id(), first.peer_id());
        let reloaded = MeshIdentityStore::new(db);
        assert_eq!(
            reloaded.private_group_identity().unwrap().peer_id(),
            group.peer_id()
        );
        reloaded.wipe().unwrap();
        assert_ne!(
            reloaded.private_group_identity().unwrap().peer_id(),
            group.peer_id()
        );
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

        let created = store.advance(1_000, false).unwrap().identity;

        // A separate store over the same database sees it, so it really landed
        // in the table rather than only in memory.
        let reloaded = MeshIdentityStore::new(db).get().unwrap().unwrap();
        assert_eq!(created.static_secret(), reloaded.static_secret());
        assert_eq!(created.signing_secret(), reloaded.signing_secret());
        assert_eq!(created.fingerprint(), reloaded.fingerprint());
    }

    #[test]
    fn a_periodic_check_before_the_deadline_preserves_the_keypair() {
        let store = MeshIdentityStore::new(setup_db());

        let first = store.advance(1_000, false).unwrap().identity;
        let second = store.advance(1_000, false).unwrap().identity;

        assert_eq!(first.static_secret(), second.static_secret());
        assert_eq!(first.signing_secret(), second.signing_secret());
        assert_eq!(first.static_public(), second.static_public());
        assert_eq!(first.signing_public(), second.signing_public());
    }

    #[test]
    fn the_two_mesh_secrets_are_independent_of_each_other() {
        let store = MeshIdentityStore::new(setup_db());
        let identity = store.advance(1_000, false).unwrap().identity;
        assert_ne!(identity.static_secret(), identity.signing_secret());
    }

    #[test]
    fn mesh_identity_shares_nothing_with_the_signal_identity() {
        let db = setup_db();
        let signal_key_pair = store_signal_identity(&db);

        let store = MeshIdentityStore::new(db.clone());
        let mesh = store.advance(1_000, false).unwrap().identity;

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

        let mesh_again = store.advance(1_000, false).unwrap().identity;
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

        store.advance(1_000, false).unwrap().identity;
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

        let before = store.advance(1_000, false).unwrap().identity;
        store.wipe().unwrap();
        let after = store.advance(1_000, false).unwrap().identity;

        // A panic wipe that handed back the same key would leave the device
        // recognisable to every peer that had already seen it.
        assert_ne!(before.static_secret(), after.static_secret());
        assert_ne!(before.signing_secret(), after.signing_secret());
        assert_ne!(before.fingerprint(), after.fingerprint());
    }

    #[test]
    fn an_epoch_rotation_keeps_only_the_current_keypair_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("epochs.db");
        let open = || {
            let db = Arc::new(Database::open(&path, "testpass").unwrap());
            db.run_migrations().unwrap();
            MeshIdentityStore::new(db)
        };
        let store = open();
        let first = store.advance(1_000, true).unwrap();
        let deadline = first.started_at + ROTATION_SECS;
        assert_eq!(
            store
                .advance(deadline - 1, false)
                .unwrap()
                .identity
                .peer_id(),
            first.identity.peer_id()
        );
        let next = store.advance(deadline, false).unwrap();
        assert_ne!(next.identity.peer_id(), first.identity.peer_id());
        assert_ne!(
            next.identity.static_public(),
            first.identity.static_public()
        );
        assert_ne!(
            next.identity.signing_public(),
            first.identity.signing_public()
        );
        drop(store);

        let reopened = open();
        assert_eq!(
            reopened.get().unwrap().unwrap().peer_id(),
            next.identity.peer_id()
        );
        let conn = reopened.db.read_connection().unwrap();
        let rows: u32 = conn
            .query_row("SELECT count(*) FROM mesh_identity_epochs", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn a_corrupt_row_is_reported_rather_than_silently_accepted() {
        let db = setup_db();
        let store = MeshIdentityStore::new(db.clone());
        store.advance(1_000, false).unwrap().identity;

        // Bypass the store and try to plant a short secret, as a corrupted or
        // foreign writer would. The column CHECK rejects it.
        let short = vec![0u8; 16];
        {
            let conn = db.connection().unwrap();
            conn.execute(
                "UPDATE mesh_identity_epochs SET static_secret = ?1 WHERE id = 1",
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
