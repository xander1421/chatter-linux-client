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
            id INTEGER PRIMARY KEY CHECK (id = 1),
            static_secret BLOB NOT NULL CHECK (length(static_secret) = 32),
            signing_secret BLOB NOT NULL CHECK (length(signing_secret) = 32),
            started_at INTEGER NOT NULL
        );
        CREATE TABLE mesh_private_group_identity (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            static_secret BLOB NOT NULL CHECK (length(static_secret) = 32),
            signing_secret BLOB NOT NULL CHECK (length(signing_secret) = 32)
        );
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            "DROP TABLE IF EXISTS mesh_private_group_identity; DROP TABLE IF EXISTS mesh_identity_epochs;",
        )
    }
}
