//! Ephemeral mailbox manager for NDSS '21 privacy-preserving messaging.
//!
//! Manages the lifecycle of ephemeral mailboxes including:
//! - Anonymous token acquisition via blind signatures
//! - Mailbox creation/deletion
//! - Sending messages via ephemeral mailboxes
//! - Polling mailboxes for incoming messages

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use ed25519_dalek::{SigningKey, VerifyingKey};

use crate::error::{CryptoError, NetworkError};
use crate::mailbox::blind_signature::{
    blind_key, unblind, BlindingOperation, BlindingSecret, MessageRandomizer, ServerPublicKey,
};
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::mailbox_client::MailboxClient;
use crate::proto::mailbox_v1::{self, EnvelopeType};
use crate::storage::repositories::settings_repo::SettingsRepository;

/// Errors that can occur during mailbox operations.
#[derive(Debug, thiserror::Error)]
pub enum MailboxError {
    /// Cryptographic operation failed.
    #[error("crypto error: {0}")]
    Crypto(#[from] CryptoError),

    /// Network operation failed.
    #[error("network error: {0}")]
    Network(#[from] NetworkError),

    /// No mailbox token available.
    #[error("no mailbox tokens available")]
    NoTokensAvailable,

    /// Mailbox not found.
    #[error("mailbox not found for conversation: {0}")]
    MailboxNotFound(String),

    /// Mailbox expired.
    #[error("mailbox expired")]
    MailboxExpired,

    /// Server public key not available.
    #[error("server blind signature public key not available")]
    ServerKeyNotAvailable,
}

/// Represents a blinded token that can be used to create a mailbox.
#[derive(Clone)]
struct MailboxToken {
    /// Ephemeral Ed25519 public key (unblinded)
    public_key: VerifyingKey,
    /// Private key for the ephemeral mailbox
    private_key: SigningKey,
    /// Blinding secret needed for unblinding (kept until server responds)
    blinding_secret: BlindingSecret,
    /// Message randomizer from blinding operation
    /// Note: With deterministic mode (NDSS '21 spec), this is typically None
    /// and server verifies signature directly against public_key
    msg_randomizer: MessageRandomizer,
    /// Unblinded signature (proof of authorization)
    signature: Option<Vec<u8>>,
    /// Timestamp when token was acquired (Unix seconds).
    acquired_at: u64,
}

/// Represents an active ephemeral mailbox.
#[derive(Clone)]
pub struct ActiveMailbox {
    /// Mailbox ID (opaque server identifier)
    pub mailbox_id: String,
    /// Public key of the mailbox
    pub public_key: VerifyingKey,
    /// Private key for authentication
    private_key: SigningKey,
    /// When the mailbox expires
    pub expires_at: u64,
    /// Conversation this mailbox is for
    pub conversation_id: String,
}

// Settings key for cached server blind signature public key.
const SERVER_BLIND_SIG_PUBKEY: &str = "mailbox.server_blind_sig_pubkey";

// Maximum number of tokens to keep in the pool.
// Prevents unbounded accumulation when tokens are acquired faster than used.
const MAX_TOKEN_POOL_SIZE: usize = 50;

// Maximum number of active mailboxes to track.
// Oldest mailboxes are evicted when this limit is exceeded.
const MAX_ACTIVE_MAILBOXES: usize = 100;

// Token expiry duration in seconds (24 hours).
// Tokens older than this are automatically discarded.
const TOKEN_EXPIRY_SECS: u64 = 24 * 60 * 60;

/// Manages ephemeral mailboxes for privacy-preserving messaging.
///
/// Implements the NDSS '21 mailbox system for sender anonymity.
pub struct MailboxManager {
    /// gRPC client pool (for creating MailboxClient instances)
    client_pool: Arc<GrpcClientPool>,
    /// Settings repository (for persisting server public key)
    settings_repo: Arc<SettingsRepository>,
    /// Cached server blind signature public key (uses deterministic RSABSSA per NDSS '21)
    server_pubkey: Arc<RwLock<Option<ServerPublicKey>>>,
    /// Pool of available unblinded tokens
    token_pool: Arc<RwLock<Vec<MailboxToken>>>,
    /// Active mailboxes per conversation
    active_mailboxes: Arc<RwLock<HashMap<String, ActiveMailbox>>>,
    /// LRU order for mailbox eviction (most recent at back).
    mailbox_lru: Arc<RwLock<VecDeque<String>>>,
}

impl MailboxManager {
    /// Create a new MailboxManager.
    pub fn new(
        client_pool: Arc<GrpcClientPool>,
        settings_repo: Arc<SettingsRepository>,
    ) -> Self {
        Self {
            client_pool,
            settings_repo,
            server_pubkey: Arc::new(RwLock::new(None)),
            token_pool: Arc::new(RwLock::new(Vec::new())),
            active_mailboxes: Arc::new(RwLock::new(HashMap::new())),
            mailbox_lru: Arc::new(RwLock::new(VecDeque::new())),
        }
    }

    /// Initialize the manager by loading or fetching the server's blind signature public key.
    ///
    /// First checks settings cache, then fetches from server if not cached.
    /// Uses deterministic RSABSSA variant per NDSS '21 specification.
    pub async fn initialize(&self) -> Result<(), MailboxError> {
        // Try loading from cache first (stored as base64)
        if let Ok(Some(cached_b64)) = self.settings_repo.get_string(SERVER_BLIND_SIG_PUBKEY) {
            if let Ok(cached_key) = BASE64.decode(&cached_b64) {
                // Use a default key_id and far-future expiry for cached keys
                // In production, these should be stored alongside the key
                if let Ok(server_pk) = ServerPublicKey::from_der(&cached_key, "cached".to_string(), u64::MAX) {
                    *self.server_pubkey.write().await = Some(server_pk);
                    tracing::debug!("Loaded blind signature public key from cache");
                    return Ok(());
                }
            }
        }

        // Fetch from server
        let mut client = self.create_mailbox_client();
        let response = client.get_blind_signature_public_key().await?;

        // Parse and cache in memory - use server-provided key_id and valid_until
        let server_pk = ServerPublicKey::from_der(
            &response.public_key,
            response.key_id.clone(),
            response.valid_until,
        ).map_err(|e| MailboxError::Crypto(CryptoError::InvalidKey(e.to_string())))?;
        *self.server_pubkey.write().await = Some(server_pk);

        // Persist to settings for next startup (as base64)
        let key_b64 = BASE64.encode(&response.public_key);
        if let Err(e) = self.settings_repo.set_string(SERVER_BLIND_SIG_PUBKEY, &key_b64) {
            tracing::warn!("Failed to cache blind signature public key: {}", e);
        }

        tracing::debug!("Fetched and cached blind signature public key from server");
        Ok(())
    }

    /// Acquire mailbox tokens from the server.
    ///
    /// Generates ephemeral keypairs, blinds them, requests blind signatures,
    /// and stores the unblinded tokens for later mailbox creation.
    /// Uses deterministic RSABSSA per NDSS '21 / RFC 9474.
    ///
    /// # Arguments
    ///
    /// * `count` - Number of tokens to acquire (max 10 per request)
    pub async fn acquire_tokens(&self, count: usize) -> Result<(), MailboxError> {
        if count == 0 || count > 10 {
            return Err(MailboxError::Crypto(CryptoError::InvalidKey(
                "Token count must be 1-10".to_string(),
            )));
        }

        // Get server public key (clone it so we can use it after releasing the lock)
        let server_pk = self
            .server_pubkey
            .read()
            .await
            .clone()
            .ok_or(MailboxError::ServerKeyNotAvailable)?;

        // Generate ephemeral keypairs and blind them
        let mut tokens = Vec::new();
        let mut blinded_keys = Vec::new();

        for _ in 0..count {
            // Generate Ed25519 keypair for this mailbox
            let mut secret_bytes = [0u8; 32];
            rand::RngCore::fill_bytes(&mut rand::rng(), &mut secret_bytes);
            let private_key = SigningKey::from_bytes(&secret_bytes);
            let public_key = private_key.verifying_key();

            // Blind the public key using deterministic RSABSSA
            let BlindingOperation {
                blinded_message,
                secret,
                randomizer,
            } = blind_key(&server_pk, public_key.as_bytes())
                .map_err(|e| MailboxError::Crypto(CryptoError::BlindSignatureError(e.to_string())))?;

            blinded_keys.push(blinded_message);

            let now_secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            tokens.push(MailboxToken {
                public_key,
                private_key,
                blinding_secret: secret,
                msg_randomizer: randomizer,
                signature: None,
                acquired_at: now_secs,
            });
        }

        // Request blind signatures from server
        let mut client = self.create_mailbox_client();
        let response = client
            .get_mailbox_tokens(count as i32, blinded_keys)
            .await?;

        // Unblind the signatures
        for (token, blinded_sig) in tokens.iter_mut().zip(response.blinded_signatures.iter()) {
            let signature = unblind(
                &server_pk,
                blinded_sig,
                &token.blinding_secret,
                &token.msg_randomizer,
                token.public_key.as_bytes(),
            ).map_err(|e| MailboxError::Crypto(CryptoError::BlindSignatureError(e.to_string())))?;

            token.signature = Some(signature);
        }

        // Add to token pool with bounds checking
        {
            let mut pool = self.token_pool.write().await;

            // First, remove any expired tokens
            let now_secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            pool.retain(|t| now_secs.saturating_sub(t.acquired_at) < TOKEN_EXPIRY_SECS);

            // Add new tokens
            pool.extend(tokens);

            // Truncate to max size (keep newest - they're at the end)
            if pool.len() > MAX_TOKEN_POOL_SIZE {
                let excess = pool.len() - MAX_TOKEN_POOL_SIZE;
                pool.drain(0..excess);
            }
        }

        Ok(())
    }

    /// Get or create a mailbox for a conversation.
    ///
    /// If a mailbox already exists and is not expired, returns it.
    /// Otherwise, creates a new one using an available token.
    ///
    /// # Arguments
    ///
    /// * `conversation_id` - Unique identifier for the conversation
    /// * `ttl_seconds` - Requested mailbox TTL (server may cap at 30 days)
    pub async fn get_or_create_mailbox(
        &self,
        conversation_id: &str,
        ttl_seconds: u64,
    ) -> Result<ActiveMailbox, MailboxError> {
        // Check if we have an active mailbox
        {
            let mailboxes = self.active_mailboxes.read().await;
            if let Some(mailbox) = mailboxes.get(conversation_id) {
                // Check if it's still valid (expires_at is in nanoseconds per proto)
                let now_nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as u64;

                if mailbox.expires_at > now_nanos {
                    return Ok(mailbox.clone());
                }
            }
        }

        // Need to create a new mailbox - get a token from the pool
        let token = {
            let mut pool = self.token_pool.write().await;

            // Remove expired tokens first
            let now_secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            pool.retain(|t| now_secs.saturating_sub(t.acquired_at) < TOKEN_EXPIRY_SECS);

            pool.pop().ok_or(MailboxError::NoTokensAvailable)?
        };

        // Create the mailbox anonymously using deterministic RSABSSA proof
        // Per NDSS '21 spec, server verifies signature directly against public_key
        // (no message_randomizer needed with deterministic mode)
        let signature = token.signature.ok_or(MailboxError::NoTokensAvailable)?;
        let message_randomizer = token.msg_randomizer.as_bytes().map(|b| b.to_vec()).unwrap_or_default();
        
        let mut client = self.create_mailbox_client();
        let response = client
            .create_ephemeral_mailbox(
                token.public_key.as_bytes().to_vec(),
                signature,
                ttl_seconds,
                message_randomizer,
            )
            .await?;

        if !response.success {
            return Err(MailboxError::Network(NetworkError::ServerError(
                response.error,
            )));
        }

        // Store the active mailbox with LRU tracking
        let mailbox = ActiveMailbox {
            mailbox_id: response.mailbox_id,
            public_key: token.public_key,
            private_key: token.private_key,
            expires_at: response.expires_at,
            conversation_id: conversation_id.to_string(),
        };

        {
            let mut mailboxes = self.active_mailboxes.write().await;
            let mut lru = self.mailbox_lru.write().await;

            // Update LRU order
            lru.retain(|id| id != conversation_id);
            lru.push_back(conversation_id.to_string());

            // Evict oldest mailboxes if over limit
            while lru.len() > MAX_ACTIVE_MAILBOXES {
                if let Some(oldest) = lru.pop_front() {
                    mailboxes.remove(&oldest);
                }
            }

            mailboxes.insert(conversation_id.to_string(), mailbox.clone());
        }

        Ok(mailbox)
    }

    /// Send a message to an ephemeral mailbox.
    ///
    /// # Arguments
    ///
    /// * `destination_public_key` - Public key of the destination mailbox
    /// * `encrypted_envelope` - Encrypted message envelope (sealed sender format)
    /// * `envelope_type` - Type of envelope (SealedSender, PrekeyMessage, etc.)
    pub async fn send_to_mailbox(
        &self,
        destination_public_key: &[u8],
        encrypted_envelope: Vec<u8>,
        envelope_type: EnvelopeType,
    ) -> Result<(), MailboxError> {
        let mut client = self.create_mailbox_client();
        
        let _response = client
            .send_to_ephemeral_mailbox(
                destination_public_key.to_vec(),
                encrypted_envelope,
                envelope_type as i32,
            )
            .await?;

        Ok(())
    }

    /// Poll a mailbox for new messages.
    ///
    /// # Arguments
    ///
    /// * `conversation_id` - Conversation to poll mailbox for
    /// * `after_message_id` - Only return messages after this ID
    /// * `limit` - Max messages to return
    pub async fn poll_mailbox(
        &self,
        conversation_id: &str,
        after_message_id: Option<&str>,
        limit: i32,
    ) -> Result<Vec<mailbox_v1::EphemeralMessage>, MailboxError> {
        // Get the active mailbox
        let mailbox = {
            let mailboxes = self.active_mailboxes.read().await;
            mailboxes
                .get(conversation_id)
                .cloned()
                .ok_or_else(|| MailboxError::MailboxNotFound(conversation_id.to_string()))?
        };

        // Get challenge from server
        let mut client = self.create_mailbox_client();
        let challenge_response = client
            .get_mailbox_challenge(mailbox.public_key.as_bytes().to_vec())
            .await?;

        // Sign the challenge with our private key
        use ed25519_dalek::Signer;
        let challenge_sig = mailbox.private_key.sign(&challenge_response.challenge);

        // Poll with signed challenge
        let response = client
            .poll_ephemeral_mailbox(
                mailbox.public_key.as_bytes().to_vec(),
                challenge_sig.to_bytes().to_vec(),
                challenge_response.challenge.clone(),
                after_message_id,
                limit,
            )
            .await?;

        Ok(response.messages)
    }

    /// Acknowledge (delete) messages from a mailbox.
    ///
    /// # Arguments
    ///
    /// * `conversation_id` - Conversation whose mailbox to acknowledge messages from
    /// * `message_ids` - IDs of messages to acknowledge (will be deleted)
    pub async fn acknowledge_messages(
        &self,
        conversation_id: &str,
        message_ids: Vec<String>,
    ) -> Result<(), MailboxError> {
        let mailbox = {
            let mailboxes = self.active_mailboxes.read().await;
            mailboxes
                .get(conversation_id)
                .cloned()
                .ok_or_else(|| MailboxError::MailboxNotFound(conversation_id.to_string()))?
        };

        // Get challenge
        let mut client = self.create_mailbox_client();
        let challenge_response = client
            .get_mailbox_challenge(mailbox.public_key.as_bytes().to_vec())
            .await?;

        // Sign challenge
        use ed25519_dalek::Signer;
        let challenge_sig = mailbox.private_key.sign(&challenge_response.challenge);

        // Acknowledge messages
        let _response = client
            .acknowledge_messages(
                mailbox.public_key.as_bytes().to_vec(),
                challenge_sig.to_bytes().to_vec(),
                challenge_response.challenge,
                message_ids,
            )
            .await?;

        Ok(())
    }

    /// Helper to create a MailboxClient from the pool.
    fn create_mailbox_client(&self) -> MailboxClient {
        self.client_pool.mailbox_client()
    }

    /// Get the number of tokens currently available in the pool.
    pub async fn token_count(&self) -> usize {
        self.token_pool.read().await.len()
    }

    /// Get the number of active mailboxes.
    pub async fn active_mailbox_count(&self) -> usize {
        self.active_mailboxes.read().await.len()
    }
}
