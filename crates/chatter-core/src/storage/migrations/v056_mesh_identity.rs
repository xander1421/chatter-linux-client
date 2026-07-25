//! V056: Persistent storage for the bitchat mesh identity.
//!
//! One row holds the two long-term mesh secrets: the Curve25519 static key used
//! for Noise key agreement and the Ed25519 key used to sign announces. They are
//! two keys, not one, for the same reason `chatter_mesh_noise::MeshIdentity`
//! keeps them apart — reusing a single keypair across key agreement and a
//! signature scheme is a cross-protocol footgun.
//!
//! ## Why this is not the `local_identity` table
//!
//! V010 already stores a Curve25519 identity keypair — the Signal one. This
//! table exists *because* it must not be shared with it:
//!
//! - Driving a Noise handshake and the Signal protocol from the same secret is
//!   cross-protocol key reuse. No analysis anywhere says that composition is
//!   safe, so we do not rely on it.
//! - The mesh identity is meant to be unlinkable from the chatter account. One
//!   shared key would permanently bind a user's mesh presence to their account
//!   and defeat the point.
//!
//! Separate table, separate rows, no foreign key, no shared column. Nothing in
//! the mesh path should ever read `local_identity`.

use super::Migration;

/// V056: Create the `mesh_identity` table.
pub struct V056MeshIdentity;

impl Migration for V056MeshIdentity {
    fn version(&self) -> u32 {
        56
    }

    fn description(&self) -> &'static str {
        "Create mesh_identity table for the bitchat mesh keypair"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Single-row table: a device has exactly one mesh identity at a time.
        -- The `id = 1` CHECK is the same singleton idiom `local_identity` uses.
        CREATE TABLE IF NOT EXISTS mesh_identity (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            -- Curve25519 static secret, raw 32 bytes, handed to `snow`.
            static_secret BLOB NOT NULL CHECK (length(static_secret) = 32),
            -- Ed25519 signing secret, raw 32 bytes (the seed, not the
            -- expanded key) — what `SigningKey::from_bytes` takes.
            signing_secret BLOB NOT NULL CHECK (length(signing_secret) = 32),
            created_at TEXT NOT NULL
        );
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some("DROP TABLE IF EXISTS mesh_identity;")
    }
}
