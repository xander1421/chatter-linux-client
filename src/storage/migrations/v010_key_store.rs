//! V010: Signal Protocol key store migration.
//!
//! Adds tables for Signal Protocol key storage including identity keys,
//! prekeys, signed prekeys, Kyber prekeys, sessions, and sender keys.

use super::Migration;

/// V010: Create Signal Protocol key store tables.
pub struct V010KeyStore;

impl Migration for V010KeyStore {
    fn version(&self) -> u32 {
        10
    }

    fn description(&self) -> &'static str {
        "Create Signal Protocol key store tables"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Local device identity (single row)
        CREATE TABLE local_identity (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            registration_id INTEGER NOT NULL,
            identity_key_pair BLOB NOT NULL,
            created_at TEXT NOT NULL
        );

        -- Remote identity keys (TOFU)
        CREATE TABLE remote_identities (
            address TEXT PRIMARY KEY,
            identity_key BLOB NOT NULL,
            first_seen_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        -- One-time pre-keys
        CREATE TABLE prekeys (
            id INTEGER PRIMARY KEY,
            record BLOB NOT NULL,
            created_at TEXT NOT NULL
        );

        -- Signed pre-keys
        CREATE TABLE signed_prekeys (
            id INTEGER PRIMARY KEY,
            record BLOB NOT NULL,
            created_at TEXT NOT NULL
        );

        -- Kyber (post-quantum) pre-keys
        CREATE TABLE kyber_prekeys (
            id INTEGER PRIMARY KEY,
            record BLOB NOT NULL,
            created_at TEXT NOT NULL
        );

        -- Kyber key usage tracking (for last-resort key protection)
        CREATE TABLE kyber_prekey_usage (
            kyber_prekey_id INTEGER NOT NULL,
            ec_prekey_id INTEGER NOT NULL,
            base_key BLOB NOT NULL,
            used_at TEXT NOT NULL,
            PRIMARY KEY (kyber_prekey_id, ec_prekey_id, base_key),
            FOREIGN KEY (kyber_prekey_id) REFERENCES kyber_prekeys(id)
        );

        -- Double Ratchet sessions
        CREATE TABLE sessions (
            address TEXT PRIMARY KEY,
            record BLOB NOT NULL,
            updated_at TEXT NOT NULL
        );

        -- Group sender keys
        CREATE TABLE sender_keys (
            sender TEXT NOT NULL,
            distribution_id TEXT NOT NULL,
            record BLOB NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (sender, distribution_id)
        );
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
            DROP TABLE IF EXISTS sender_keys;
            DROP TABLE IF EXISTS sessions;
            DROP TABLE IF EXISTS kyber_prekey_usage;
            DROP TABLE IF EXISTS kyber_prekeys;
            DROP TABLE IF EXISTS signed_prekeys;
            DROP TABLE IF EXISTS prekeys;
            DROP TABLE IF EXISTS remote_identities;
            DROP TABLE IF EXISTS local_identity;
            "#,
        )
    }
}
