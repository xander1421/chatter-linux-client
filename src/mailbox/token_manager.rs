//! Token manager for blind-signed mailbox tokens.
//!
//! Manages the full lifecycle of Chaum blind signature tokens:
//! - Fetching server's RSA public key
//! - Generating ephemeral Ed25519 keypairs
//! - Blinding keys for server signing
//! - Unblinding signatures to create usable tokens
//! - Storing tokens securely in SQLCipher
//! - Tracking daily quota

use std::sync::Arc;

use chrono::Utc;
use ed25519_dalek::{SigningKey, VerifyingKey};
use uuid::Uuid;

use crate::error::MailboxError;
use crate::mailbox::blind_signature::{
    blind_key, unblind, BlindingSecret, MessageRandomizer, ServerPublicKey,
};
use crate::network::grpc::MailboxClient;
use crate::storage::Database;

/// Default number of tokens to request at once.
pub const DEFAULT_TOKEN_REQUEST_COUNT: u32 = 5;

/// Request more tokens when below this threshold.
pub const MIN_TOKEN_THRESHOLD: u32 = 2;

/// A ready-to-use mailbox token.
///
/// Contains the ephemeral Ed25519 keypair and its blind signature.
/// The signature proves the right to create an ephemeral mailbox.
pub struct MailboxToken {
    /// Unique token identifier.
    pub id: String,
    /// Ed25519 public key (32 bytes).
    pub public_key: Vec<u8>,
    /// Ed25519 private key (64 bytes) for signing challenges.
    pub private_key: Vec<u8>,
    /// Unblinded RSA signature over the public key.
    pub signature: Vec<u8>,
    /// Message randomizer for signature verification.
    pub randomizer: Option<Vec<u8>>,
}

/// Quota status for token requests.
#[derive(Debug, Clone)]
pub struct QuotaStatus {
    /// Remaining tokens that can be requested today.
    pub remaining: u32,
    /// When the quota resets (Unix timestamp milliseconds).
    pub resets_at: u64,
}

/// Token manager for blind-signed mailbox tokens.
///
/// Handles the complete token lifecycle from server key retrieval through
/// token usage. Stores all state in encrypted SQLCipher database.
pub struct TokenManager {
    /// Database connection for token storage.
    db: Arc<Database>,
}

impl TokenManager {
    /// Create a new TokenManager.
    ///
    /// # Arguments
    ///
    /// * `db` - Shared database connection
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Refresh the server's blind signature public key.
    ///
    /// Fetches the key from the server and caches it locally.
    /// Returns the cached key if still valid.
    ///
    /// # Arguments
    ///
    /// * `client` - MailboxClient for server communication
    pub async fn refresh_server_key(
        &self,
        client: &mut MailboxClient,
    ) -> Result<ServerPublicKey, MailboxError> {
        // Check for cached valid key first
        if let Some(key) = self.get_cached_server_key()? {
            if key.is_valid() {
                return Ok(key);
            }
        }

        // Fetch new key from server
        let response = client
            .get_blind_signature_public_key()
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        // Parse and validate the key
        let server_key = ServerPublicKey::from_der(
            &response.public_key,
            response.key_id.clone(),
            response.valid_until,
        )?;

        // Store in database
        self.store_server_key(&response.key_id, &response.public_key, response.valid_until)?;

        Ok(server_key)
    }

    /// Get the cached server public key if available and valid.
    pub fn get_cached_server_key(&self) -> Result<Option<ServerPublicKey>, MailboxError> {
        let conn = self.db.connection().map_err(|e| MailboxError::Storage(e.to_string()))?;

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        let result: Option<(String, Vec<u8>, i64)> = conn
            .query_row(
                "SELECT key_id, public_key_der, valid_until FROM mailbox_server_keys
                 WHERE valid_until > ?1 ORDER BY valid_until DESC LIMIT 1",
                rusqlite::params![now_ms],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok();

        match result {
            Some((key_id, der, valid_until)) => {
                let key = ServerPublicKey::from_der(&der, key_id, valid_until as u64)?;
                Ok(Some(key))
            }
            None => Ok(None),
        }
    }

    /// Store a server public key in the database.
    fn store_server_key(
        &self,
        key_id: &str,
        public_key_der: &[u8],
        valid_until: u64,
    ) -> Result<(), MailboxError> {
        let conn = self.db.connection().map_err(|e| MailboxError::Storage(e.to_string()))?;
        let now = Utc::now().to_rfc3339();

        conn.execute(
            "INSERT OR REPLACE INTO mailbox_server_keys (key_id, public_key_der, valid_until, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![key_id, public_key_der, valid_until as i64, now],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;

        Ok(())
    }

    /// Request new tokens from the server.
    ///
    /// Generates ephemeral Ed25519 keypairs, blinds them, sends to server for signing,
    /// unblinds the signatures, and stores the ready tokens.
    ///
    /// # Arguments
    ///
    /// * `client` - MailboxClient for server communication
    /// * `count` - Number of tokens to request
    ///
    /// # Returns
    ///
    /// The number of tokens successfully acquired.
    pub async fn request_tokens(
        &self,
        client: &mut MailboxClient,
        count: u32,
    ) -> Result<u32, MailboxError> {
        // Check quota
        self.reset_quota_if_needed()?;
        let quota = self.get_quota_status()?;
        if quota.remaining == 0 {
            return Err(MailboxError::QuotaExhausted);
        }

        let request_count = count.min(quota.remaining);

        // Get/refresh server key
        let server_key = self.refresh_server_key(client).await?;

        // Generate ephemeral keypairs and blind them
        let mut blinded_keys = Vec::with_capacity(request_count as usize);
        let mut pending_ops = Vec::with_capacity(request_count as usize);

        for _ in 0..request_count {
            // Generate Ed25519 keypair
            // Use raw random bytes to avoid rand_core version conflicts
            let mut secret_bytes = [0u8; 32];
            rand::fill(&mut secret_bytes);
            let signing_key = SigningKey::from_bytes(&secret_bytes);
            let verifying_key = VerifyingKey::from(&signing_key);

            let public_key_bytes = verifying_key.to_bytes().to_vec();
            let private_key_bytes = signing_key.to_keypair_bytes().to_vec();

            // Blind the public key
            let blinding_op = blind_key(&server_key, &public_key_bytes)?;

            blinded_keys.push(blinding_op.blinded_message.clone());
            pending_ops.push((
                Uuid::new_v4().to_string(),
                public_key_bytes,
                private_key_bytes,
                blinding_op,
            ));
        }

        // Store pending blinds
        for (id, pub_key, priv_key, op) in &pending_ops {
            self.store_pending_blind(
                id,
                pub_key,
                priv_key,
                &op.blinded_message,
                &op.secret,
                &op.randomizer,
                &server_key.key_id,
            )?;
        }

        // Request signatures from server
        let response = client
            .get_mailbox_tokens(request_count as i32, blinded_keys)
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        // Update quota from response
        self.update_quota(response.remaining_quota as u32, response.quota_resets_at)?;

        // Process each signature
        let mut success_count = 0;
        for (i, blinded_sig) in response.blinded_signatures.iter().enumerate() {
            if let Some((id, pub_key, priv_key, op)) = pending_ops.get(i) {
                // Unblind the signature
                match unblind(&server_key, blinded_sig, &op.secret, &op.randomizer, pub_key) {
                    Ok(signature) => {
                        // Store ready token
                        if self
                            .store_ready_token(
                                id,
                                pub_key,
                                priv_key,
                                &signature,
                                &op.randomizer,
                                &server_key.key_id,
                            )
                            .is_ok()
                        {
                            // Delete pending blind
                            let _ = self.delete_pending_blind(id);
                            success_count += 1;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to unblind signature: {}", e);
                    }
                }
            }
        }

        Ok(success_count)
    }

    /// Store a pending blinding operation.
    fn store_pending_blind(
        &self,
        id: &str,
        ephemeral_pk: &[u8],
        ephemeral_sk: &[u8],
        blinded_message: &[u8],
        secret: &BlindingSecret,
        randomizer: &MessageRandomizer,
        server_key_id: &str,
    ) -> Result<(), MailboxError> {
        let conn = self.db.connection().map_err(|e| MailboxError::Storage(e.to_string()))?;
        let now = Utc::now().to_rfc3339();
        let randomizer_bytes = randomizer.as_bytes().map(|b| b.to_vec());

        conn.execute(
            "INSERT INTO mailbox_pending_blinds
             (id, ephemeral_public_key, ephemeral_private_key, blinded_message, blinding_secret, message_randomizer, server_key_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                id,
                ephemeral_pk,
                ephemeral_sk,
                blinded_message,
                secret.as_bytes(),
                randomizer_bytes,
                server_key_id,
                now
            ],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;

        Ok(())
    }

    /// Delete a pending blinding operation.
    fn delete_pending_blind(&self, id: &str) -> Result<(), MailboxError> {
        let conn = self.db.connection().map_err(|e| MailboxError::Storage(e.to_string()))?;

        conn.execute("DELETE FROM mailbox_pending_blinds WHERE id = ?1", [id])
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        Ok(())
    }

    /// Store a ready-to-use token.
    fn store_ready_token(
        &self,
        id: &str,
        ephemeral_pk: &[u8],
        ephemeral_sk: &[u8],
        signature: &[u8],
        randomizer: &MessageRandomizer,
        server_key_id: &str,
    ) -> Result<(), MailboxError> {
        let conn = self.db.connection().map_err(|e| MailboxError::Storage(e.to_string()))?;
        let now = Utc::now().to_rfc3339();
        let randomizer_bytes = randomizer.as_bytes().map(|b| b.to_vec());

        conn.execute(
            "INSERT INTO mailbox_tokens
             (id, ephemeral_public_key, ephemeral_private_key, signature, message_randomizer, server_key_id, created_at, used_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
            rusqlite::params![
                id,
                ephemeral_pk,
                ephemeral_sk,
                signature,
                randomizer_bytes,
                server_key_id,
                now
            ],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;

        Ok(())
    }

    /// Get an available (unused) token.
    ///
    /// Returns the oldest unused token or None if no tokens available.
    pub fn get_available_token(&self) -> Result<Option<MailboxToken>, MailboxError> {
        let conn = self.db.connection().map_err(|e| MailboxError::Storage(e.to_string()))?;

        let result: Option<(String, Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>)> = conn
            .query_row(
                "SELECT id, ephemeral_public_key, ephemeral_private_key, signature, message_randomizer
                 FROM mailbox_tokens WHERE used_at IS NULL ORDER BY created_at ASC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .ok();

        match result {
            Some((id, public_key, private_key, signature, randomizer)) => Ok(Some(MailboxToken {
                id,
                public_key,
                private_key,
                signature,
                randomizer,
            })),
            None => Ok(None),
        }
    }

    /// Mark a token as used.
    ///
    /// # Arguments
    ///
    /// * `token_id` - ID of the token to mark as used
    pub fn mark_token_used(&self, token_id: &str) -> Result<(), MailboxError> {
        let conn = self.db.connection().map_err(|e| MailboxError::Storage(e.to_string()))?;
        let now = Utc::now().to_rfc3339();

        let rows = conn
            .execute(
                "UPDATE mailbox_tokens SET used_at = ?1 WHERE id = ?2 AND used_at IS NULL",
                rusqlite::params![now, token_id],
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        if rows == 0 {
            return Err(MailboxError::TokenNotFound(token_id.to_string()));
        }

        Ok(())
    }

    /// Count available (unused) tokens.
    pub fn available_token_count(&self) -> Result<u32, MailboxError> {
        let conn = self.db.connection().map_err(|e| MailboxError::Storage(e.to_string()))?;

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mailbox_tokens WHERE used_at IS NULL",
                [],
                |row| row.get(0),
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        Ok(count as u32)
    }

    /// Get current quota status.
    pub fn get_quota_status(&self) -> Result<QuotaStatus, MailboxError> {
        let conn = self.db.connection().map_err(|e| MailboxError::Storage(e.to_string()))?;

        let result: Option<(i64, i64)> = conn
            .query_row(
                "SELECT tokens_used_today, quota_resets_at FROM mailbox_quota WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();

        match result {
            Some((used_today, resets_at)) => {
                // Default quota: 10 tokens per day (typical server limit)
                let max_quota = 10u32;
                let remaining = max_quota.saturating_sub(used_today as u32);
                Ok(QuotaStatus {
                    remaining,
                    resets_at: resets_at as u64,
                })
            }
            None => {
                // No quota record yet - initialize with full quota
                let tomorrow_midnight = self.next_midnight_ms();
                self.initialize_quota(tomorrow_midnight)?;
                Ok(QuotaStatus {
                    remaining: 10,
                    resets_at: tomorrow_midnight,
                })
            }
        }
    }

    /// Reset quota if the reset time has passed.
    pub fn reset_quota_if_needed(&self) -> Result<(), MailboxError> {
        let conn = self.db.connection().map_err(|e| MailboxError::Storage(e.to_string()))?;

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        let result: Option<i64> = conn
            .query_row(
                "SELECT quota_resets_at FROM mailbox_quota WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .ok();

        if let Some(resets_at) = result {
            if now_ms >= resets_at {
                let tomorrow_midnight = self.next_midnight_ms();
                let now = Utc::now().to_rfc3339();

                conn.execute(
                    "UPDATE mailbox_quota SET tokens_used_today = 0, quota_resets_at = ?1, last_updated = ?2 WHERE id = 1",
                    rusqlite::params![tomorrow_midnight as i64, now],
                )
                .map_err(|e| MailboxError::Storage(e.to_string()))?;
            }
        }

        Ok(())
    }

    /// Initialize quota tracking table.
    fn initialize_quota(&self, resets_at: u64) -> Result<(), MailboxError> {
        let conn = self.db.connection().map_err(|e| MailboxError::Storage(e.to_string()))?;
        let now = Utc::now().to_rfc3339();

        conn.execute(
            "INSERT OR IGNORE INTO mailbox_quota (id, tokens_used_today, quota_resets_at, last_updated)
             VALUES (1, 0, ?1, ?2)",
            rusqlite::params![resets_at as i64, now],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;

        Ok(())
    }

    /// Update quota from server response.
    fn update_quota(&self, remaining: u32, resets_at: u64) -> Result<(), MailboxError> {
        let conn = self.db.connection().map_err(|e| MailboxError::Storage(e.to_string()))?;
        let now = Utc::now().to_rfc3339();

        // Server returns remaining, we need to store used (10 - remaining for max quota 10)
        let max_quota = 10u32;
        let used = max_quota.saturating_sub(remaining);

        conn.execute(
            "INSERT OR REPLACE INTO mailbox_quota (id, tokens_used_today, quota_resets_at, last_updated)
             VALUES (1, ?1, ?2, ?3)",
            rusqlite::params![used as i64, resets_at as i64, now],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;

        Ok(())
    }

    /// Calculate next midnight UTC in milliseconds.
    fn next_midnight_ms(&self) -> u64 {
        use chrono::{Duration, Timelike};
        let now = Utc::now();
        let tomorrow = now + Duration::days(1);
        let midnight = tomorrow
            .with_hour(0)
            .and_then(|t| t.with_minute(0))
            .and_then(|t| t.with_second(0))
            .and_then(|t| t.with_nanosecond(0))
            .unwrap_or(tomorrow);
        midnight.timestamp_millis() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quota_status_structure() {
        let quota = QuotaStatus {
            remaining: 5,
            resets_at: 1234567890000,
        };
        assert_eq!(quota.remaining, 5);
        assert_eq!(quota.resets_at, 1234567890000);
    }

    #[test]
    fn test_mailbox_token_structure() {
        let token = MailboxToken {
            id: "test-id".to_string(),
            public_key: vec![0u8; 32],
            private_key: vec![0u8; 64],
            signature: vec![0u8; 256],
            randomizer: Some(vec![0u8; 32]),
        };
        assert_eq!(token.id, "test-id");
        assert_eq!(token.public_key.len(), 32);
        assert_eq!(token.private_key.len(), 64);
        assert_eq!(token.signature.len(), 256);
        assert!(token.randomizer.is_some());
    }

    #[test]
    fn test_constants() {
        assert_eq!(DEFAULT_TOKEN_REQUEST_COUNT, 5);
        assert_eq!(MIN_TOKEN_THRESHOLD, 2);
    }
}
