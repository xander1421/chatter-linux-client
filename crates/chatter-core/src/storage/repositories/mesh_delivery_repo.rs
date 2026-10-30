//! Durable delivery state for mesh messages this device sent.
//!
//! Deliberately a side table rather than a column on `messages`: the mesh's
//! delivery vocabulary is disjoint from the server's, and merging them would put
//! two meanings of the word "sent" in one field. See V060 for why that matters.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use rusqlite::params;

use crate::error::StorageError;
use crate::storage::Database;

pub struct MeshDeliveryRepository {
    db: Arc<Database>,
}

impl MeshDeliveryRepository {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Record where a message has got to. Later states overwrite earlier ones.
    ///
    /// No ordering is enforced here. The router is the authority on progression
    /// and a repository that second-guessed it would have to encode the state
    /// machine twice.
    pub fn set(&self, message_id: &str, state: &str) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            r#"
            INSERT INTO mesh_message_delivery (message_id, state, updated_at)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(message_id) DO UPDATE SET
                state = excluded.state,
                updated_at = excluded.updated_at
            "#,
            params![message_id, state, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// States for a batch of messages. Absent IDs are simply missing from the
    /// map — a message with no recorded state is not an error, it is one whose
    /// send predates this table or never reported.
    pub fn get_many(
        &self,
        message_ids: &[String],
    ) -> Result<HashMap<String, String>, StorageError> {
        if message_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let conn = self.db.read_connection()?;
        // Rebuilt per call rather than cached: the placeholder count is the
        // batch size, so there is no one statement to prepare.
        let placeholders = vec!["?"; message_ids.len()].join(",");
        let sql = format!(
            "SELECT message_id, state FROM mesh_message_delivery WHERE message_id IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(message_ids.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut out = HashMap::with_capacity(message_ids.len());
        for row in rows {
            let (id, state) = row?;
            out.insert(id, state);
        }
        Ok(out)
    }

    /// Panic-wipe support.
    pub fn wipe(&self) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute("DELETE FROM mesh_message_delivery", [])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> (MeshDeliveryRepository, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(Database::open(&dir.path().join("test.db"), "testpass").expect("open"));
        db.run_migrations().expect("migrate");
        (MeshDeliveryRepository::new(db), dir)
    }

    #[test]
    fn a_state_survives_a_restart() {
        let (repo, _dir) = repo();
        repo.set("msg-1", "carried").expect("set");
        let found = repo.get_many(&["msg-1".to_string()]).expect("get");
        assert_eq!(found.get("msg-1").map(String::as_str), Some("carried"));
    }

    #[test]
    fn a_later_state_replaces_an_earlier_one() {
        let (repo, _dir) = repo();
        repo.set("msg-1", "waiting").expect("set");
        repo.set("msg-1", "delivered").expect("advance");
        let found = repo.get_many(&["msg-1".to_string()]).expect("get");
        assert_eq!(found.get("msg-1").map(String::as_str), Some("delivered"));
        assert_eq!(found.len(), 1, "upsert must not duplicate the row");
    }

    #[test]
    fn unknown_ids_are_absent_rather_than_defaulted() {
        // The distinction the UI depends on: "no state recorded" must not be
        // indistinguishable from a state that happens to mean success.
        let (repo, _dir) = repo();
        repo.set("msg-1", "waiting").expect("set");
        let found = repo
            .get_many(&["msg-1".to_string(), "msg-2".to_string()])
            .expect("get");
        assert_eq!(found.len(), 1);
        assert!(!found.contains_key("msg-2"));
    }

    #[test]
    fn an_empty_batch_touches_the_database_not_at_all() {
        let (repo, _dir) = repo();
        assert!(repo.get_many(&[]).expect("get").is_empty());
    }

    #[test]
    fn wipe_forgets_everything() {
        let (repo, _dir) = repo();
        repo.set("msg-1", "carried").expect("set");
        repo.wipe().expect("wipe");
        assert!(
            repo.get_many(&["msg-1".to_string()])
                .expect("get")
                .is_empty()
        );
    }
}
