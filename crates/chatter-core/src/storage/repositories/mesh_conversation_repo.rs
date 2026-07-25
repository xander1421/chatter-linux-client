//! Durable directory of open mesh conversations.
//!
//! Stores just enough to rebuild the sidebar after a restart: which mesh
//! conversations are open, and what to call them. The messages themselves are
//! ordinary rows in `messages`; this table only exists because the conversation
//! UUID is derived one-way from the wire identifier and so cannot be reversed
//! out of those rows.

use std::sync::Arc;

use chrono::Utc;
use rusqlite::params;

use crate::error::StorageError;
use crate::storage::Database;

/// One remembered mesh conversation, in the storage layer's own terms.
///
/// Deliberately not `MeshConversation`: that type lives in the service layer and
/// a repository should not depend on it. The service maps between them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredMeshConversation {
    pub conversation_id: String,
    /// `room`, `geohash`, or `dm`.
    pub kind: String,
    /// Peer ID for a DM, cell for a geohash channel, `None` for the room.
    pub wire_id: Option<String>,
    pub label: String,
}

pub struct MeshConversationRepository {
    db: Arc<Database>,
}

impl MeshConversationRepository {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Remember a conversation, or update its label.
    ///
    /// The label is only overwritten by a non-empty one. A peer that has not yet
    /// named itself must not blank out a name it gave earlier.
    pub fn upsert(&self, entry: &StoredMeshConversation) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            r#"
            INSERT INTO mesh_conversations (conversation_id, kind, wire_id, label, joined_at)
            VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(conversation_id) DO UPDATE SET
                label = CASE WHEN excluded.label = '' THEN mesh_conversations.label
                             ELSE excluded.label END
            "#,
            params![
                entry.conversation_id,
                entry.kind,
                entry.wire_id,
                entry.label,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Every remembered conversation.
    pub fn list(&self) -> Result<Vec<StoredMeshConversation>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT conversation_id, kind, wire_id, label FROM mesh_conversations",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(StoredMeshConversation {
                conversation_id: row.get(0)?,
                kind: row.get(1)?,
                wire_id: row.get(2)?,
                label: row.get(3)?,
            })
        })?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Forget one conversation. Its messages are left alone — leaving a geohash
    /// channel should not destroy what was said in it.
    pub fn remove(&self, conversation_id: &str) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "DELETE FROM mesh_conversations WHERE conversation_id = ?1",
            params![conversation_id],
        )?;
        Ok(())
    }

    /// Panic-wipe support: forget every mesh conversation.
    pub fn wipe(&self) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute("DELETE FROM mesh_conversations", [])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> (MeshConversationRepository, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(
            Database::open(&dir.path().join("test.db"), "testpass").expect("open"),
        );
        db.run_migrations().expect("migrate");
        (MeshConversationRepository::new(db), dir)
    }

    fn dm(id: &str, label: &str) -> StoredMeshConversation {
        StoredMeshConversation {
            conversation_id: id.to_string(),
            kind: "dm".to_string(),
            wire_id: Some("peer-1".to_string()),
            label: label.to_string(),
        }
    }

    #[test]
    fn a_conversation_survives_a_restart() {
        let (repo, _dir) = repo();
        repo.upsert(&dm("conv-1", "alice")).expect("upsert");

        let listed = repo.list().expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].label, "alice");
        assert_eq!(listed[0].wire_id.as_deref(), Some("peer-1"));
    }

    #[test]
    fn an_empty_label_never_erases_a_known_one() {
        // A peer that has not named itself must not blank out a name it gave
        // earlier — otherwise one anonymous message renames the conversation.
        let (repo, _dir) = repo();
        repo.upsert(&dm("conv-1", "alice")).expect("upsert");
        repo.upsert(&dm("conv-1", "")).expect("upsert again");

        assert_eq!(repo.list().expect("list")[0].label, "alice");
    }

    #[test]
    fn a_later_name_replaces_an_earlier_one() {
        let (repo, _dir) = repo();
        repo.upsert(&dm("conv-1", "alice")).expect("upsert");
        repo.upsert(&dm("conv-1", "alice-renamed")).expect("rename");

        let listed = repo.list().expect("list");
        assert_eq!(listed.len(), 1, "upsert must not duplicate the row");
        assert_eq!(listed[0].label, "alice-renamed");
    }

    #[test]
    fn removing_one_leaves_the_others() {
        let (repo, _dir) = repo();
        repo.upsert(&dm("conv-1", "alice")).expect("upsert");
        repo.upsert(&StoredMeshConversation {
            conversation_id: "conv-2".into(),
            kind: "geohash".into(),
            wire_id: Some("dr5rsj7".into()),
            label: "block".into(),
        })
        .expect("upsert cell");

        repo.remove("conv-1").expect("remove");
        let listed = repo.list().expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].conversation_id, "conv-2");
    }

    #[test]
    fn the_room_is_the_only_kind_allowed_a_null_wire_id() {
        let (repo, _dir) = repo();
        repo.upsert(&StoredMeshConversation {
            conversation_id: "room".into(),
            kind: "room".into(),
            wire_id: None,
            label: "mesh".into(),
        })
        .expect("room stores");

        // The schema CHECK ties the two together: a DM without a wire id could
        // never be mapped back to a peer, so it must not be storable.
        let bad = repo.upsert(&StoredMeshConversation {
            conversation_id: "conv-bad".into(),
            kind: "dm".into(),
            wire_id: None,
            label: String::new(),
        });
        assert!(bad.is_err(), "a dm with no wire id must be rejected");
    }

    #[test]
    fn wipe_forgets_everything() {
        let (repo, _dir) = repo();
        repo.upsert(&dm("conv-1", "alice")).expect("upsert");
        repo.wipe().expect("wipe");
        assert!(repo.list().expect("list").is_empty());
    }
}
