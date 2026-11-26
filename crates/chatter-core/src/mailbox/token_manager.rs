//! Token manager for blind-signed mailbox tokens.
//!
//! Manages the full lifecycle of Chaum blind signature tokens:
//! - Fetching server's RSA public key
//! - Generating ephemeral Ed25519 keypairs
//! - Blinding keys for server signing
//! - Unblinding signatures to create usable tokens
//! - Storing tokens securely in SQLCipher
//!
//! Tokens create ephemeral mailboxes and pay for anonymous sends, drawn from a
//! server-owned daily budget (the server is the only quota authority; see
//! `grant_error`).

use std::sync::Arc;

use chatter_proto::mailbox_v1::GetBlindSignaturePublicKeyResponse;
use chrono::Utc;
use ed25519_dalek::{SigningKey, VerifyingKey};
use prost::Message;
use rusqlite::OptionalExtension;
use uuid::Uuid;

use crate::error::MailboxError;
use crate::mailbox::blind_signature::{blind_key, unblind, ServerPublicKey};
use crate::network::grpc::{AnonymousKtClient, GrpcClientPool, MailboxClient};
use crate::storage::repositories::SettingsRepository;
use crate::storage::Database;

/// Default number of mailbox tokens to request at once.
pub const DEFAULT_TOKEN_REQUEST_COUNT: u32 = 5;

/// Request more mailbox tokens when below this threshold.
pub const MIN_TOKEN_THRESHOLD: u32 = 2;

/// Most tokens one GetMailboxTokens request may ask for (proto `lte: 10`).
pub const MAX_TOKENS_PER_REQUEST: u32 = 10;

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
}

fn credential_error(error: MailboxError) -> crate::error::NetworkError {
    match error {
        MailboxError::NoTokensAvailable => crate::error::NetworkError::AnonymousCredentialsUnavailable,
        other => crate::error::NetworkError::ServerError(other.to_string()),
    }
}

/// Map a GetMailboxTokens failure. The server is the only authority on the
/// daily token budget; it answers ResourceExhausted once that budget is
/// spent. The client used to keep its own counter with a hard-coded ceiling
/// against a server budget of 48 + 2/conversation, so the two disagreed
/// (OPEN-ITEMS §120).
fn grant_error(e: crate::error::NetworkError) -> MailboxError {
    match e {
        crate::error::NetworkError::MailboxQuotaExceeded => {
            tracing::debug!("TokenManager: server reports daily token quota exhausted");
            MailboxError::QuotaExhausted
        }
        e => {
            tracing::error!("TokenManager: get_mailbox_tokens failed: {}", e);
            MailboxError::Network(e.to_string())
        }
    }
}

// Hold the cached manifest stable across fetch, verification and replacement.
static MANIFEST_REFRESH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// The current RPC supplies a single manifest. An unknown, later current key
// must pass the same inclusion and pinned-head checks as a first fetch.
fn manifest_skipped_rotation(
    cached: &GetBlindSignaturePublicKeyResponse,
    offered: &GetBlindSignaturePublicKeyResponse,
) -> Result<bool, MailboxError> {
    if offered.key_id == cached.key_id && offered.public_key == cached.public_key {
        return Ok(false);
    }
    if let Some(next) = &cached.next_key {
        if offered.key_id == next.key_id && offered.public_key == next.public_key {
            return Ok(false);
        }
        if offered.key_id != cached.key_id
            && offered.key_id != next.key_id
            && offered.valid_from > next.valid_from
        {
            return Ok(true);
        }
    }
    tracing::warn!(target: "token_manifest::continuity", "manifest verification failed");
    Err(MailboxError::InvalidServerKey(
        "manifest current key contradicts the cached rotation schedule".into(),
    ))
}

/// Token manager for blind-signed mailbox tokens.
///
/// Handles the complete token lifecycle from server key retrieval through
/// token usage. Stores all state in encrypted SQLCipher database.
pub struct TokenManager {
    /// Database connection for token storage.
    db: Arc<Database>,
    attachment: bool,
}

impl TokenManager {
    /// Create a new TokenManager.
    ///
    /// # Arguments
    ///
    /// * `db` - Shared database connection
    pub fn new(db: Arc<Database>) -> Self {
        Self {
            db,
            attachment: false,
        }
    }

    pub(crate) fn for_attachments(&self) -> Self {
        Self {
            db: self.db.clone(),
            attachment: true,
        }
    }

    fn manifest_key(&self) -> &'static str {
        if self.attachment {
            "attachment.libsignal_key_manifest"
        } else {
            "mailbox.libsignal_key_manifest"
        }
    }

    fn discontinuity_key(&self) -> &'static str {
        if self.attachment {
            "attachment.manifest_discontinuity"
        } else {
            "mailbox.manifest_discontinuity"
        }
    }

    fn verify_purpose(
        &self,
        manifest: &GetBlindSignaturePublicKeyResponse,
    ) -> Result<(), MailboxError> {
        if manifest.attachment != self.attachment {
            return Err(MailboxError::InvalidServerKey(
                "incorrect blind credential purpose".into(),
            ));
        }
        Ok(())
    }

    /// Refresh the server's blind signature public key.
    ///
    /// Fetches the key from the server and caches it locally.
    /// Returns the cached key if still valid.
    ///
    /// # Arguments
    ///
    /// * `client` - MailboxClient for server communication
    /// * `kt` - public KT client on the account transport
    async fn refresh_server_key(
        &self,
        client: &mut MailboxClient,
        kt: &mut AnonymousKtClient,
    ) -> Result<ServerPublicKey, MailboxError> {
        tracing::info!("TokenManager: refresh_server_key called");

        let _refresh = MANIFEST_REFRESH.lock().await;

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

        let (cached, unreadable_cache) = match self.load_cached_manifest() {
            Ok(Some(previous)) => {
                // An expired manifest still commits to its successor.
                if ServerPublicKey::authenticated_manifest_at(&previous, previous.valid_from)
                    .is_ok()
                {
                    (Some(previous), false)
                } else {
                    (None, true)
                }
            }
            Ok(None) => (None, false),
            Err(MailboxError::InvalidServerKey(_)) => (None, true),
            Err(err) => return Err(err),
        };

        // Fetch new key from server with timeout to prevent hangs
        let settings = SettingsRepository::new(self.db.clone());
        let previous = cached.as_ref();
        let (response, server_key) = ServerPublicKey::fetch_manifest(
            |consistency| {
                let mut client = client.clone();
                async move {
                    let response = client.get_blind_signature_public_key(consistency).await
                        .map_err(|error| MailboxError::Network(error.to_string()))?;
                    self.verify_purpose(&response)?;
                    if let Some(previous) = previous {
                        manifest_skipped_rotation(previous, &response)?;
                    }
                    Ok(response)
                }
            },
            &settings,
            kt,
        )
        .await?;
        let discontinuity = match cached.as_ref() {
            Some(previous) if manifest_skipped_rotation(previous, &response)? => {
                Some("skipped_rotation")
            }
            _ if unreadable_cache => Some("unreadable_cache"),
            _ => None,
        };
        // Publish the authenticated key rows, manifest and recovery record together.
        let mut conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        let tx = conn
            .transaction()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        let now = Utc::now().to_rfc3339();
        Self::store_server_key(
            &tx,
            &response.key_id,
            &response.public_key,
            response.valid_until,
        )?;
        if let Some(next) = &response.next_key {
            Self::store_server_key(&tx, &next.key_id, &next.public_key, next.valid_until)?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                self.manifest_key(),
                hex::encode(response.encode_to_vec()),
                now
            ],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;
        if let Some(reason) = discontinuity {
            tx.execute(
                "INSERT OR REPLACE INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![self.discontinuity_key(), reason, now],
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        }
        tx.commit()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        if let Some(reason) = discontinuity {
            tracing::warn!(target: "token_manifest::continuity", reason, "manifest recovered through pinned KT log");
        }
        Ok(server_key)
    }

    /// Cached DER/expiry rows are never a source of trust; verify the signed
    /// manifest again on every load and use its authenticated activation times.
    pub fn get_cached_server_key(&self) -> Result<Option<ServerPublicKey>, MailboxError> {
        let key = match self.load_cached_manifest() {
            Ok(manifest) => manifest.as_ref().and_then(|m| ServerPublicKey::from_manifest(m).ok()),
            Err(MailboxError::InvalidServerKey(_)) => None,
            Err(error) => return Err(error),
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(u64::MAX);
        Ok(key.filter(|k| k.valid_until.saturating_sub(now) > 3_600_000_000_000))
    }

    fn load_cached_manifest(
        &self,
    ) -> Result<Option<GetBlindSignaturePublicKeyResponse>, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        let encoded: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                [self.manifest_key()],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        let Some(encoded) = encoded else {
            return Ok(None);
        };
        let invalid = |e: &dyn std::fmt::Display| {
            MailboxError::InvalidServerKey(format!("could not decode cached manifest: {e}"))
        };
        let bytes = hex::decode(encoded).map_err(|e| invalid(&e))?;
        let manifest = GetBlindSignaturePublicKeyResponse::decode(bytes.as_slice())
            .map_err(|e| invalid(&e))?;
        self.verify_purpose(&manifest)?;
        Ok(Some(manifest))
    }

    /// Store a server public key in the database.
    fn store_server_key(
        conn: &rusqlite::Connection,
        key_id: &str,
        public_key_der: &[u8],
        valid_until: u64,
    ) -> Result<(), MailboxError> {
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
    /// * `pool` - account connection for issuance and manifest verification
    /// * `count` - Number of tokens to request
    ///
    /// # Returns
    ///
    /// The number of tokens successfully acquired.
    pub async fn request_tokens(
        &self,
        pool: &GrpcClientPool,
        count: u32,
    ) -> Result<u32, MailboxError> {
        let mut client = pool
            .mailbox_client()
            .with_attachment_purpose(self.attachment);
        let mut kt = pool.account_kt_client();
        tracing::info!("TokenManager: request_tokens called, count={}", count);

        // No local quota gate: the server owns the daily budget (it clamps the
        // count to what is left and answers ResourceExhausted at zero). A local
        // mirror disagreed with it — see `grant_error`. The request itself is
        // capped at 10 (mailbox.proto GetMailboxTokensRequest.count `lte: 10`); a
        // larger count is rejected outright, so each scheduled batch uses one
        // request at the protocol limit.
        let request_count = count.min(MAX_TOKENS_PER_REQUEST);
        tracing::info!("TokenManager: will request {} tokens", request_count);

        // Get/refresh server key
        tracing::info!("TokenManager: refreshing server key...");
        let server_key = self.refresh_server_key(&mut client, &mut kt).await?;
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

        // The signature response and its blinding secrets belong to this
        // single RPC. A denial or timeout can discard them with the future.
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
                match unblind(&server_key, blinded_sig, op, pub_key) {
                    Ok(signature) => {
                        // Store ready token
                        if self
                            .store_ready_token(
                                id,
                                pub_key,
                                priv_key,
                                &signature,
                                &server_key.key_id,
                            )
                            .is_ok()
                        {
                            success_count += 1;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to unblind signature: {}", e);
                    }
                }
            }
        }

        tracing::info!(
            "TokenManager: request_tokens complete, got {} tokens",
            success_count
        );
        Ok(success_count)
    }

    /// Fixed-rate batches can arrive even when the device has no work.
    /// Drop expired, unused credentials. Claimed tokens can be referenced by
    /// live mailboxes, including a mailbox whose creation is still in flight.
    pub(crate) fn prune_expired_unused_tokens(&self) -> Result<(), MailboxError> {
        let conn = self.db.connection()
            .map_err(|error| MailboxError::Storage(error.to_string()))?;
        let now = Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
        conn.execute(
            "DELETE FROM mailbox_tokens
             WHERE attachment = ?2 AND (used_at IS NULL OR attachment) AND server_key_id IN
                (SELECT key_id FROM mailbox_server_keys WHERE valid_until <= ?1)",
            rusqlite::params![now, self.attachment],
        ).map_err(|error| MailboxError::Storage(error.to_string()))?;
        Ok(())
    }

    /// Store a ready-to-use token.
    fn store_ready_token(
        &self,
        id: &str,
        ephemeral_pk: &[u8],
        ephemeral_sk: &[u8],
        signature: &[u8],
        server_key_id: &str,
    ) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        let now = Utc::now().to_rfc3339();

        conn.execute(
            "INSERT INTO mailbox_tokens
             (id, ephemeral_public_key, ephemeral_private_key, signature, server_key_id, created_at, used_at, attachment)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7)",
            rusqlite::params![
                id,
                ephemeral_pk,
                ephemeral_sk,
                signature,
                server_key_id,
                now,
                self.attachment,
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

        let result: Option<(String, Vec<u8>, Vec<u8>, Vec<u8>)> = conn
            .query_row(
                "SELECT id, ephemeral_public_key, ephemeral_private_key, signature
                 FROM mailbox_tokens WHERE used_at IS NULL AND attachment = ?1 ORDER BY created_at ASC LIMIT 1",
                [self.attachment],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .ok();

        match result {
            Some((id, public_key, private_key, signature)) => Ok(Some(MailboxToken {
                id,
                public_key,
                private_key,
                signature,
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
        Self::claim_available_token_in(&conn, 0, self.attachment)
    }

    fn claim_available_token_in(
        conn: &rusqlite::Connection,
        reserve: usize,
        attachment: bool,
    ) -> Result<Option<MailboxToken>, MailboxError> {
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
                 WHERE t.used_at IS NULL AND k.valid_until > ?2 AND t.attachment = ?4 \
                 ORDER BY t.created_at ASC \
                 LIMIT 1 OFFSET ?3 \
             ) \
             RETURNING id, ephemeral_public_key, ephemeral_private_key, signature",
            rusqlite::params![now, now_nanos, reserve, attachment],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            },
        );

        match result {
            Ok((id, public_key, private_key, signature)) => Ok(Some(MailboxToken {
                id,
                public_key,
                private_key,
                signature,
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
            tracing::warn!(target: "token_claim::no_prefetched_tokens",
                "TokenManager: pre-fetched token batch is empty; refusing to request-then-spend"
            );
            MailboxError::NoTokensAvailable
        })
    }

    pub(crate) fn claim_send_credential(&self) -> Result<MailboxToken, crate::error::NetworkError> {
        self.claim_prefetched_token().map_err(credential_error)
    }

    /// Debit a durable send and claim its credential together. A dry pool
    /// rolls back the debit, including when the eligible tokens are reserved.
    pub(crate) fn claim_budgeted_credential(
        &self,
        reserve: usize,
        debit: impl FnOnce(&rusqlite::Connection) -> Result<(), crate::error::NetworkError>,
    ) -> Result<MailboxToken, crate::error::NetworkError> {
        let mut conn = self.db.connection().map_err(|error| {
            crate::error::NetworkError::ServerError(error.to_string())
        })?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| crate::error::NetworkError::ServerError(error.to_string()))?;
        debit(&tx)?;
        let token = Self::claim_available_token_in(&tx, reserve, self.attachment)
            .map_err(credential_error)?
            .ok_or(crate::error::NetworkError::AnonymousCredentialsUnavailable)?;
        tx.commit().map_err(|error| crate::error::NetworkError::ServerError(error.to_string()))?;
        Ok(token)
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
                "UPDATE mailbox_tokens SET used_at = ?1 WHERE id = ?2 AND used_at IS NULL AND attachment = ?3",
                rusqlite::params![now, token_id, self.attachment],
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        if rows == 0 {
            return Err(MailboxError::TokenNotFound(token_id.to_string()));
        }

        Ok(())
    }

    /// Count claimable tokens: unused, under a signing key that is still
    /// valid — the same filter as `claim_available_token`. Sends can use this
    /// to park locally while the scheduled issuer continues independently.
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
                 WHERE t.used_at IS NULL AND k.valid_until > ?1 AND t.attachment = ?2",
                rusqlite::params![now_nanos, self.attachment],
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
            grant_error(crate::error::NetworkError::MailboxQuotaExceeded),
            MailboxError::QuotaExhausted
        ));
        assert!(matches!(
            grant_error(crate::error::NetworkError::RateLimited),
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
        };
        assert_eq!(token.id, "test-id");
        assert_eq!(token.public_key.len(), 32);
        assert_eq!(token.private_key.len(), 64);
        assert_eq!(token.signature.len(), 256);
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
        TokenManager::store_server_key(
            &tm.db.connection().unwrap(),
            "unsigned",
            b"untrusted DER",
            i64::MAX as u64,
        )
        .unwrap();
        assert!(tm.get_cached_server_key().unwrap().is_none());
        let conn = tm.db.connection().unwrap();
        conn.execute("INSERT INTO settings (key, value, updated_at) VALUES ('mailbox.libsignal_key_manifest', 'corrupt', 'now')", []).unwrap();
        drop(conn);
        assert!(tm.get_cached_server_key().unwrap().is_none());
    }

    #[test]
    fn cached_manifest_propagates_storage_errors() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open(&dir.path().join("tokens.db"), "testpass").unwrap());
        // An unmigrated database has no settings table.
        assert!(matches!(
            TokenManager::new(db).get_cached_server_key(),
            Err(MailboxError::Storage(_))
        ));
    }

    #[test]
    fn cached_manifest_is_reverified_with_its_kt_proof() {
        use crate::mailbox::blind_signature::tests::kt_manifest_vector;
        let Some(m) = kt_manifest_vector() else {
            return;
        };
        let tm = TokenManager::new(test_db());
        let cache = |m: &GetBlindSignaturePublicKeyResponse| {
            tm.db.connection().unwrap().execute(
                "INSERT OR REPLACE INTO settings (key, value, updated_at) VALUES ('mailbox.libsignal_key_manifest', ?1, 'now')",
                [hex::encode(m.encode_to_vec())],
            ).unwrap();
        };
        cache(&m);
        assert_eq!(
            tm.get_cached_server_key().unwrap().unwrap().key_id,
            "vector-current"
        );
        let mut stripped = m.clone();
        stripped.kt_proof = None;
        cache(&stripped);
        assert!(tm.get_cached_server_key().unwrap().is_none());
    }

    #[test]
    fn manifest_continuity_distinguishes_skipped_rotations_from_contradictions() {
        let mut cached = GetBlindSignaturePublicKeyResponse {
            attachment: false,
            key_id: "current".into(),
            public_key: vec![1],
            next_key: Some(Default::default()),
            ..Default::default()
        };
        let next = cached.next_key.as_mut().unwrap();
        next.key_id = "next".into();
        next.public_key = vec![2];
        next.valid_from = 10;
        let mut offered = cached.clone();
        assert!(!manifest_skipped_rotation(&cached, &offered).unwrap());

        offered.key_id = "next".into();
        offered.public_key = vec![2];
        assert!(!manifest_skipped_rotation(&cached, &offered).unwrap());

        offered.public_key = vec![3];
        assert!(manifest_skipped_rotation(&cached, &offered).is_err());
        offered.key_id = "unannounced".into();
        assert!(manifest_skipped_rotation(&cached, &offered).is_err());
        offered.valid_from = 20;
        assert!(manifest_skipped_rotation(&cached, &offered).unwrap());
    }

    #[test]
    fn test_constants() {
        assert_eq!(DEFAULT_TOKEN_REQUEST_COUNT, 5);
        assert_eq!(MIN_TOKEN_THRESHOLD, 2);
    }
}
