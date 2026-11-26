//! Persistent custody for rotating mesh identities. Mesh and Signal keys are generated independently.

use std::sync::Arc;

use crate::crypto::os_rng;
use chatter_mesh::ids::PeerId;
use chatter_mesh_noise::MeshIdentity;
use rusqlite::{Connection, OptionalExtension, params};
use zeroize::Zeroizing;

use crate::error::StorageError;
use crate::storage::Database;

/// Length of both mesh secrets, in bytes.
const SECRET_LEN: usize = 32;

pub(crate) const ROTATION_SECS: u64 = 24 * 60 * 60;
pub(crate) const OVERLAP_SECS: u64 = 5 * 60;

pub(crate) struct MeshEpoch {
    pub identity: Arc<MeshIdentity>,
    pub started_at: u64,
    pub previous_peer: Option<PeerId>,
}

pub(crate) struct RetiredMeshEpoch {
    pub identity: Arc<MeshIdentity>,
    pub expires_at: u64,
}

/// Keeps independent Noise and signing keys in SQLCipher, separate from Signal.
pub struct MeshIdentityStore {
    db: Arc<Database>,
}

impl MeshIdentityStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
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
        Self::purge(&tx, now)?;
        let stamp = i64::try_from(now).unwrap_or(i64::MAX);
        let (identity, started_at, previous_peer) = match Self::read(&tx)? {
            Some((identity, started_at))
                if !enable && now.saturating_sub(started_at) < ROTATION_SECS =>
            {
                (identity, started_at, None)
            }
            Some((previous, _)) => {
                tx.execute(
                    "UPDATE mesh_identity_epochs SET retired_at = ?1 WHERE retired_at IS NULL",
                    [stamp],
                )?;
                (Self::insert(&tx, stamp)?, now, Some(previous.peer_id()))
            }
            None => (Self::insert(&tx, stamp)?, now, None),
        };
        tx.commit()?;
        Ok(MeshEpoch {
            identity: Arc::new(identity),
            started_at,
            previous_peer,
        })
    }

    /// Expire retired epochs even if the radio is switched off meanwhile.
    pub(crate) fn schedule_cleanup(self: &Arc<Self>, retired_at: u64) {
        let store = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(OVERLAP_SECS)).await;
            if let Some(store) = store.upgrade() {
                let cutoff = retired_at.saturating_add(OVERLAP_SECS);
                if let Err(error) = store.delete_expired(cutoff) {
                    tracing::warn!(target: "mesh", %error, "Could not delete expired mesh epochs");
                }
            }
        });
    }

    fn delete_expired(&self, now: u64) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.pragma_update(None, "secure_delete", true)?;
        Self::purge(&conn, now)
    }

    /// Read the overlap following a restart and delete epochs that have expired.
    pub(crate) fn retired(&self, now: u64) -> Result<Vec<RetiredMeshEpoch>, StorageError> {
        let conn = self.db.connection()?;
        conn.pragma_update(None, "secure_delete", true)?;
        Self::purge(&conn, now)?;
        let mut stmt = conn.prepare(
            "SELECT static_secret, signing_secret, retired_at FROM mesh_identity_epochs
             WHERE retired_at IS NOT NULL ORDER BY id DESC",
        )?;
        let mut rows = stmt.query([])?;
        let mut epochs = Vec::new();
        while let Some(row) = rows.next()? {
            let retired_at: u64 = row.get(2)?;
            epochs.push(RetiredMeshEpoch {
                identity: Arc::new(Self::decode_row(row)?),
                expires_at: retired_at.saturating_add(OVERLAP_SECS),
            });
        }
        Ok(epochs)
    }

    fn purge(conn: &Connection, now: u64) -> Result<(), StorageError> {
        if let Some(cutoff) = now.checked_sub(OVERLAP_SECS) {
            conn.execute(
                "DELETE FROM mesh_identity_epochs WHERE retired_at <= ?1",
                [i64::try_from(cutoff).unwrap_or(i64::MAX)],
            )?;
        }
        Ok(())
    }

    /// Used by the panic wipe to erase current and retired secrets.
    pub fn wipe(&self) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.pragma_update(None, "secure_delete", true)?;
        conn.execute("DELETE FROM mesh_identity_epochs", [])?;
        Ok(())
    }

    fn insert(conn: &Connection, started_at: i64) -> Result<MeshIdentity, StorageError> {
        let identity = MeshIdentity::generate(&mut os_rng());
        conn.execute(
            "INSERT INTO mesh_identity_epochs (static_secret, signing_secret, started_at)
             VALUES (?1, ?2, ?3)",
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
             WHERE retired_at IS NULL",
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

    fn decode_row(row: &rusqlite::Row<'_>) -> Result<MeshIdentity, StorageError> {
        let static_secret = Zeroizing::new(row.get::<_, Vec<u8>>(0)?);
        let signing_secret = Zeroizing::new(row.get::<_, Vec<u8>>(1)?);
        Self::decode(&static_secret, &signing_secret)
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
    fn a_daily_rotation_keeps_only_unexpired_old_keys_across_a_restart() {
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
            first.identity.peer_id(),
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
        let retired = reopened.retired(deadline + OVERLAP_SECS - 1).unwrap();
        assert_eq!(retired.len(), 1);
        assert_eq!(retired[0].identity.peer_id(), first.identity.peer_id());
        assert_eq!(
            reopened.get().unwrap().unwrap().peer_id(),
            next.identity.peer_id()
        );
        assert!(
            reopened
                .retired(deadline + OVERLAP_SECS)
                .unwrap()
                .is_empty()
        );
        assert!(
            reopened
                .retired(deadline + OVERLAP_SECS + 1)
                .unwrap()
                .is_empty()
        );
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
                "UPDATE mesh_identity_epochs SET static_secret = ?1 WHERE retired_at IS NULL",
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
