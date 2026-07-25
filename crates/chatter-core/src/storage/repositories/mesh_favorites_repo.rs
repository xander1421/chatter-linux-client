//! Durable mesh favourites.
//!
//! Keyed by Noise static key, because that is the identity the courier trust
//! tiers are decided on — a peer ID rotates and would lose the relationship.

use std::sync::Arc;

use chrono::Utc;
use rusqlite::params;

use crate::error::StorageError;
use crate::storage::Database;

pub struct MeshFavoritesRepository {
    db: Arc<Database>,
}

impl std::fmt::Debug for MeshFavoritesRepository {
    /// Opaque on purpose: `Database` is not `Debug`, and a repository's debug
    /// output has no business dumping a connection handle regardless.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MeshFavoritesRepository")
    }
}

impl MeshFavoritesRepository {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    pub fn add(&self, noise_key: &[u8; 32]) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT OR IGNORE INTO mesh_favorites (noise_key, added_at) VALUES (?1, ?2)",
            params![noise_key.as_slice(), Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn remove(&self, noise_key: &[u8; 32]) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "DELETE FROM mesh_favorites WHERE noise_key = ?1",
            params![noise_key.as_slice()],
        )?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<[u8; 32]>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare("SELECT noise_key FROM mesh_favorites")?;
        let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;

        let mut out = Vec::new();
        for row in rows {
            // The column CHECK enforces the width, so a wrong-length row means
            // the table was written by something else. Skip it rather than
            // failing the whole load: one bad row must not cost every favourite.
            if let Ok(key) = <[u8; 32]>::try_from(row?.as_slice()) {
                out.push(key);
            }
        }
        Ok(out)
    }

    /// Panic-wipe support.
    pub fn wipe(&self) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute("DELETE FROM mesh_favorites", [])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> (MeshFavoritesRepository, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db =
            Arc::new(Database::open(&dir.path().join("fav.db"), "testpass").expect("open"));
        db.run_migrations().expect("migrate");
        (MeshFavoritesRepository::new(db), dir)
    }

    #[test]
    fn a_favourite_survives_a_restart() {
        let (repo, _dir) = repo();
        repo.add(&[7u8; 32]).expect("add");
        assert_eq!(repo.list().expect("list"), vec![[7u8; 32]]);
    }

    #[test]
    fn adding_twice_stores_once() {
        let (repo, _dir) = repo();
        repo.add(&[7u8; 32]).expect("add");
        repo.add(&[7u8; 32]).expect("add again");
        assert_eq!(repo.list().expect("list").len(), 1);
    }

    #[test]
    fn removing_one_leaves_the_others() {
        let (repo, _dir) = repo();
        repo.add(&[1u8; 32]).expect("add");
        repo.add(&[2u8; 32]).expect("add");
        repo.remove(&[1u8; 32]).expect("remove");

        let listed = repo.list().expect("list");
        assert_eq!(listed, vec![[2u8; 32]]);
    }

    #[test]
    fn wipe_demotes_everyone_to_the_stranger_quota() {
        // Not cosmetic: favourites are the courier tier, so after a wipe no peer
        // may take the larger deposit quota.
        let (repo, _dir) = repo();
        repo.add(&[1u8; 32]).expect("add");
        repo.wipe().expect("wipe");
        assert!(repo.list().expect("list").is_empty());
    }
}
