//! Ephemeral mailbox manager for NDSS '21 privacy-preserving messaging.
//!
//! Manages the lifecycle of ephemeral mailboxes including:
//! - Anonymous token acquisition via blind signatures
//! - Mailbox creation/deletion
//! - Sending messages via ephemeral mailboxes
//! - Polling mailboxes for incoming messages

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use ed25519_dalek::{SigningKey, VerifyingKey};

use crate::crypto::blind_signature::{BlindSignature, BlindingResult};
use crate::error::{CryptoError, NetworkError};
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
    /// Blinding result from the blind operation
    blinding_result: BlindingResult,
    /// Unblinded signature (proof of authorization)
    signature: Option<Vec<u8>>,
    /// Message randomizer from RFC 9474 (32-byte random prefix)
    /// Required for server-side verification with randomized variants
    msg_randomizer: Option<Vec<u8>>,
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

/// Manages ephemeral mailboxes for privacy-preserving messaging.
///
/// Implements the NDSS '21 mailbox system for sender anonymity.
/// Settings key for cached server blind signature public key.
const SERVER_BLIND_SIG_PUBKEY: &str = "mailbox.server_blind_sig_pubkey";

pub struct MailboxManager {
    /// gRPC client pool (for creating MailboxClient instances)
    client_pool: Arc<GrpcClientPool>,
    /// Settings repository (for persisting server public key)
    settings_repo: Arc<SettingsRepository>,
    /// Cached server blind signature public key
    server_pubkey: Arc<RwLock<Option<BlindSignature>>>,
    /// Pool of available unblinded tokens
    token_pool: Arc<RwLock<Vec<MailboxToken>>>,
    /// Active mailboxes per conversation
    active_mailboxes: Arc<RwLock<HashMap<String, ActiveMailbox>>>,
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
        }
    }

    /// Initialize the manager by loading or fetching the server's blind signature public key.
    ///
    /// First checks settings cache, then fetches from server if not cached.
    pub async fn initialize(&self) -> Result<(), MailboxError> {
        // Try loading from cache first (stored as base64)
        if let Ok(Some(cached_b64)) = self.settings_repo.get_string(SERVER_BLIND_SIG_PUBKEY) {
            if let Ok(cached_key) = BASE64.decode(&cached_b64) {
                if let Ok(blind_sig) = BlindSignature::new(&cached_key) {
                    *self.server_pubkey.write().await = Some(blind_sig);
                    tracing::debug!("Loaded blind signature public key from cache");
                    return Ok(());
                }
            }
        }

        // Fetch from server
        let mut client = self.create_mailbox_client();
        let response = client.get_blind_signature_public_key().await?;

        // Parse and cache in memory
        let blind_sig = BlindSignature::new(&response.public_key)?;
        *self.server_pubkey.write().await = Some(blind_sig);

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
        let blind_sig = self
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
            // Use manual generation to avoid RNG version conflicts
            let mut secret_bytes = [0u8; 32];
            rand::RngCore::fill_bytes(&mut rand::rng(), &mut secret_bytes);
            let private_key = SigningKey::from_bytes(&secret_bytes);
            let public_key = private_key.verifying_key();

            // Blind the public key
            let blinding_result = blind_sig.blind(public_key.as_bytes())?;
            blinded_keys.push(blinding_result.blind_msg.clone().into());

            tokens.push(MailboxToken {
                public_key,
                private_key,
                blinding_result,
                signature: None,
                msg_randomizer: None,  // Will be set after unblinding
            });
        }

        // Request blind signatures from server
        let mut client = self.create_mailbox_client();
        let response = client
            .get_mailbox_tokens(count as i32, blinded_keys)
            .await?;

        // Unblind the signatures and extract message randomizers
        for (token, blinded_sig) in tokens.iter_mut().zip(response.blinded_signatures.iter()) {
            let signature = blind_sig.unblind(
                blinded_sig,
                &token.blinding_result,
                token.public_key.as_bytes(),
            )?;
            token.signature = Some(signature);
            
            // Extract the message randomizer (32-byte prefix from RFC 9474)
            // This is needed for server-side verification
            if let Some(randomizer) = &token.blinding_result.msg_randomizer {
                token.msg_randomizer = Some(randomizer.as_ref().to_vec());
            }
        }

        // Add to token pool
        self.token_pool.write().await.extend(tokens);

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
                // Check if it's still valid
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;

                if mailbox.expires_at > now {
                    return Ok(mailbox.clone());
                }
            }
        }

        // Need to create a new mailbox - get a token from the pool
        let token = {
            let mut pool = self.token_pool.write().await;
            pool.pop().ok_or(MailboxError::NoTokensAvailable)?
        };

        // Create the mailbox anonymously using RFC 9474 blind signature proof
        let signature = token.signature.ok_or(MailboxError::NoTokensAvailable)?;
        let message_randomizer = token.msg_randomizer.unwrap_or_default();
        
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

        // Store the active mailbox
        let mailbox = ActiveMailbox {
            mailbox_id: response.mailbox_id,
            public_key: token.public_key,
            private_key: token.private_key,
            expires_at: response.expires_at,
            conversation_id: conversation_id.to_string(),
        };

        self.active_mailboxes
            .write()
            .await
            .insert(conversation_id.to_string(), mailbox.clone());

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
        MailboxClient::new(self.client_pool.channel(), self.client_pool.auth_clone())
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
