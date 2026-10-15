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

        let result: Result<(u32, Vec<u8>), _> =
            stmt.query_row([], |row| Ok((row.get(0)?, row.get(1)?)));

        match result {
            Ok(data) => Ok(Some(data)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Current account/device for the device-local mesh Signal path. These
    /// settings are read together with the same SQLCipher connection.
    #[cfg(feature = "mesh")]
    pub fn mesh_local_account_device(&self) -> Result<Option<(String, u32)>, StorageError> {
        use rusqlite::OptionalExtension;
        let conn = self.db.read_connection()?;
        let user: Option<Vec<u8>> = conn
            .query_row(
                "SELECT CAST(value AS BLOB) FROM settings WHERE key='auth.current_user_id'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let device: Option<Vec<u8>> = conn
            .query_row(
                "SELECT CAST(value AS BLOB) FROM settings WHERE key='auth.current_device_id'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let (Some(user), Some(device)) = (user, device) else {
            return Ok(None);
        };
        let user: String = serde_json::from_slice(&user)
            .map_err(|_| StorageError::InvalidData("invalid current account".into()))?;
        let device: u32 = serde_json::from_slice(&device)
            .map_err(|_| StorageError::InvalidData("invalid current device".into()))?;
        Ok(Some((user, device)))
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

    /// Clear the local identity.
    ///
    /// Used to clean up after a failed registration attempt.
    pub fn clear_local_identity(&self) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute("DELETE FROM local_identity", [])?;
        Ok(())
    }

    /// Clear all crypto state.
    ///
    /// Removes local identity, prekeys, signed prekeys, kyber prekeys,
    /// and all sessions. Used to reset crypto state after failed registration.
    pub fn clear_all_crypto_state(&self) -> Result<(), StorageError> {
        let conn = self.db.connection()?;

        // Clear in order to respect foreign key constraints
        conn.execute("DELETE FROM kyber_prekey_usage", [])?;
        conn.execute("DELETE FROM sender_keys", [])?;
        conn.execute("DELETE FROM sessions", [])?;
        conn.execute("DELETE FROM kyber_prekeys", [])?;
        conn.execute("DELETE FROM signed_prekeys", [])?;
        conn.execute("DELETE FROM prekeys", [])?;
        conn.execute("DELETE FROM remote_identities", [])?;
        conn.execute("DELETE FROM local_identity", [])?;

        Ok(())
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

        let key_changed = existing
            .as_ref()
            .is_some_and(|existing_key| existing_key.as_slice() != identity_key);

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

    /// Get a remote identity key by UUID prefix (any device ID).
    ///
    /// This is useful for call encryption where we need the peer's identity
    /// but don't care which device it came from.
    ///
    /// Note: Address format in the store is "{uuid}:{device_id}" (colon separator).
    pub fn get_identity_by_uuid(&self, uuid: &str) -> Result<Option<Vec<u8>>, StorageError> {
        // Identity keys are pinned per ACCOUNT (keyed by the bare user id), so
        // this is an exact lookup that returns THE single pinned key for the
        // user — which is what the safety number must reflect. (Previously a
        // `LIKE 'uuid:%' LIMIT 1` over per-device rows returned an arbitrary
        // device's key.)
        self.get_identity(uuid)
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

    /// Count the number of available pre-keys.
    ///
    /// Used to determine when to refresh pre-keys.
    /// Signal recommends maintaining at least 100 pre-keys.
    pub fn count_prekeys(&self) -> Result<u32, StorageError> {
        let conn = self.db.connection()?;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM prekeys", [], |row| row.get(0))?;
        Ok(count as u32)
    }

    /// Get the maximum pre-key ID currently stored.
    ///
    /// Returns 0 if no pre-keys exist. Used to determine the starting ID
    /// for generating new pre-keys.
    pub fn max_prekey_id(&self) -> Result<u32, StorageError> {
        let conn = self.db.connection()?;
        let max_id: Option<i64> =
            conn.query_row("SELECT MAX(id) FROM prekeys", [], |row| row.get(0))?;
        Ok(max_id.unwrap_or(0) as u32)
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

    /// Get the creation timestamp of the most recent signed prekey.
    ///
    /// Returns `None` if no signed prekeys exist (e.g., before registration).
    /// Used by the background task to check if signed prekey rotation is needed.
    pub fn get_latest_signed_prekey_created_at(
        &self,
    ) -> Result<Option<chrono::DateTime<Utc>>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt =
            conn.prepare("SELECT created_at FROM signed_prekeys ORDER BY id DESC LIMIT 1")?;

        let result: Result<String, _> = stmt.query_row([], |row| row.get(0));

        match result {
            Ok(ts_str) => {
                let dt = chrono::DateTime::parse_from_rfc3339(&ts_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .map_err(|e| {
                        StorageError::Database(rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        ))
                    })?;
                Ok(Some(dt))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Get the maximum signed prekey ID.
    ///
    /// Returns 0 if no signed prekeys exist. Used to determine the next ID
    /// for generating a rotated signed prekey.
    pub fn max_signed_prekey_id(&self) -> Result<u32, StorageError> {
        let conn = self.db.connection()?;
        let max_id: Option<i64> =
            conn.query_row("SELECT MAX(id) FROM signed_prekeys", [], |row| row.get(0))?;
        Ok(max_id.unwrap_or(0) as u32)
    }

    /// Delete signed prekeys older than `max_age_days` days.
    ///
    /// Retains old signed prekeys for a grace period (typically 30 days)
    /// so in-flight X3DH sessions referencing them can still complete.
    /// After the grace period, they are safe to prune.
    pub fn cleanup_expired_signed_prekeys(&self, max_age_days: u64) -> Result<u32, StorageError> {
        let conn = self.db.connection()?;
        let cutoff = (Utc::now() - chrono::Duration::days(max_age_days as i64)).to_rfc3339();

        // Keep at least the most recent signed prekey (never delete it)
        let max_id: Option<i64> =
            conn.query_row("SELECT MAX(id) FROM signed_prekeys", [], |row| row.get(0))?;

        let deleted = if let Some(current_id) = max_id {
            conn.execute(
                "DELETE FROM signed_prekeys WHERE created_at < ?1 AND id != ?2",
                params![cutoff, current_id],
            )?
        } else {
            0
        };

        Ok(deleted as u32)
    }

    // ========================================================================
    // Kyber Pre-keys (Post-Quantum)
    // ========================================================================

    /// Get the maximum Kyber prekey ID.
    ///
    /// Returns 0 if no Kyber prekeys exist. Used to determine the next ID
    /// for generating a rotated Kyber prekey.
    pub fn max_kyber_prekey_id(&self) -> Result<u32, StorageError> {
        let conn = self.db.connection()?;
        let max_id: Option<i64> =
            conn.query_row("SELECT MAX(id) FROM kyber_prekeys", [], |row| row.get(0))?;
        Ok(max_id.unwrap_or(0) as u32)
    }

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

    /// Delete *spent* Kyber pre-keys older than `max_age_days` days.
    ///
    /// Retains old Kyber pre-keys for a grace period (typically 30 days) so
    /// in-flight PQXDH sessions referencing them can still complete. After the
    /// grace period, a key that has already been used is safe to prune.
    ///
    /// A key is pruned only if it is BOTH older than the cutoff AND has a usage
    /// record. An age check alone is not sufficient, and this is a correctness
    /// requirement, not a tidiness one:
    ///
    /// Registration seeds a pool of ~100 one-time Kyber prekeys (PQOPKs) that
    /// live on the server until some peer handshakes with them, and the server
    /// hands them out OLDEST-FIRST (`signal_protocol.go`, `ORDER BY created_at
    /// ASC`). Those keys are all created at registration, so ~30 days later the
    /// whole unused remainder crosses the cutoff at once — while still published
    /// and still first in line to be served. Deleting a private key whose public
    /// half the server will hand out produces a PreKeySignalMessage this device
    /// cannot decrypt: a silent, delayed, unrecoverable session failure.
    ///
    /// This function predates the PQOPK pool and its "keep only MAX(id)" rule
    /// encoded the old assumption that exactly one Kyber prekey — the
    /// medium-term last-resort PQSPK — is ever published. That assumption no
    /// longer holds. MAX(id) is still retained unconditionally as a floor.
    ///
    /// The cost of the safer rule is that a rotated-away last-resort PQSPK that
    /// was never used is now retained rather than pruned (locally it is
    /// indistinguishable from an unused pool key). That is a bounded leak of a
    /// ~1.5KB key a couple of times a week; serving an undecryptable handshake
    /// is not bounded. Correctness wins.
    pub fn cleanup_expired_kyber_prekeys(&self, max_age_days: u64) -> Result<u32, StorageError> {
        let conn = self.db.connection()?;
        let cutoff = (Utc::now() - chrono::Duration::days(max_age_days as i64)).to_rfc3339();

        let max_id: Option<i64> =
            conn.query_row("SELECT MAX(id) FROM kyber_prekeys", [], |row| row.get(0))?;

        let deleted = if let Some(current_id) = max_id {
            // Resolve the victims first: once the usage records are gone the
            // "has been used" predicate can no longer be evaluated.
            let ids: Vec<i64> = {
                let mut stmt = conn.prepare(
                    "SELECT id FROM kyber_prekeys
                     WHERE created_at < ?1
                       AND id != ?2
                       AND id IN (SELECT kyber_prekey_id FROM kyber_prekey_usage)",
                )?;
                let rows = stmt.query_map(params![cutoff, current_id], |row| row.get(0))?;
                rows.collect::<Result<Vec<i64>, _>>()?
            };

            let mut deleted = 0usize;
            for id in ids {
                // Usage records first (FK constraint).
                conn.execute(
                    "DELETE FROM kyber_prekey_usage WHERE kyber_prekey_id = ?1",
                    params![id],
                )?;
                deleted += conn.execute("DELETE FROM kyber_prekeys WHERE id = ?1", params![id])?;
            }
            deleted
        } else {
            0
        };

        Ok(deleted as u32)
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

    /// Delete a session record.
    ///
    /// This is NOT how sessions are invalidated — identity changes and peer
    /// device-roster changes ARCHIVE via `SessionRecord::archive_current_state`
    /// so in-transit messages under the old ratchet still decrypt. No
    /// production caller: the live proofs (`tests/validated.rs`) use it to
    /// simulate a device that lost its session.
    pub fn delete_session(&self, address: &str) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let rows_affected =
            conn.execute("DELETE FROM sessions WHERE address = ?1", params![address])?;
        Ok(rows_affected > 0)
    }

    /// Mesh sessions are local-only and can be destroyed on account removal.
    /// The online ratchets have a different address format and are untouched.
    pub fn delete_mesh_sessions(&self, owner: Option<&str>) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        match owner {
            Some(owner) => {
                let prefix = format!("mesh:{owner}:");
                conn.execute(
                    "DELETE FROM sessions WHERE substr(address, 1, length(?1)) = ?1",
                    params![prefix],
                )?;
            }
            None => {
                conn.execute(
                    "DELETE FROM sessions WHERE substr(address, 1, 5) = 'mesh:'",
                    [],
                )?;
            }
        }
        Ok(())
    }

    // `delete_sessions_for_user` is deliberately GONE. It was the §12 defect:
    // a peer device-roster change ran `DELETE FROM sessions WHERE address LIKE
    // 'user:%'`, destroying the archived ratchet states that let in-transit
    // messages decrypt. Both invalidation paths (identity change, roster
    // change) now archive through `list_sessions_for_user` +
    // `SessionRecord::archive_current_state`. If bulk deletion seems needed
    // again, it almost certainly wants to be an archive.

    /// Peers whose ratchets must be rebound when our local device is replaced.
    pub fn session_peer_ids(&self) -> Result<Vec<String>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT DISTINCT substr(address, 1, instr(address, ':') - 1)
             FROM sessions WHERE instr(address, ':') > 1
               AND substr(address, 1, 5) <> 'mesh:' ORDER BY 1",
        )?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::Database)
    }

    /// List all sessions for a user (all devices) as `(address, record)` pairs.
    ///
    /// Address format is "user_id:device_id", so we match on "user_id:%". Used
    /// to archive every device session when the account's identity key changes.
    pub fn list_sessions_for_user(
        &self,
        user_id: &str,
    ) -> Result<Vec<(String, Vec<u8>)>, StorageError> {
        let conn = self.db.connection()?;
        let pattern = format!("{}:%", user_id);
        let mut stmt =
            conn.prepare("SELECT address, record FROM sessions WHERE address LIKE ?1")?;
        let rows = stmt.query_map(params![pattern], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
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
        tracing::debug!(
            sender = sender,
            distribution_id = distribution_id,
            "[KEY_STORE] Loading sender key"
        );

        let conn = self.db.connection()?;
        let mut stmt = conn
            .prepare("SELECT record FROM sender_keys WHERE sender = ?1 AND distribution_id = ?2")?;

        let result: Result<Vec<u8>, _> =
            stmt.query_row(params![sender, distribution_id], |row| row.get(0));

        match result {
            Ok(record) => {
                tracing::info!(
                    sender = sender,
                    distribution_id = distribution_id,
                    record_len = record.len(),
                    "[KEY_STORE] Loaded sender key successfully"
                );
                Ok(Some(record))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                tracing::warn!(
                    sender = sender,
                    distribution_id = distribution_id,
                    "[KEY_STORE] No sender key found for sender/distribution"
                );
                Ok(None)
            }
            Err(e) => {
                tracing::error!(
                    sender = sender,
                    distribution_id = distribution_id,
                    error = %e,
                    "[KEY_STORE] Database error loading sender key"
                );
                Err(StorageError::Database(e))
            }
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
        tracing::info!(
            sender = sender,
            distribution_id = distribution_id,
            record_len = record.len(),
            "[KEY_STORE] Storing sender key"
        );

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

        tracing::info!(
            sender = sender,
            distribution_id = distribution_id,
            "[KEY_STORE] Sender key stored successfully"
        );

        Ok(())
    }

    /// Delete a sender key record.
    ///
    /// Used when rotating keys (e.g., after a member leaves a channel).
    pub fn delete_sender_key(
        &self,
        sender: &str,
        distribution_id: &str,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "DELETE FROM sender_keys WHERE sender = ?1 AND distribution_id = ?2",
            [sender, distribution_id],
        )?;
        Ok(())
    }

    /// Delete all sender keys for a specific distribution ID (channel).
    pub fn delete_sender_keys_for_distribution(
        &self,
        distribution_id: &str,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "DELETE FROM sender_keys WHERE distribution_id = ?1",
            [distribution_id],
        )?;
        // Also clear metadata for this channel
        conn.execute(
            "DELETE FROM sender_key_metadata WHERE channel_id = ?1",
            [distribution_id],
        )?;
        Ok(())
    }

    // ========================================================================
    // Sender Key Rotation Tracking
    // ========================================================================

    /// Maximum age for a sender key before rotation (24 hours in seconds).
    pub const SENDER_KEY_MAX_AGE_SECS: i64 = 24 * 60 * 60;

    /// Maximum messages per sender key before rotation.
    /// Signal uses 2000 messages per sender key chain before rotation.
    pub const SENDER_KEY_MAX_MESSAGES: i64 = 2000;

    /// Initialize sender key metadata when creating a new sender key.
    pub fn init_sender_key_metadata(&self, channel_id: &str) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now().to_rfc3339();

        conn.execute(
            "INSERT INTO sender_key_metadata (channel_id, created_at, message_count, last_used_at)
             VALUES (?1, ?2, 0, ?2)
             ON CONFLICT(channel_id) DO UPDATE SET
                 created_at = excluded.created_at,
                 message_count = 0,
                 last_used_at = excluded.last_used_at",
            params![channel_id, now],
        )?;

        Ok(())
    }

    /// Increment message count for a sender key.
    /// Returns the new message count.
    pub fn increment_sender_key_message_count(
        &self,
        channel_id: &str,
    ) -> Result<i64, StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now().to_rfc3339();

        conn.execute(
            "UPDATE sender_key_metadata
             SET message_count = message_count + 1, last_used_at = ?2
             WHERE channel_id = ?1",
            params![channel_id, now],
        )?;

        let count: i64 = conn
            .query_row(
                "SELECT message_count FROM sender_key_metadata WHERE channel_id = ?1",
                params![channel_id],
                |row| row.get(0),
            )
            .unwrap_or(0);

        Ok(count)
    }

    /// Check if a sender key needs rotation based on age or message count.
    /// Returns (needs_rotation, reason) where reason is "age", "messages", or empty.
    pub fn should_rotate_sender_key(
        &self,
        channel_id: &str,
    ) -> Result<(bool, &'static str), StorageError> {
        let conn = self.db.connection()?;

        let result: Result<(String, i64), _> = conn.query_row(
            "SELECT created_at, message_count FROM sender_key_metadata WHERE channel_id = ?1",
            params![channel_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        );

        match result {
            Ok((created_at_str, message_count)) => {
                // Check message count first (cheaper)
                if message_count >= Self::SENDER_KEY_MAX_MESSAGES {
                    return Ok((true, "messages"));
                }

                // Check age
                if let Ok(created_at) = chrono::DateTime::parse_from_rfc3339(&created_at_str) {
                    let age = Utc::now().signed_duration_since(created_at.with_timezone(&Utc));
                    if age.num_seconds() >= Self::SENDER_KEY_MAX_AGE_SECS {
                        return Ok((true, "age"));
                    }
                }

                Ok((false, ""))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                // No metadata means no key exists, so no rotation needed
                Ok((false, ""))
            }
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Get sender key metadata for a channel.
    /// Returns (created_at, message_count) or None if not found.
    pub fn get_sender_key_metadata(
        &self,
        channel_id: &str,
    ) -> Result<Option<(String, i64)>, StorageError> {
        let conn = self.db.connection()?;

        let result: Result<(String, i64), _> = conn.query_row(
            "SELECT created_at, message_count FROM sender_key_metadata WHERE channel_id = ?1",
            params![channel_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        );

        match result {
            Ok(data) => Ok(Some(data)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    // ── Peer verification ───────────────────────────────────────────────

    /// Check if a peer's safety number has been marked as verified.
    pub fn is_peer_verified(&self, peer_user_id: &str) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let result: Result<bool, _> = conn.query_row(
            "SELECT is_verified FROM peer_verification WHERE peer_user_id = ?1",
            params![peer_user_id],
            |row| row.get::<_, bool>(0),
        );
        match result {
            Ok(v) => Ok(v),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Save or update peer verification state.
    ///
    /// Stores the safety number that was verified so we can detect if the
    /// identity key changes (safety number would differ → auto-unverify).
    pub fn set_peer_verified(
        &self,
        peer_user_id: &str,
        safety_number: &str,
        verified: bool,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now().to_rfc3339();
        let verified_at = if verified { Some(&now) } else { None };

        conn.execute(
            "INSERT INTO peer_verification (peer_user_id, safety_number, is_verified, verified_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(peer_user_id) DO UPDATE SET
                 safety_number = excluded.safety_number,
                 is_verified = excluded.is_verified,
                 verified_at = excluded.verified_at",
            params![peer_user_id, safety_number, verified, verified_at],
        )?;
        Ok(())
    }

    /// Clear a peer's verification flag without touching the stored safety
    /// number.
    ///
    /// Called when a peer's identity key changes: the previously-verified
    /// safety number no longer matches the new key, so the peer must be
    /// re-verified. No-op if the peer was never verified.
    pub fn unverify_peer(&self, peer_user_id: &str) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "UPDATE peer_verification SET is_verified = 0, verified_at = NULL
             WHERE peer_user_id = ?1",
            params![peer_user_id],
        )?;
        Ok(())
    }

    // ── Pending identity changes ────────────────────────────────────────

    /// Park a peer's new, not-yet-accepted identity key.
    ///
    /// libsignal only calls `save_identity` AFTER `is_trusted_identity` passes,
    /// so a changed key is refused everywhere and the protocol itself can never
    /// re-pin it. This row is where the refused key waits for a human decision,
    /// and it is what the SEND path consults to stop encrypting under a stale
    /// session (see `SignalService::ensure_session`).
    ///
    /// First writer wins. A later sighting must not overwrite the key the user
    /// may already be looking at in an accept prompt — otherwise a server that
    /// keeps injecting fresh keys could swap the key out from under the prompt
    /// and have the user accept one they never saw.
    pub fn record_pending_identity_change(
        &self,
        peer_user_id: &str,
        new_identity_key: &[u8],
        source: &str,
        device_id: u32,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT INTO pending_identity_changes
                 (peer_user_id, new_identity_key, source, device_id, first_seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(peer_user_id) DO NOTHING",
            params![
                peer_user_id,
                new_identity_key,
                source,
                device_id,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    /// The peer's parked identity key, as `(key, source, device_id)`.
    pub fn get_pending_identity_change(
        &self,
        peer_user_id: &str,
    ) -> Result<Option<(Vec<u8>, String, u32)>, StorageError> {
        let conn = self.db.connection()?;
        let result: Result<(Vec<u8>, String, u32), _> = conn.query_row(
            "SELECT new_identity_key, source, device_id
             FROM pending_identity_changes WHERE peer_user_id = ?1",
            params![peer_user_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        );
        match result {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Apply a peer's identity-key change as ONE transaction.
    ///
    /// The four writes are a single unit because any partial application is a
    /// worse state than the wedge being cleared. In particular, pinning the new
    /// key while leaving the old sessions in place produces a half-working
    /// conversation: inbound messages start decrypting, but `ensure_session`
    /// sees a session, skips the prekey fetch, and keeps encrypting under a
    /// ratchet negotiated against the DEAD key — so outbound is silently
    /// undeliverable with nothing left to signal that it is wrong. Committing
    /// them together means a crash leaves either the old wedge (recoverable by
    /// re-running) or the accepted state, never that hybrid.
    ///
    /// Sessions are deleted, not archived: they were negotiated against the old
    /// identity key, so nothing under them can be trusted, and an archived state
    /// would only keep failing `is_trusted_identity` on the way in.
    ///
    /// Returns the number of sessions dropped. `Ok(0)` with nothing written when
    /// the key is already pinned, so a re-run cannot clear a verification the
    /// user has since re-done.
    ///
    /// Callers must not use the other `KeyStore` methods to assemble this — the
    /// writer connection is behind a non-reentrant mutex, so the SQL is inlined
    /// against the transaction here.
    pub fn accept_peer_identity_change(
        &self,
        peer_user_id: &str,
        new_identity_key: &[u8],
    ) -> Result<u32, StorageError> {
        let conn = self.db.connection()?;
        let tx = conn.unchecked_transaction()?;

        let pinned: Option<Vec<u8>> = {
            let mut stmt =
                tx.prepare("SELECT identity_key FROM remote_identities WHERE address = ?1")?;
            stmt.query_row(params![peer_user_id], |row| row.get(0)).ok()
        };
        if pinned.as_deref() == Some(new_identity_key) {
            return Ok(0);
        }

        let now = Utc::now().to_rfc3339();
        // Identity keys are pinned per ACCOUNT, so the address is the bare user
        // id (see `SignalStore::is_trusted_identity`).
        tx.execute(
            "INSERT INTO remote_identities (address, identity_key, first_seen_at, updated_at)
             VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(address) DO UPDATE SET
                 identity_key = excluded.identity_key,
                 updated_at = excluded.updated_at",
            params![peer_user_id, new_identity_key, now],
        )?;
        // The safety number the user checked describes the OLD key and says
        // nothing about this one.
        tx.execute(
            "UPDATE peer_verification SET is_verified = 0, verified_at = NULL
             WHERE peer_user_id = ?1",
            params![peer_user_id],
        )?;
        let dropped = tx.execute(
            "DELETE FROM sessions WHERE address LIKE ?1",
            params![format!("{}:%", peer_user_id)],
        )?;
        tx.execute(
            "DELETE FROM pending_identity_changes WHERE peer_user_id = ?1",
            params![peer_user_id],
        )?;

        tx.commit()?;
        Ok(dropped as u32)
    }

    /// Get the stored safety number for a peer (if any).
    ///
    /// Returns `None` if we have never generated a safety number for this peer.
    pub fn get_peer_verification(
        &self,
        peer_user_id: &str,
    ) -> Result<Option<(String, bool)>, StorageError> {
        let conn = self.db.connection()?;
        let result: Result<(String, bool), _> = conn.query_row(
            "SELECT safety_number, is_verified FROM peer_verification WHERE peer_user_id = ?1",
            params![peer_user_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        );
        match result {
            Ok(data) => Ok(Some(data)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }
}

impl KeyStore {
    /// Internal receive CAS read; callers use fixed schema identifiers only.
    pub(crate) fn receive_read(
        &self,
        table: &'static str,
        key: &str,
        column: &'static str,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        let conn = self.db.connection()?;
        Self::receive_read_with_conn(&conn, table, key, column)
    }
    fn receive_read_with_conn(
        conn: &rusqlite::Connection,
        table: &str,
        key: &str,
        column: &str,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        use rusqlite::OptionalExtension;
        let key_column = match (table, column) {
            ("sessions", "record") | ("remote_identities", "identity_key") => "address",
            ("settings", "CAST(value AS BLOB)") if key == "auth.current_user_id" => "key",
            ("prekeys" | "signed_prekeys" | "kyber_prekeys", "record")
            | ("local_identity", "identity_key_pair" | "CAST(registration_id AS BLOB)") => "id",
            _ => {
                return Err(StorageError::InvalidData(
                    "invalid receive observation".into(),
                ));
            }
        };
        Ok(conn
            .query_row(
                &format!("SELECT {column} FROM {table} WHERE {key_column}=?1"),
                [key],
                |r| r.get(0),
            )
            .optional()?)
    }
    pub fn receive_artifact(
        &self,
        key: &crate::storage::receive_artifact::ReceiveKey,
        digest: &[u8],
    ) -> Result<Option<crate::storage::receive_artifact::ReceiveArtifact>, StorageError> {
        let conn = self.db.connection()?;
        Self::check_receive_binding(&conn, key)?;
        crate::storage::receive_artifact::load(&conn, key, digest)
    }
    pub fn pending_receive_artifacts(
        &self,
        account: &str,
        device: u32,
        limit: usize,
    ) -> Result<Vec<crate::storage::receive_artifact::ReceiveArtifact>, StorageError> {
        let conn = self.db.connection()?;
        crate::storage::receive_artifact::list_pending(&conn, account, device, limit)
    }
    fn check_receive_binding(
        conn: &rusqlite::Connection,
        key: &crate::storage::receive_artifact::ReceiveKey,
    ) -> Result<(), StorageError> {
        key.validate()?;
        let account: String = conn.query_row(
            "SELECT value FROM settings WHERE key=?1",
            [crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID],
            |r| r.get(0),
        )?;
        let device: String = conn.query_row(
            "SELECT value FROM settings WHERE key=?1",
            [crate::storage::repositories::settings_repo::keys::CURRENT_DEVICE_ID],
            |r| r.get(0),
        )?;
        if serde_json::from_str::<String>(&account).ok().as_deref()
            != Some(key.local_account.as_str())
            || serde_json::from_str::<u32>(&device).ok() != Some(key.local_device)
        {
            return Err(StorageError::InvalidData(
                "receive account/device changed".into(),
            ));
        }
        Ok(())
    }
    /// Caller must have observed upstream ACK success for this exact binding.
    pub fn acknowledge_receive_artifact(
        &self,
        key: &crate::storage::receive_artifact::ReceiveKey,
        digest: &[u8],
    ) -> Result<(), StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        crate::storage::receive_artifact::mark_acknowledged(&tx, key, digest)?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn check_receive_capacity(&self) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        crate::storage::receive_artifact::check_capacity(&conn)
    }
    pub(crate) fn receive_kyber_used(
        &self,
        kyber: u32,
        ec: u32,
        base: &[u8],
    ) -> Result<bool, StorageError> {
        Ok(self.db.connection()?.query_row("SELECT EXISTS(SELECT 1 FROM kyber_prekey_usage WHERE kyber_prekey_id=?1 AND ec_prekey_id=?2 AND base_key=?3)",params![kyber,ec,base],|r|r.get(0))?)
    }
    /// Commit every libsignal receive mutation and authenticated artifact together.
    /// Never call public KeyStore writers while holding this connection guard.
    pub(crate) fn commit_receive(
        &self,
        key: &crate::storage::receive_artifact::ReceiveKey,
        changes: &crate::storage::receive_artifact::ReceiveChanges,
        artifact: Option<&crate::storage::receive_artifact::ReceiveArtifact>,
    ) -> Result<Option<crate::storage::receive_artifact::ReceiveArtifact>, StorageError> {
        use crate::storage::receive_artifact;
        key.validate()?;
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        Self::check_receive_binding(&tx, key)?;
        if let Some(a) = artifact {
            if let Some(existing) = receive_artifact::load(&tx, key, &a.digest)? {
                return Ok(Some(existing));
            }
        }
        if artifact.is_some() {
            receive_artifact::check_capacity(&tx)?;
            receive_artifact::reclaim_acknowledged(&tx)?;
        }
        for observed in &changes.observed {
            if Self::receive_read_with_conn(&tx, observed.table, &observed.key, observed.column)?
                != observed.value
            {
                return Err(StorageError::InvalidData(
                    "Signal receive base state changed before commit".into(),
                ));
            }
        }
        let now = Utc::now().to_rfc3339();
        for (address, identity) in &changes.identities {
            tx.execute("INSERT INTO remote_identities(address,identity_key,first_seen_at,updated_at) VALUES(?1,?2,?3,?3) ON CONFLICT(address) DO UPDATE SET identity_key=excluded.identity_key,updated_at=excluded.updated_at",params![address,identity,now])?;
        }
        for (id, record) in &changes.signed {
            tx.execute("INSERT INTO signed_prekeys(id,record,created_at) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET record=excluded.record,created_at=excluded.created_at",params![id,record,now])?;
        }
        for (id, record) in &changes.kyber {
            tx.execute("INSERT INTO kyber_prekeys(id,record,created_at) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET record=excluded.record,created_at=excluded.created_at",params![id,record,now])?;
        }
        for (kyber, ec, base) in &changes.usage {
            // Unique key is authoritative: never ignore reused base-key usage.
            tx.execute("INSERT INTO kyber_prekey_usage(kyber_prekey_id,ec_prekey_id,base_key,used_at) VALUES(?1,?2,?3,?4)",params![kyber,ec,base,now])?;
        }
        for (id, record) in &changes.prekeys {
            if let Some(record) = record {
                tx.execute("INSERT INTO prekeys(id,record,created_at) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET record=excluded.record,created_at=excluded.created_at",params![id,record,now])?;
            } else {
                tx.execute("DELETE FROM prekeys WHERE id=?1", [id])?;
            }
        }
        for (address, record) in &changes.sessions {
            tx.execute("INSERT INTO sessions(address,record,updated_at) VALUES(?1,?2,?3) ON CONFLICT(address) DO UPDATE SET record=excluded.record,updated_at=excluded.updated_at",params![address,record,now])?;
        }
        if let Some(a) = artifact {
            receive_artifact::insert(&tx, a)?;
        }
        tx.commit()?;
        Ok(artifact.cloned())
    }
}
