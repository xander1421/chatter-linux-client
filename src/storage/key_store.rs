//! Signal protocol key storage.
//!
//! Persistent storage for Signal Protocol keys including identity keys,
//! prekeys, sessions, and sender keys.

use crate::error::StorageError;
use crate::storage::Database;
use chrono::Utc;
use rusqlite::params;
use std::sync::Arc;

/// Signal key store.
///
/// Provides CRUD operations for all Signal Protocol key material.
/// Uses Arc<Database> for shared access to the encrypted database.
pub struct KeyStore {
    db: Arc<Database>,
}

impl KeyStore {
    /// Create a new KeyStore.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    // ========================================================================
    // Local Identity
    // ========================================================================

    /// Get the local identity (registration_id, identity_key_pair bytes).
    ///
    /// Returns `None` if no local identity has been set.
    pub fn get_local_identity(&self) -> Result<Option<(u32, Vec<u8>)>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT registration_id, identity_key_pair FROM local_identity WHERE id = 1",
        )?;

        let result: Result<(u32, Vec<u8>), _> = stmt.query_row([], |row| {
            Ok((row.get(0)?, row.get(1)?))
        });

        match result {
            Ok(data) => Ok(Some(data)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Set the local identity.
    ///
    /// This should only be called once during device registration.
    /// Returns an error if a local identity already exists.
    pub fn set_local_identity(
        &self,
        registration_id: u32,
        identity_key_pair: &[u8],
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now().to_rfc3339();

        conn.execute(
            "INSERT INTO local_identity (id, registration_id, identity_key_pair, created_at)
             VALUES (1, ?1, ?2, ?3)",
            params![registration_id, identity_key_pair, now],
        )?;

        Ok(())
    }

    /// Check if a local identity exists.
    pub fn has_local_identity(&self) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM local_identity WHERE id = 1",
            [],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    // ========================================================================
    // Remote Identities
    // ========================================================================

    /// Get a remote identity key by address.
    ///
    /// Address format is "name:device_id" (ProtocolAddress).
    pub fn get_identity(&self, address: &str) -> Result<Option<Vec<u8>>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt =
            conn.prepare("SELECT identity_key FROM remote_identities WHERE address = ?1")?;

        let result: Result<Vec<u8>, _> = stmt.query_row(params![address], |row| row.get(0));

        match result {
            Ok(key) => Ok(Some(key)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Save a remote identity key.
    ///
    /// Uses upsert semantics. Returns `true` if an existing identity was
    /// updated (key change detected), `false` if this is a new identity.
    pub fn save_identity(&self, address: &str, identity_key: &[u8]) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now().to_rfc3339();

        // Check if identity exists and if key changed
        let existing: Option<Vec<u8>> = {
            let mut stmt =
                conn.prepare("SELECT identity_key FROM remote_identities WHERE address = ?1")?;
            let result: Result<Vec<u8>, _> = stmt.query_row(params![address], |row| row.get(0));
            result.ok()
        };

        let key_changed = existing.as_ref().map_or(false, |existing_key| {
            existing_key.as_slice() != identity_key
        });

        conn.execute(
            "INSERT INTO remote_identities (address, identity_key, first_seen_at, updated_at)
             VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(address) DO UPDATE SET
                 identity_key = excluded.identity_key,
                 updated_at = excluded.updated_at",
            params![address, identity_key, now],
        )?;

        Ok(key_changed)
    }

    /// Check if a remote identity exists.
    pub fn identity_exists(&self, address: &str) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM remote_identities WHERE address = ?1",
            params![address],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    // ========================================================================
    // Pre-keys (One-time)
    // ========================================================================

    /// Get a pre-key record by ID.
    pub fn get_prekey(&self, id: u32) -> Result<Option<Vec<u8>>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare("SELECT record FROM prekeys WHERE id = ?1")?;

        let result: Result<Vec<u8>, _> = stmt.query_row(params![id], |row| row.get(0));

        match result {
            Ok(record) => Ok(Some(record)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Save a pre-key record.
    pub fn save_prekey(&self, id: u32, record: &[u8]) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now().to_rfc3339();

        conn.execute(
            "INSERT OR REPLACE INTO prekeys (id, record, created_at) VALUES (?1, ?2, ?3)",
            params![id, record, now],
        )?;

        Ok(())
    }

    /// Remove a pre-key by ID.
    ///
    /// Returns `true` if the pre-key existed and was removed.
    pub fn remove_prekey(&self, id: u32) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute("DELETE FROM prekeys WHERE id = ?1", params![id])?;
        Ok(rows > 0)
    }

    // ========================================================================
    // Signed Pre-keys
    // ========================================================================

    /// Get a signed pre-key record by ID.
    pub fn get_signed_prekey(&self, id: u32) -> Result<Option<Vec<u8>>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare("SELECT record FROM signed_prekeys WHERE id = ?1")?;

        let result: Result<Vec<u8>, _> = stmt.query_row(params![id], |row| row.get(0));

        match result {
            Ok(record) => Ok(Some(record)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Save a signed pre-key record.
    pub fn save_signed_prekey(&self, id: u32, record: &[u8]) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now().to_rfc3339();

        conn.execute(
            "INSERT OR REPLACE INTO signed_prekeys (id, record, created_at) VALUES (?1, ?2, ?3)",
            params![id, record, now],
        )?;

        Ok(())
    }

    // ========================================================================
    // Kyber Pre-keys (Post-Quantum)
    // ========================================================================

    /// Get a Kyber pre-key record by ID.
    pub fn get_kyber_prekey(&self, id: u32) -> Result<Option<Vec<u8>>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare("SELECT record FROM kyber_prekeys WHERE id = ?1")?;

        let result: Result<Vec<u8>, _> = stmt.query_row(params![id], |row| row.get(0));

        match result {
            Ok(record) => Ok(Some(record)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Save a Kyber pre-key record.
    pub fn save_kyber_prekey(&self, id: u32, record: &[u8]) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now().to_rfc3339();

        conn.execute(
            "INSERT OR REPLACE INTO kyber_prekeys (id, record, created_at) VALUES (?1, ?2, ?3)",
            params![id, record, now],
        )?;

        Ok(())
    }

    /// Mark a Kyber pre-key as used with a specific EC key and base key.
    ///
    /// This implements last-resort key protection: if the same combination
    /// of (kyber_id, ec_id, base_key) is seen twice, it indicates key reuse
    /// which should be rejected.
    ///
    /// Returns an error if this exact combination has already been used.
    pub fn mark_kyber_prekey_used(
        &self,
        kyber_id: u32,
        ec_id: u32,
        base_key: &[u8],
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now().to_rfc3339();

        // Check if this usage already exists
        let exists: bool = {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM kyber_prekey_usage
                 WHERE kyber_prekey_id = ?1 AND ec_prekey_id = ?2 AND base_key = ?3",
                params![kyber_id, ec_id, base_key],
                |row| row.get(0),
            )?;
            count > 0
        };

        if exists {
            return Err(StorageError::InvalidData(format!(
                "Kyber pre-key {} with EC key {} already used with this base key",
                kyber_id, ec_id
            )));
        }

        conn.execute(
            "INSERT INTO kyber_prekey_usage (kyber_prekey_id, ec_prekey_id, base_key, used_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![kyber_id, ec_id, base_key, now],
        )?;

        Ok(())
    }

    /// Remove a Kyber pre-key by ID.
    ///
    /// Also removes all associated usage records.
    /// Returns `true` if the pre-key existed and was removed.
    pub fn remove_kyber_prekey(&self, id: u32) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;

        // Delete usage records first (foreign key constraint)
        conn.execute(
            "DELETE FROM kyber_prekey_usage WHERE kyber_prekey_id = ?1",
            params![id],
        )?;

        let rows = conn.execute("DELETE FROM kyber_prekeys WHERE id = ?1", params![id])?;
        Ok(rows > 0)
    }

    // ========================================================================
    // Sessions (Double Ratchet)
    // ========================================================================

    /// Load a session record by address.
    ///
    /// Address format is "name:device_id" (ProtocolAddress).
    pub fn load_session(&self, address: &str) -> Result<Option<Vec<u8>>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare("SELECT record FROM sessions WHERE address = ?1")?;

        let result: Result<Vec<u8>, _> = stmt.query_row(params![address], |row| row.get(0));

        match result {
            Ok(record) => Ok(Some(record)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Store a session record.
    ///
    /// Uses upsert semantics.
    pub fn store_session(&self, address: &str, record: &[u8]) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now().to_rfc3339();

        conn.execute(
            "INSERT INTO sessions (address, record, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(address) DO UPDATE SET
                 record = excluded.record,
                 updated_at = excluded.updated_at",
            params![address, record, now],
        )?;

        Ok(())
    }

    // ========================================================================
    // Sender Keys (Group)
    // ========================================================================

    /// Load a sender key record.
    ///
    /// Sender is a ProtocolAddress ("name:device_id"), distribution_id is a UUID.
    pub fn load_sender_key(
        &self,
        sender: &str,
        distribution_id: &str,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT record FROM sender_keys WHERE sender = ?1 AND distribution_id = ?2",
        )?;

        let result: Result<Vec<u8>, _> =
            stmt.query_row(params![sender, distribution_id], |row| row.get(0));

        match result {
            Ok(record) => Ok(Some(record)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Store a sender key record.
    ///
    /// Uses upsert semantics.
    pub fn store_sender_key(
        &self,
        sender: &str,
        distribution_id: &str,
        record: &[u8],
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now().to_rfc3339();

        conn.execute(
            "INSERT INTO sender_keys (sender, distribution_id, record, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(sender, distribution_id) DO UPDATE SET
                 record = excluded.record,
                 updated_at = excluded.updated_at",
            params![sender, distribution_id, record, now],
        )?;

        Ok(())
    }
}
