//! Token manager for blind-signed mailbox and call room tokens.
//!
//! Manages the full lifecycle of Chaum blind signature tokens:
//! - Fetching server's RSA public key
//! - Generating ephemeral Ed25519 keypairs
//! - Blinding keys for server signing
//! - Unblinding signatures to create usable tokens
//! - Storing tokens securely in SQLCipher
//!
//! Supports two token types, both drawn from ONE server-owned daily budget
//! (the server is the only quota authority; see `grant_error`):
//! - **Mailbox tokens**: For creating ephemeral mailboxes / anonymous sends
//! - **Call room tokens**: For joining anonymous call rooms

use std::sync::Arc;
use std::time::Duration;

use crate::proto::mailbox_v1::GetBlindSignaturePublicKeyResponse;
use chrono::Utc;
use ed25519_dalek::{SigningKey, VerifyingKey};
use prost::Message;
use rusqlite::OptionalExtension;
use uuid::Uuid;

use crate::error::MailboxError;
use crate::mailbox::blind_signature::{
    blind_key, unblind, BlindingSecret, MessageRandomizer, ServerPublicKey,
};
use crate::network::grpc::{AnonymousKtClient, MailboxClient};
use crate::storage::repositories::SettingsRepository;
use crate::storage::Database;

/// Default number of mailbox tokens to request at once.
pub const DEFAULT_TOKEN_REQUEST_COUNT: u32 = 5;

/// Request more mailbox tokens when below this threshold.
pub const MIN_TOKEN_THRESHOLD: u32 = 2;

/// Call-room tokens the refill loop keeps claimable. An outgoing anonymous
/// call spends two (the offer's room and the join); accepting spends one.
pub const DEFAULT_CALL_ROOM_TOKEN_REQUEST_COUNT: u32 = 4;

/// The refill loop tops call-room tokens up once fewer than this remain.
pub const MIN_CALL_ROOM_TOKEN_THRESHOLD: u32 = 2;

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

/// A ready-to-use call room token.
///
/// Contains the ephemeral Ed25519 keypair and its blind signature.
/// The signature proves the right to join/create an anonymous call room.
#[derive(Clone)]
pub struct CallRoomToken {
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

/// Map a GetMailboxTokens failure. The server is the only authority on the
/// daily token budget (mailbox and call-room tokens draw from the same one);
/// it answers ResourceExhausted once that budget is spent. The client used to
/// keep its own counter with a hard-coded ceiling of 10 (5 for call rooms)
/// against a server budget of 48 + 2/conversation, so the two disagreed
/// (OPEN-ITEMS §120).
fn grant_error(e: crate::error::NetworkError) -> MailboxError {
    match e {
        crate::error::NetworkError::RateLimited => {
            tracing::warn!("TokenManager: server reports daily token quota exhausted");
            MailboxError::QuotaExhausted
        }
        e => {
            tracing::error!("TokenManager: get_mailbox_tokens failed: {}", e);
            MailboxError::Network(e.to_string())
        }
    }
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
    /// * `kt` - anonymous KT client for the manifest's consistency proof
    pub async fn refresh_server_key(
        &self,
        client: &mut MailboxClient,
        kt: &mut AnonymousKtClient,
    ) -> Result<ServerPublicKey, MailboxError> {
        tracing::info!("TokenManager: refresh_server_key called");

        // Check for cached valid key first
        tracing::info!("TokenManager: checking for cached server key...");
        if let Some(key) = self.get_cached_server_key()? {
            tracing::info!("TokenManager: found cached key, checking validity...");
            if key.is_valid() {
                tracing::info!("TokenManager: cached key is valid, using it");
                return Ok(key);
            }
            tracing::info!("TokenManager: cached key expired");
        } else {
            tracing::info!("TokenManager: no cached key found");
        }

        // Fetch new key from server with timeout to prevent hangs
        let settings = SettingsRepository::new(self.db.clone());
        let (response, server_key) = ServerPublicKey::fetch_manifest(
            || async {
                tracing::info!("TokenManager: calling get_blind_signature_public_key RPC...");
                let response = tokio::time::timeout(
                    Duration::from_secs(10),
                    client.get_blind_signature_public_key(),
                )
                .await
                .map_err(|_| {
                    tracing::error!("get_blind_signature_public_key timed out after 10s");
                    MailboxError::Network("RPC timeout".to_string())
                })?
                .map_err(|e| {
                    tracing::error!("get_blind_signature_public_key failed: {}", e);
                    MailboxError::Network(e.to_string())
                })?;
                tracing::info!("Got blind signature public key from server");
                Ok(response)
            },
            &settings,
            |old, new| kt.get_consistency_proof(old, new),
        )
        .await?;
        // Only authenticated keys populate the token-expiry join table.
        self.store_server_key(&response.key_id, &response.public_key, response.valid_until)?;
        if let Some(next) = &response.next_key {
            self.store_server_key(&next.key_id, &next.public_key, next.valid_until)?;
        }
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        conn.execute("INSERT OR REPLACE INTO settings (key, value, updated_at) VALUES ('mailbox.signed_key_manifest', ?1, ?2)",
            rusqlite::params![hex::encode(response.encode_to_vec()), Utc::now().to_rfc3339()])
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        Ok(server_key)
    }

    /// Cached DER/expiry rows are never a source of trust; verify the signed
    /// manifest again on every load and use its authenticated activation times.
    pub fn get_cached_server_key(&self) -> Result<Option<ServerPublicKey>, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        let encoded: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key = 'mailbox.signed_key_manifest'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        let Some(encoded) = encoded else {
            return Ok(None);
        };
        let manifest = hex::decode(encoded)
            .ok()
            .and_then(|b| GetBlindSignaturePublicKeyResponse::decode(b.as_slice()).ok());
        let key = manifest
            .as_ref()
            .and_then(|m| ServerPublicKey::from_manifest(m).ok());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(u64::MAX);
        Ok(key.filter(|k| k.valid_until.saturating_sub(now) > 3_600_000_000_000))
    }

    /// Store a server public key in the database.
    fn store_server_key(
        &self,
        key_id: &str,
        public_key_der: &[u8],
        valid_until: u64,
    ) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
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
    /// * `kt` - anonymous KT client (see [`Self::refresh_server_key`])
    /// * `count` - Number of tokens to request
    ///
    /// # Returns
    ///
    /// The number of tokens successfully acquired.
    pub async fn request_tokens(
        &self,
        client: &mut MailboxClient,
        kt: &mut AnonymousKtClient,
        count: u32,
    ) -> Result<u32, MailboxError> {
        tracing::info!("TokenManager: request_tokens called, count={}", count);

        // No local quota gate: the server owns the daily budget (it clamps the
        // count to what is left and answers ResourceExhausted at zero). A local
        // mirror disagreed with it — see `grant_error`.
        let request_count = count;
        tracing::info!("TokenManager: will request {} tokens", request_count);

        // Get/refresh server key
        tracing::info!("TokenManager: refreshing server key...");
        let server_key = self.refresh_server_key(client, kt).await?;
        tracing::info!("TokenManager: got server key, generating keypairs...");

        // Generate ephemeral keypairs and blind them
        let mut blinded_keys = Vec::with_capacity(request_count as usize);
        let mut pending_ops = Vec::with_capacity(request_count as usize);

        for _ in 0..request_count {
            // Generate Ed25519 keypair
            // Use raw random bytes to avoid rand_core version conflicts
            let mut secret_bytes = [0u8; 32];
            {
                use rand::TryRngCore;
                rand::rngs::OsRng
                    .try_fill_bytes(&mut secret_bytes)
                    .expect("OS CSPRNG must not fail");
            }
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

        tracing::info!(
            "TokenManager: generated {} keypairs, storing pending blinds...",
            pending_ops.len()
        );

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

        tracing::info!("TokenManager: requesting signatures from server...");
        // Request signatures from server
        let response = client
            .get_mailbox_tokens(
                request_count as i32,
                blinded_keys,
                server_key.key_id.clone(),
            )
            .await
            .map_err(grant_error)?;
        tracing::info!(
            "TokenManager: got {} signatures from server, server quota remaining={}",
            response.blinded_signatures.len(),
            response.remaining_quota
        );

        // Process each signature
        let mut success_count = 0;
        for (i, blinded_sig) in response.blinded_signatures.iter().enumerate() {
            if let Some((id, pub_key, priv_key, op)) = pending_ops.get(i) {
                // Unblind the signature
                match unblind(
                    &server_key,
                    blinded_sig,
                    &op.secret,
                    &op.randomizer,
                    pub_key,
                ) {
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

        // The server clamps the grant to the remaining budget; blinds past the
        // returned signatures will never be signed.
        for (id, ..) in pending_ops.iter().skip(response.blinded_signatures.len()) {
            let _ = self.delete_pending_blind(id);
        }

        tracing::info!(
            "TokenManager: request_tokens complete, got {} tokens",
            success_count
        );
        Ok(success_count)
    }

    /// Store a pending blinding operation.
    #[allow(clippy::too_many_arguments)]
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
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
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
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

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
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
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
    #[allow(clippy::type_complexity)]
    pub fn get_available_token(&self) -> Result<Option<MailboxToken>, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

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

    /// Atomically claim an available token: returns a token that has been
    /// marked used in the same SQL statement, so concurrent callers never
    /// see the same public key.
    ///
    /// Use this instead of `get_available_token` + `mark_token_used` when the
    /// token is going straight onto the wire — the server rejects any
    /// credential it has seen before, so a race would manifest as a
    /// `permission_denied: credential already used` error.
    pub fn claim_available_token(&self) -> Result<Option<MailboxToken>, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        let now = Utc::now().to_rfc3339();
        // Signing-key validity is stored in nanoseconds. Only claim tokens whose
        // signing key is STILL currently valid: the server verifies anonymous
        // credentials against currently-valid keys only (no grace), so a token
        // signed by a now-expired key would be rejected. Skipping them here means
        // request_tokens re-mints under the current key instead of us burning a
        // send on a dead credential.
        let now_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);

        let result = conn.query_row(
            "UPDATE mailbox_tokens \
             SET used_at = ?1 \
             WHERE id = ( \
                 SELECT t.id FROM mailbox_tokens t \
                 JOIN mailbox_server_keys k ON k.key_id = t.server_key_id \
                 WHERE t.used_at IS NULL AND k.valid_until > ?2 \
                 ORDER BY t.created_at ASC \
                 LIMIT 1 \
             ) \
             RETURNING id, ephemeral_public_key, ephemeral_private_key, signature, message_randomizer",
            rusqlite::params![now, now_nanos],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Option<Vec<u8>>>(4)?,
                ))
            },
        );

        match result {
            Ok((id, public_key, private_key, signature, randomizer)) => Ok(Some(MailboxToken {
                id,
                public_key,
                private_key,
                signature,
                randomizer,
            })),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(MailboxError::Storage(e.to_string())),
        }
    }

    /// Claim a token from the pre-fetched batch, and ONLY from it.
    ///
    /// The one way a send spends a mailbox token. It never asks the server
    /// for more: an authenticated `GetMailboxTokens` followed a moment later
    /// by the anonymous open that spends its output links the two in time,
    /// which is exactly what the blind signature exists to prevent (NDSS'21
    /// §VI-F). The batch is refilled on its own jittered schedule, decoupled
    /// from sends (`CoreServices::spawn_token_refill`); an empty batch fails
    /// the send with `NoTokensAvailable`.
    pub fn claim_prefetched_token(&self) -> Result<MailboxToken, MailboxError> {
        self.claim_available_token()?.ok_or_else(|| {
            tracing::warn!(
                "TokenManager: pre-fetched token batch is empty; refusing to request-then-spend"
            );
            MailboxError::NoTokensAvailable
        })
    }

    /// Mark a token as used.
    ///
    /// # Arguments
    ///
    /// * `token_id` - ID of the token to mark as used
    pub fn mark_token_used(&self, token_id: &str) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
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

    /// Count claimable tokens: unused, under a signing key that is still
    /// valid — the same filter as `claim_available_token`. Counting dead
    /// tokens would let the refill think the batch is full while every
    /// claim comes back empty.
    pub fn available_token_count(&self) -> Result<u32, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        let now_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mailbox_tokens t \
                 JOIN mailbox_server_keys k ON k.key_id = t.server_key_id \
                 WHERE t.used_at IS NULL AND k.valid_until > ?1",
                [now_nanos],
                |row| row.get(0),
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        Ok(count as u32)
    }

    // ========================================================================
    // Call Room Token Methods
    // ========================================================================

    /// Request new call room tokens from the server.
    ///
    /// Uses the same blind signature infrastructure and server budget as
    /// mailbox tokens, with separate storage tables.
    ///
    /// # Arguments
    ///
    /// * `client` - MailboxClient for server communication
    /// * `kt` - anonymous KT client (see [`Self::refresh_server_key`])
    /// * `count` - Number of tokens to request
    ///
    /// # Returns
    ///
    /// The number of tokens successfully acquired.
    pub async fn request_call_room_tokens(
        &self,
        client: &mut MailboxClient,
        kt: &mut AnonymousKtClient,
        count: u32,
    ) -> Result<u32, MailboxError> {
        tracing::info!(
            "TokenManager: request_call_room_tokens called, count={}",
            count
        );

        // Same server-owned budget as mailbox tokens; no local gate.
        let request_count = count;
        tracing::info!(
            "TokenManager: will request {} call room tokens",
            request_count
        );

        // Get/refresh server key (shared with mailbox tokens)
        let server_key = self.refresh_server_key(client, kt).await?;

        // Generate ephemeral keypairs and blind them
        let mut blinded_keys = Vec::with_capacity(request_count as usize);
        let mut pending_ops = Vec::with_capacity(request_count as usize);

        for _ in 0..request_count {
            let mut secret_bytes = [0u8; 32];
            {
                use rand::TryRngCore;
                rand::rngs::OsRng
                    .try_fill_bytes(&mut secret_bytes)
                    .expect("OS CSPRNG must not fail");
            }
            let signing_key = SigningKey::from_bytes(&secret_bytes);
            let verifying_key = VerifyingKey::from(&signing_key);

            let public_key_bytes = verifying_key.to_bytes().to_vec();
            let private_key_bytes = signing_key.to_keypair_bytes().to_vec();

            let blinding_op = blind_key(&server_key, &public_key_bytes)?;

            blinded_keys.push(blinding_op.blinded_message.clone());
            pending_ops.push((
                Uuid::new_v4().to_string(),
                public_key_bytes,
                private_key_bytes,
                blinding_op,
            ));
        }

        // Store pending blinds in call_room_pending_blinds table
        for (id, pub_key, priv_key, op) in &pending_ops {
            self.store_call_room_pending_blind(
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
            .get_mailbox_tokens(
                request_count as i32,
                blinded_keys,
                server_key.key_id.clone(),
            )
            .await
            .map_err(grant_error)?;
        tracing::info!(
            "TokenManager: got {} call room signatures, server quota remaining={}",
            response.blinded_signatures.len(),
            response.remaining_quota
        );

        // Process each signature
        let mut success_count = 0;
        for (i, blinded_sig) in response.blinded_signatures.iter().enumerate() {
            if let Some((id, pub_key, priv_key, op)) = pending_ops.get(i) {
                match unblind(
                    &server_key,
                    blinded_sig,
                    &op.secret,
                    &op.randomizer,
                    pub_key,
                ) {
                    Ok(signature) => {
                        if self
                            .store_call_room_ready_token(
                                id,
                                pub_key,
                                priv_key,
                                &signature,
                                &op.randomizer,
                                &server_key.key_id,
                            )
                            .is_ok()
                        {
                            let _ = self.delete_call_room_pending_blind(id);
                            success_count += 1;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to unblind call room signature: {}", e);
                    }
                }
            }
        }

        for (id, ..) in pending_ops.iter().skip(response.blinded_signatures.len()) {
            let _ = self.delete_call_room_pending_blind(id);
        }

        tracing::info!(
            "TokenManager: request_call_room_tokens complete, got {} tokens",
            success_count
        );
        Ok(success_count)
    }

    /// Store a pending call room blinding operation.
    #[allow(clippy::too_many_arguments)]
    fn store_call_room_pending_blind(
        &self,
        id: &str,
        ephemeral_pk: &[u8],
        ephemeral_sk: &[u8],
        blinded_message: &[u8],
        secret: &BlindingSecret,
        randomizer: &MessageRandomizer,
        server_key_id: &str,
    ) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        let now = Utc::now().to_rfc3339();
        let randomizer_bytes = randomizer.as_bytes().map(|b| b.to_vec());

        conn.execute(
            "INSERT INTO call_room_pending_blinds
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

    /// Delete a pending call room blinding operation.
    fn delete_call_room_pending_blind(&self, id: &str) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        conn.execute("DELETE FROM call_room_pending_blinds WHERE id = ?1", [id])
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        Ok(())
    }

    /// Store a ready-to-use call room token.
    fn store_call_room_ready_token(
        &self,
        id: &str,
        ephemeral_pk: &[u8],
        ephemeral_sk: &[u8],
        signature: &[u8],
        randomizer: &MessageRandomizer,
        server_key_id: &str,
    ) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        let now = Utc::now().to_rfc3339();
        let randomizer_bytes = randomizer.as_bytes().map(|b| b.to_vec());

        conn.execute(
            "INSERT INTO call_room_tokens
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

    /// Claim a call-room token from the pre-fetched pool, and ONLY from it.
    ///
    /// The call-room twin of `claim_prefetched_token`: the one way a call
    /// spends a call-room token. Marked used in the same statement, so two
    /// concurrent calls never get the same key. Never asks the server for
    /// more (NDSS'21 §VI-F); the pool is refilled by
    /// `CoreServices::spawn_token_refill`, and an empty pool fails the call
    /// with `NoTokensAvailable`. Tokens under an expired signing key are
    /// skipped, as the server would reject them.
    pub fn claim_prefetched_call_room_token(&self) -> Result<CallRoomToken, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        let now = Utc::now().to_rfc3339();
        let now_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);

        let result = conn.query_row(
            "UPDATE call_room_tokens \
             SET used_at = ?1 \
             WHERE id = ( \
                 SELECT t.id FROM call_room_tokens t \
                 JOIN mailbox_server_keys k ON k.key_id = t.server_key_id \
                 WHERE t.used_at IS NULL AND k.valid_until > ?2 \
                 ORDER BY t.created_at ASC \
                 LIMIT 1 \
             ) \
             RETURNING id, ephemeral_public_key, ephemeral_private_key, signature, message_randomizer",
            rusqlite::params![now, now_nanos],
            |row| {
                Ok(CallRoomToken {
                    id: row.get(0)?,
                    public_key: row.get(1)?,
                    private_key: row.get(2)?,
                    signature: row.get(3)?,
                    randomizer: row.get(4)?,
                })
            },
        );

        match result {
            Ok(token) => Ok(token),
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                tracing::warn!("TokenManager: pre-fetched call-room pool is empty; refusing to request-then-spend");
                Err(MailboxError::NoTokensAvailable)
            }
            Err(e) => Err(MailboxError::Storage(e.to_string())),
        }
    }

    /// Count claimable call-room tokens (same filter as
    /// `claim_prefetched_call_room_token`).
    pub fn available_call_room_token_count(&self) -> Result<u32, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        let now_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM call_room_tokens t \
                 JOIN mailbox_server_keys k ON k.key_id = t.server_key_id \
                 WHERE t.used_at IS NULL AND k.valid_until > ?1",
                [now_nanos],
                |row| row.get(0),
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        Ok(count as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grant_error_maps_exhaustion() {
        assert!(matches!(
            grant_error(crate::error::NetworkError::RateLimited),
            MailboxError::QuotaExhausted
        ));
        assert!(matches!(
            grant_error(crate::error::NetworkError::Unavailable),
            MailboxError::Network(_)
        ));
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

    fn test_db() -> Arc<Database> {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(Database::open(&dir.path().join("tokens.db"), "testpass").expect("open"));
        db.run_migrations().expect("migrations");
        crate::test_tempdir::hold(dir);
        db
    }

    #[test]
    fn unsigned_or_corrupted_cache_is_never_used_for_blinding() {
        let tm = TokenManager::new(test_db());
        tm.store_server_key("unsigned", b"untrusted DER", i64::MAX as u64)
            .unwrap();
        assert!(tm.get_cached_server_key().unwrap().is_none());
        let conn = tm.db.connection().unwrap();
        conn.execute("INSERT INTO settings (key, value, updated_at) VALUES ('mailbox.signed_key_manifest', 'corrupt', 'now')", []).unwrap();
        drop(conn);
        assert!(tm.get_cached_server_key().unwrap().is_none());
    }

    #[test]
    fn cached_manifest_is_reverified_with_its_kt_proof() {
        use crate::mailbox::blind_signature::tests::kt_manifest_vector;
        let Some(m) = kt_manifest_vector() else { return };
        let tm = TokenManager::new(test_db());
        let cache = |m: &GetBlindSignaturePublicKeyResponse| {
            tm.db.connection().unwrap().execute(
                "INSERT OR REPLACE INTO settings (key, value, updated_at) VALUES ('mailbox.signed_key_manifest', ?1, 'now')",
                [hex::encode(m.encode_to_vec())],
            ).unwrap();
        };
        cache(&m);
        assert_eq!(tm.get_cached_server_key().unwrap().unwrap().key_id, "vector-current");
        let mut stripped = m.clone();
        stripped.kt_proof = None;
        cache(&stripped);
        assert!(tm.get_cached_server_key().unwrap().is_none());
    }

    #[test]
    fn call_room_claim_spends_only_the_prefetched_pool() {
        let tm = TokenManager::new(test_db());
        let far = i64::MAX / 2;
        tm.store_server_key("live", b"der", far as u64).unwrap();
        tm.store_server_key("dead", b"der", 1).unwrap();
        let conn = tm.db.connection().unwrap();
        for (id, key) in [("t-live", "live"), ("t-dead", "dead")] {
            conn.execute(
                "INSERT INTO call_room_tokens
                 (id, ephemeral_public_key, ephemeral_private_key, signature, message_randomizer, server_key_id, created_at, used_at)
                 VALUES (?1, CAST(?1 AS BLOB), x'00', x'00', NULL, ?2, ?3, NULL)",
                rusqlite::params![id, key, Utc::now().to_rfc3339()],
            )
            .unwrap();
        }
        drop(conn);

        // A token under an expired signing key is neither counted nor claimed.
        assert_eq!(tm.available_call_room_token_count().unwrap(), 1);
        assert_eq!(tm.claim_prefetched_call_room_token().unwrap().id, "t-live");
        // Claimed means spent: the pool is now empty and says so.
        assert_eq!(tm.available_call_room_token_count().unwrap(), 0);
        assert!(matches!(
            tm.claim_prefetched_call_room_token(),
            Err(MailboxError::NoTokensAvailable)
        ));
    }

    #[test]
    fn test_constants() {
        assert_eq!(DEFAULT_TOKEN_REQUEST_COUNT, 5);
        assert_eq!(MIN_TOKEN_THRESHOLD, 2);
    }
}
