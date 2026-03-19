//! Migration v018: Rename public_key to svr_public_key.
//!
//! Renames the `public_key` column in the users table to `svr_public_key`
//! (Secure Value Recovery) for PIN-based key recovery functionality.

use super::Migration;

pub struct V018RenameSvrPublicKey;

impl Migration for V018RenameSvrPublicKey {
    fn version(&self) -> u32 {
        18
    }

    fn description(&self) -> &'static str {
        "Rename public_key to svr_public_key"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Rename public_key column to svr_public_key for Secure Value Recovery
        ALTER TABLE users RENAME COLUMN public_key TO svr_public_key;
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
            ALTER TABLE users RENAME COLUMN svr_public_key TO public_key;
            "#,
        )
    }
}
