//! Separate SQLCipher custody for each mesh broadcast epoch.

use super::Migration;

pub struct V056MeshIdentity;

impl Migration for V056MeshIdentity {
    fn version(&self) -> u32 {
        56
    }

    fn description(&self) -> &'static str {
        "Create the mesh broadcast identity store"
    }

    fn up(&self) -> &'static str {
        r#"
        CREATE TABLE mesh_identity_epochs (
            id INTEGER PRIMARY KEY,
            static_secret BLOB NOT NULL CHECK (length(static_secret) = 32),
            signing_secret BLOB NOT NULL CHECK (length(signing_secret) = 32),
            started_at INTEGER,
            retired_at INTEGER
        );
        CREATE UNIQUE INDEX mesh_current_identity ON mesh_identity_epochs ((1))
            WHERE retired_at IS NULL;
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some("DROP TABLE IF EXISTS mesh_identity_epochs;")
    }
}
