//! DM mailbox bootstrap.
//!
//! Initial sealed sender to mailbox exchange for first contact.
//!
//! Bootstrap flow:
//! 1. Alice wants to DM Bob (first time)
//! 2. Alice acquires blind token via TokenManager
//! 3. Alice creates ephemeral mailbox with token
//! 4. Alice sends sealed sender message to Bob with her mailbox address
//! 5. Bob receives via device inbox, extracts Alice's mailbox address
//! 6. Bob creates his own mailbox, sends address to Alice's mailbox
//! 7. Both now have each other's mailbox addresses - two-way anonymity established

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{Duration, Utc};
use prost::Message;
use tokio::time::sleep;

use crate::domain::UserId;
use crate::error::MailboxError;
use crate::mailbox::dm::{ConversationKey, DmConversationManager, DmMailbox, PeerMailbox};
use crate::mailbox::{MailboxToken, TokenManager};
use crate::network::grpc::AnonymousMailboxClient;
use crate::proto::content_v1::{content, Content, KeyExchange};
use crate::storage::Database;
use crate::util::time::TimestampNanos;

/// Default mailbox TTL: 48 hours in seconds. Paired with a 12h rotation
/// threshold (key_rolling.rs) → ~36h effective epoch, 12h overlap.
pub const DEFAULT_MAILBOX_TTL_SECONDS: u64 = 48 * 60 * 60;

/// Global lock to prevent concurrent bootstrap operations for the same peer.
/// This avoids race conditions where multiple threads could create duplicate mailboxes.
static BOOTSTRAP_IN_PROGRESS: OnceLock<Mutex<HashSet<UserId>>> = OnceLock::new();

fn get_bootstrap_lock() -> &'static Mutex<HashSet<UserId>> {
    BOOTSTRAP_IN_PROGRESS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Try to acquire bootstrap lock for a peer. Returns true if acquired, false if already in progress.
/// Caller must call `release_bootstrap_lock` when done.
pub fn try_acquire_bootstrap_lock(peer_id: &UserId) -> bool {
    let mut lock = get_bootstrap_lock().lock().unwrap();
    if lock.contains(peer_id) {
        false
    } else {
        lock.insert(*peer_id);
        true
    }
}

/// Release bootstrap lock for a peer.
pub fn release_bootstrap_lock(peer_id: &UserId) {
    let mut lock = get_bootstrap_lock().lock().unwrap();
    lock.remove(peer_id);
}

/// DM bootstrap handler for initiating anonymous DM conversations.
///
/// Bootstrap flow:
/// 1. Check if conversation exists (might just need key exchange, not new mailbox)
/// 2. Acquire token from TokenManager if needed
/// 3. Create ephemeral mailbox on server
/// 4. Generate conversation keys
/// 5. Build bootstrap message with mailbox address + ephemeral public key
/// 6. Return BootstrapResult for caller to send via sealed sender
pub struct DmBootstrap {
    /// Token manager for blind signature tokens.
    token_manager: TokenManager,
    /// Conversation manager for state persistence.
    conversation_manager: DmConversationManager,
}

/// Result of bootstrap preparation (not yet sent).
///
/// Contains all information needed to complete the bootstrap via sealed sender.
pub struct BootstrapResult {
    /// Our newly created mailbox info.
    pub our_mailbox: DmMailbox,
    /// Our ephemeral public key for the conversation.
    pub our_ephemeral_public: [u8; 32],
    /// Serialized bootstrap message to send via sealed sender.
    pub bootstrap_message: Vec<u8>,
    /// Peer user ID this is for.
    pub peer_id: UserId,
}

/// Response to send back to bootstrap initiator.
///
/// Created when handling an incoming bootstrap message from a peer.
/// Contains our mailbox address and ephemeral key to send back.
pub struct BootstrapResponse {
    /// Our mailbox public key (for them to send to us).
    pub our_mailbox_public_key: [u8; 32],
    /// Our ephemeral conversation key.
    pub our_ephemeral_public: [u8; 32],
    /// Their mailbox public key (where to send response).
    pub peer_mailbox_public_key: [u8; 32],
    /// Peer user ID.
    pub peer_id: UserId,
}

impl BootstrapResponse {
    /// Build the response message to send to peer's mailbox.
    ///
    /// Response uses the same 64-byte combined key format as bootstrap:
    /// - First 32 bytes: our mailbox public key
    /// - Next 32 bytes: our conversation ephemeral public key
    pub fn build_response_message(&self) -> Result<Vec<u8>, MailboxError> {
        // Response is also 64-byte combined key
        let mut combined_pk = Vec::with_capacity(64);
        combined_pk.extend_from_slice(&self.our_mailbox_public_key);
        combined_pk.extend_from_slice(&self.our_ephemeral_public);

        let key_exchange = KeyExchange {
            ephemeral_pk: combined_pk,
            conversation_id: self.peer_id.to_string(),
            timestamp: TimestampNanos::now().as_u64(),
        };

        let content = Content {
            content: Some(content::Content::KeyExchange(key_exchange)),
            ephemeral_reply_to: None,
            timestamp: TimestampNanos::now().as_u64(),
            padding: vec![],
        };

        let mut bytes = Vec::new();
        content
            .encode(&mut bytes)
            .map_err(|e| MailboxError::Storage(format!("Failed to encode response: {}", e)))?;
        Ok(bytes)
    }
}

impl DmBootstrap {
    /// Create a new DmBootstrap handler.
    ///
    /// # Arguments
    ///
    /// * `db` - Shared database connection
    pub fn new(db: Arc<Database>) -> Self {
        let token_manager = TokenManager::new(db.clone());
        let conversation_manager = DmConversationManager::new(db);
        Self {
            token_manager,
            conversation_manager,
        }
    }

    /// Initiate a DM with a peer.
    ///
    /// Creates our mailbox, generates conversation keys, prepares bootstrap message.
    /// Caller is responsible for sending the message via sealed sender.
    ///
    /// # Arguments
    ///
    /// * `peer_id` - The user we want to DM
    /// * `anon_mailbox_client` - For creating the ephemeral mailbox
    ///
    /// # Returns
    ///
    /// BootstrapResult with prepared bootstrap message, or error.
    ///
    /// # Errors
    ///
    /// - `ConversationExists` - Conversation already exists and doesn't need bootstrap
    /// - `NoTokensAvailable` - The pre-fetched token batch is empty
    /// - `Network` - Failed to communicate with server
    /// - `Storage` - Failed to persist state
    pub async fn initiate_dm(
        &self,
        peer_id: UserId,
        anon_mailbox_client: &mut AnonymousMailboxClient,
    ) -> Result<BootstrapResult, MailboxError> {
        // Acquire bootstrap lock for this peer to prevent concurrent operations
        {
            let mut lock = get_bootstrap_lock().lock().unwrap();
            if lock.contains(&peer_id) {
                tracing::debug!(
                    peer_id = %peer_id,
                    "DM bootstrap: already in progress for this peer, skipping"
                );
                return Err(MailboxError::ConversationExists);
            }
            lock.insert(peer_id);
        }

        // Use a guard to ensure cleanup on all exit paths
        let result = self.initiate_dm_inner(peer_id, anon_mailbox_client).await;

        // Release lock
        {
            let mut lock = get_bootstrap_lock().lock().unwrap();
            lock.remove(&peer_id);
        }

        result
    }

    /// Inner implementation of initiate_dm (extracted for lock management).
    async fn initiate_dm_inner(
        &self,
        peer_id: UserId,
        anon_mailbox_client: &mut AnonymousMailboxClient,
    ) -> Result<BootstrapResult, MailboxError> {
        tracing::info!(peer_id = %peer_id, "DM bootstrap: step 1 - checking existing conversation");

        // 1. Check existing conversation state
        let existing = self.conversation_manager.load_conversation(&peer_id)?;
        if let Some(conv) = &existing {
            tracing::debug!(
                peer_id = %peer_id,
                needs_bootstrap = conv.needs_bootstrap(),
                has_our_mailbox = conv.our_mailbox.is_some(),
                has_peer_mailbox = conv.peer_mailbox.is_some(),
                has_shared_secret = conv.conversation_key.as_ref().map(|k| k.shared_secret.is_some()).unwrap_or(false),
                "DM bootstrap: existing conversation found"
            );
            if !conv.needs_bootstrap() {
                return Err(MailboxError::ConversationExists);
            }
        } else {
            tracing::debug!(peer_id = %peer_id, "DM bootstrap: no existing conversation");
        }

        tracing::info!(peer_id = %peer_id, "DM bootstrap: step 2 - ensuring token available");

        // 2. Spend a pre-fetched token (never request-then-spend)
        let token = self.token_manager.claim_prefetched_token()?;
        tracing::debug!(peer_id = %peer_id, token_id = %token.id, "DM bootstrap: got token");

        tracing::info!(peer_id = %peer_id, "DM bootstrap: step 3 - creating ephemeral mailbox");

        // 3. Create ephemeral mailbox on server (with retry for transient transport errors)
        //    Uses anonymous client to avoid leaking device identity alongside blind signature
        let mut retried = false;
        let response = {
            const MAX_RETRIES: u32 = 3;
            let mut attempt = 0;
            loop {
                attempt += 1;
                match anon_mailbox_client
                    .create_ephemeral_mailbox(
                        token.public_key.clone(),
                        token.signature.clone(),
                        DEFAULT_MAILBOX_TTL_SECONDS,
                        vec![],  // Empty randomizer for compatibility
                    )
                    .await
                {
                    Ok(resp) => break resp,
                    Err(e) => {
                        if e.is_retryable() && attempt < MAX_RETRIES {
                            let backoff = std::time::Duration::from_millis(200 * u64::from(attempt));
                            tracing::warn!(
                                peer_id = %peer_id,
                                error = %e,
                                attempt,
                                backoff_ms = backoff.as_millis() as u64,
                                "DM bootstrap: mailbox creation failed (retrying)"
                            );
                            sleep(backoff).await;
                            retried = true;
                        } else {
                            tracing::error!(
                                peer_id = %peer_id,
                                error = %e,
                                attempt,
                                "DM bootstrap: failed to create mailbox"
                            );
                            return Err(MailboxError::Network(e.to_string()));
                        }
                    }
                }
            }
        };
        if retried {
            tracing::info!(peer_id = %peer_id, "DM bootstrap: mailbox created after retry");
        }
        tracing::debug!(peer_id = %peer_id, mailbox_id = %response.mailbox_id, "DM bootstrap: mailbox created");

        // 4. Build our mailbox from token + response
        let our_mailbox = self.build_mailbox_from_token(&token, &response)?;
        tracing::debug!(peer_id = %peer_id, "DM bootstrap: step 4 - mailbox built from token");

        // 5. Generate conversation keys
        let conversation_key = ConversationKey::new();
        tracing::debug!(peer_id = %peer_id, "DM bootstrap: step 5 - conversation keys generated");

        // 6. Save state (token already marked used by claim_prefetched_token)
        self.conversation_manager
            .save_our_mailbox(&peer_id, &our_mailbox)?;
        self.conversation_manager
            .save_conversation_key(&peer_id, &conversation_key)?;
        let _ = token;
        tracing::debug!(peer_id = %peer_id, "DM bootstrap: step 6 - state saved");

        // 7. Build bootstrap message (KeyExchange proto)
        let bootstrap_message =
            self.build_bootstrap_message(&our_mailbox, &conversation_key, &peer_id)?;
        tracing::debug!(peer_id = %peer_id, message_len = bootstrap_message.len(), "DM bootstrap: step 7 - bootstrap message built");

        tracing::info!(peer_id = %peer_id, "DM bootstrap: initiate_dm completed successfully");

        Ok(BootstrapResult {
            our_mailbox,
            our_ephemeral_public: conversation_key.our_ephemeral_public,
            bootstrap_message,
            peer_id,
        })
    }

    /// Build DmMailbox from token and server response.
    fn build_mailbox_from_token(
        &self,
        token: &MailboxToken,
        response: &crate::proto::mailbox_v1::CreateEphemeralMailboxResponse,
    ) -> Result<DmMailbox, MailboxError> {
        let mut public_key = [0u8; 32];
        let mut private_key = [0u8; 64];

        if token.public_key.len() != 32 {
            return Err(MailboxError::InvalidServerKey(format!(
                "Invalid token public key size: expected 32, got {}",
                token.public_key.len()
            )));
        }
        if token.private_key.len() != 64 {
            return Err(MailboxError::InvalidServerKey(format!(
                "Invalid token private key size: expected 64, got {}",
                token.private_key.len()
            )));
        }

        public_key.copy_from_slice(&token.public_key);
        private_key.copy_from_slice(&token.private_key);

        Ok(DmMailbox {
            id: response.mailbox_id.clone(),
            public_key,
            private_key,
            signature: token.signature.clone(),
            randomizer: token.randomizer.clone(),
            token_id: token.id.clone(),
            created_at: Utc::now(),
            expires_at: Utc::now() + Duration::seconds(DEFAULT_MAILBOX_TTL_SECONDS as i64),
            last_polled_at: None,
        })
    }

    /// Check if an ephemeral_pk is a bootstrap message (64 bytes) vs regular key exchange (32 bytes).
    ///
    /// Bootstrap messages use a combined 64-byte key containing both the mailbox
    /// public key and conversation ephemeral key.
    pub fn is_bootstrap_message(ephemeral_pk: &[u8]) -> bool {
        ephemeral_pk.len() == 64
    }

    /// Handle receiving a bootstrap message from a peer.
    ///
    /// Called when KeyExchangeReceived event is processed and we detect
    /// it's a bootstrap (64-byte ephemeral_pk = mailbox + conversation key).
    ///
    /// # Arguments
    ///
    /// * `sender_id` - Who sent the bootstrap
    /// * `ephemeral_pk` - The 64-byte combined key (mailbox || conversation)
    /// * `anon_mailbox_client` - For creating our response mailbox
    ///
    /// # Returns
    ///
    /// `Some(BootstrapResponse)` with our mailbox address to send back,
    /// or `None` if ephemeral_pk is not a bootstrap message (not 64 bytes).
    ///
    /// # Errors
    ///
    /// - `NoTokensAvailable` - The pre-fetched token batch is empty
    /// - `Network` - Failed to communicate with server
    /// - `Storage` - Failed to persist state
    pub async fn handle_incoming_bootstrap(
        &self,
        sender_id: UserId,
        ephemeral_pk: &[u8],
        anon_mailbox_client: &mut AnonymousMailboxClient,
    ) -> Result<Option<BootstrapResponse>, MailboxError> {
        // Validate 64-byte format (bootstrap vs regular key exchange)
        if ephemeral_pk.len() != 64 {
            // Regular 32-byte key exchange, not bootstrap
            return Ok(None);
        }

        // Acquire bootstrap lock for this peer to prevent concurrent operations
        {
            let mut lock = get_bootstrap_lock().lock().unwrap();
            if lock.contains(&sender_id) {
                tracing::debug!(
                    peer_id = %sender_id,
                    "DM bootstrap: already in progress for this peer, skipping incoming"
                );
                return Ok(None);
            }
            lock.insert(sender_id);
        }

        // Use inner function for cleanup
        let result = self
            .handle_incoming_bootstrap_inner(sender_id, ephemeral_pk, anon_mailbox_client)
            .await;

        // Release lock
        {
            let mut lock = get_bootstrap_lock().lock().unwrap();
            lock.remove(&sender_id);
        }

        result
    }

    /// Inner implementation of handle_incoming_bootstrap (extracted for lock management).
    async fn handle_incoming_bootstrap_inner(
        &self,
        sender_id: UserId,
        ephemeral_pk: &[u8],
        anon_mailbox_client: &mut AnonymousMailboxClient,
    ) -> Result<Option<BootstrapResponse>, MailboxError> {
        // Extract peer's mailbox address and conversation key
        let mut peer_mailbox_pk = [0u8; 32];
        let mut peer_conversation_pk = [0u8; 32];
        peer_mailbox_pk.copy_from_slice(&ephemeral_pk[0..32]);
        peer_conversation_pk.copy_from_slice(&ephemeral_pk[32..64]);

        // Check if we already have a conversation
        let existing = self.conversation_manager.load_conversation(&sender_id)?;

        // SECURITY: gate re-bootstrap of an established conversation.
        //
        // Without this check, any envelope that decrypts via the established
        // sealed-sender path would silently overwrite the existing
        // `shared_secret` and `their_ephemeral_public`. That breaks two
        // properties at once:
        //   1. Replay protection (#6): a captured (or server-replayed)
        //      bootstrap can be re-applied repeatedly, pinning the session
        //      to whatever ephemeral the attacker recorded.
        //   2. Re-key gating (#3): libsignal's `process_prekey_bundle`
        //      gates session replacement on `is_trusted_identity`; chatter
        //      had no equivalent identity-key trust gate at this layer.
        //
        // Idempotent re-delivery of the SAME bootstrap (same peer ephemeral)
        // is benign — we no-op and return the existing mailbox response.
        // A DIFFERENT ephemeral against an established conversation is
        // rejected; legitimate rotation (peer device reset etc.) requires
        // an out-of-band conversation reset by the user.
        if let Some(ex_key) = existing.as_ref().and_then(|c| c.conversation_key.as_ref())
        {
            if ex_key.is_established() {
                if ex_key.their_ephemeral_public == Some(peer_conversation_pk) {
                    tracing::debug!(
                        peer_id = %sender_id,
                        "DM bootstrap: idempotent re-delivery of established session — no-op"
                    );
                    let our_mailbox = existing.as_ref().and_then(|c| c.our_mailbox.clone());
                    return Ok(our_mailbox.map(|mailbox| BootstrapResponse {
                        our_mailbox_public_key: mailbox.public_key,
                        our_ephemeral_public: ex_key.our_ephemeral_public,
                        peer_mailbox_public_key: peer_mailbox_pk,
                        peer_id: sender_id,
                    }));
                }
                tracing::warn!(
                    peer_id = %sender_id,
                    "SECURITY: dropping bootstrap with new ephemeral against established conversation"
                );
                return Err(MailboxError::ConversationExists);
            }
        }

        // Save peer's mailbox address
        let peer_mailbox = PeerMailbox {
            public_key: peer_mailbox_pk,
            received_at: Utc::now(),
            expires_at: None, // Unknown - peer's mailbox TTL isn't communicated
        };
        self.conversation_manager
            .save_peer_mailbox(&sender_id, &peer_mailbox)?;

        // If we don't have our own mailbox, create one
        let our_mailbox = if existing
            .as_ref()
            .map(|c| c.our_mailbox.is_none())
            .unwrap_or(true)
        {
            let token = self.token_manager.claim_prefetched_token()?;
            let response = {
                const MAX_RETRIES: u32 = 3;
                let mut attempt = 0;
                loop {
                    attempt += 1;
                    match anon_mailbox_client
                        .create_ephemeral_mailbox(
                            token.public_key.clone(),
                            token.signature.clone(),
                            DEFAULT_MAILBOX_TTL_SECONDS,
                            vec![],  // Empty randomizer for legacy tokens
                        )
                        .await
                    {
                        Ok(resp) => break resp,
                        Err(e) => {
                            if e.is_retryable() && attempt < MAX_RETRIES {
                                let backoff = std::time::Duration::from_millis(200 * u64::from(attempt));
                                tracing::warn!(
                                    peer_id = %sender_id,
                                    error = %e,
                                    attempt,
                                    "Incoming bootstrap: mailbox creation failed (retrying)"
                                );
                                sleep(backoff).await;
                            } else {
                                return Err(MailboxError::Network(e.to_string()));
                            }
                        }
                    }
                }
            };

            let mailbox = self.build_mailbox_from_token(&token, &response)?;
            self.conversation_manager
                .save_our_mailbox(&sender_id, &mailbox)?;
            Some(mailbox)
        } else {
            existing.and_then(|c| c.our_mailbox)
        };

        // Get or create conversation key and derive shared secret
        let existing_for_key = self.conversation_manager.load_conversation(&sender_id)?;
        let mut conversation_key = existing_for_key
            .and_then(|c| c.conversation_key)
            .unwrap_or_else(ConversationKey::new);

        // Derive shared secret from their ephemeral key
        conversation_key.derive_shared_secret(peer_conversation_pk)?;
        self.conversation_manager
            .save_conversation_key(&sender_id, &conversation_key)?;

        // Build response if we have a mailbox
        let response = our_mailbox.map(|mailbox| BootstrapResponse {
            our_mailbox_public_key: mailbox.public_key,
            our_ephemeral_public: conversation_key.our_ephemeral_public,
            peer_mailbox_public_key: peer_mailbox_pk,
            peer_id: sender_id,
        });

        Ok(response)
    }

    /// Build bootstrap message as serialized Content proto.
    ///
    /// Bootstrap message uses 64-byte combined key:
    /// - First 32 bytes: mailbox public key (where to send replies)
    /// - Next 32 bytes: conversation ephemeral public key (for key agreement)
    fn build_bootstrap_message(
        &self,
        mailbox: &DmMailbox,
        key: &ConversationKey,
        peer_id: &UserId,
    ) -> Result<Vec<u8>, MailboxError> {
        // For bootstrap, we encode mailbox address + ephemeral key together
        // Using 64-byte format: first 32 bytes = mailbox pk, next 32 = conversation ephemeral
        let mut combined_pk = Vec::with_capacity(64);
        combined_pk.extend_from_slice(&mailbox.public_key);
        combined_pk.extend_from_slice(&key.our_ephemeral_public);

        let bootstrap_exchange = KeyExchange {
            ephemeral_pk: combined_pk, // 64 bytes: mailbox || conversation key
            conversation_id: peer_id.to_string(),
            timestamp: TimestampNanos::now().as_u64(),
        };

        let content = Content {
            content: Some(content::Content::KeyExchange(bootstrap_exchange)),
            ephemeral_reply_to: None,
            timestamp: TimestampNanos::now().as_u64(),
            padding: vec![],
        };

        let mut bytes = Vec::new();
        content
            .encode(&mut bytes)
            .map_err(|e| MailboxError::Storage(format!("Failed to encode content: {}", e)))?;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_mailbox_ttl() {
        // 7 days in seconds
        assert_eq!(DEFAULT_MAILBOX_TTL_SECONDS, 48 * 60 * 60);
        assert_eq!(DEFAULT_MAILBOX_TTL_SECONDS, 172800);
    }

    #[test]
    fn test_bootstrap_result_structure() {
        let peer_id = UserId::new();
        let result = BootstrapResult {
            our_mailbox: DmMailbox {
                id: "test-mailbox".to_string(),
                public_key: [1u8; 32],
                private_key: [2u8; 64],
                signature: vec![3u8; 256],
                randomizer: Some(vec![4u8; 32]),
                token_id: "test-token".to_string(),
                created_at: Utc::now(),
                expires_at: Utc::now() + Duration::days(7),
                last_polled_at: None,
            },
            our_ephemeral_public: [5u8; 32],
            bootstrap_message: vec![6u8; 100],
            peer_id,
        };

        assert_eq!(result.our_mailbox.id, "test-mailbox");
        assert_eq!(result.our_ephemeral_public, [5u8; 32]);
        assert_eq!(result.bootstrap_message.len(), 100);
        assert_eq!(result.peer_id, peer_id);
    }

    #[test]
    fn test_build_bootstrap_message_format() {
        // Create a mock Content to verify the 64-byte format
        let mailbox_pk = [1u8; 32];
        let conversation_pk = [2u8; 32];
        let peer_id = UserId::new();

        // Build combined key manually (what build_bootstrap_message does)
        let mut combined_pk = Vec::with_capacity(64);
        combined_pk.extend_from_slice(&mailbox_pk);
        combined_pk.extend_from_slice(&conversation_pk);

        assert_eq!(combined_pk.len(), 64);
        assert_eq!(&combined_pk[0..32], &mailbox_pk);
        assert_eq!(&combined_pk[32..64], &conversation_pk);

        let key_exchange = KeyExchange {
            ephemeral_pk: combined_pk,
            conversation_id: peer_id.to_string(),
            timestamp: TimestampNanos::now().as_u64(),
        };

        let content = Content {
            content: Some(content::Content::KeyExchange(key_exchange)),
            ephemeral_reply_to: None,
            timestamp: TimestampNanos::now().as_u64(),
            padding: vec![],
        };

        let mut bytes = Vec::new();
        content.encode(&mut bytes).unwrap();

        // Verify we can decode it back
        let decoded = Content::decode(bytes.as_slice()).unwrap();
        assert!(decoded.content.is_some());

        if let Some(content::Content::KeyExchange(ke)) = decoded.content {
            assert_eq!(ke.ephemeral_pk.len(), 64);
            assert_eq!(&ke.ephemeral_pk[0..32], &mailbox_pk);
            assert_eq!(&ke.ephemeral_pk[32..64], &conversation_pk);
        } else {
            panic!("Expected KeyExchange content");
        }
    }

    #[test]
    fn test_is_bootstrap_message_detection() {
        // 64-byte key = bootstrap
        let bootstrap_key = vec![0u8; 64];
        assert!(DmBootstrap::is_bootstrap_message(&bootstrap_key));

        // 32-byte key = regular key exchange
        let regular_key = vec![0u8; 32];
        assert!(!DmBootstrap::is_bootstrap_message(&regular_key));

        // Other sizes = not bootstrap
        let short_key = vec![0u8; 16];
        assert!(!DmBootstrap::is_bootstrap_message(&short_key));

        let long_key = vec![0u8; 128];
        assert!(!DmBootstrap::is_bootstrap_message(&long_key));
    }

    #[test]
    fn test_bootstrap_response_structure() {
        let peer_id = UserId::new();
        let response = BootstrapResponse {
            our_mailbox_public_key: [1u8; 32],
            our_ephemeral_public: [2u8; 32],
            peer_mailbox_public_key: [3u8; 32],
            peer_id,
        };

        assert_eq!(response.our_mailbox_public_key, [1u8; 32]);
        assert_eq!(response.our_ephemeral_public, [2u8; 32]);
        assert_eq!(response.peer_mailbox_public_key, [3u8; 32]);
        assert_eq!(response.peer_id, peer_id);
    }

    #[test]
    fn test_bootstrap_response_build_message() {
        let peer_id = UserId::new();
        let response = BootstrapResponse {
            our_mailbox_public_key: [1u8; 32],
            our_ephemeral_public: [2u8; 32],
            peer_mailbox_public_key: [3u8; 32],
            peer_id,
        };

        let message_bytes = response.build_response_message().unwrap();
        assert!(!message_bytes.is_empty());

        // Decode and verify
        let decoded = Content::decode(message_bytes.as_slice()).unwrap();
        assert!(decoded.content.is_some());

        if let Some(content::Content::KeyExchange(ke)) = decoded.content {
            assert_eq!(ke.ephemeral_pk.len(), 64);
            assert_eq!(&ke.ephemeral_pk[0..32], &[1u8; 32]); // our mailbox
            assert_eq!(&ke.ephemeral_pk[32..64], &[2u8; 32]); // our ephemeral
            assert_eq!(ke.conversation_id, peer_id.to_string());
        } else {
            panic!("Expected KeyExchange content");
        }
    }

    #[test]
    fn test_extract_bootstrap_keys() {
        // Simulate extracting keys from a 64-byte bootstrap message
        let mailbox_pk = [1u8; 32];
        let conversation_pk = [2u8; 32];

        let mut combined = Vec::with_capacity(64);
        combined.extend_from_slice(&mailbox_pk);
        combined.extend_from_slice(&conversation_pk);

        // Extract keys (what handle_incoming_bootstrap does)
        let mut extracted_mailbox = [0u8; 32];
        let mut extracted_conversation = [0u8; 32];
        extracted_mailbox.copy_from_slice(&combined[0..32]);
        extracted_conversation.copy_from_slice(&combined[32..64]);

        assert_eq!(extracted_mailbox, mailbox_pk);
        assert_eq!(extracted_conversation, conversation_pk);
    }

    #[test]
    fn test_combined_key_roundtrip() {
        // Test full roundtrip: create bootstrap message, decode, extract keys
        let mailbox_pk = [7u8; 32];
        let conversation_pk = [8u8; 32];
        let peer_id = UserId::new();

        // Create combined key
        let mut combined = Vec::with_capacity(64);
        combined.extend_from_slice(&mailbox_pk);
        combined.extend_from_slice(&conversation_pk);

        // Encode as KeyExchange
        let key_exchange = KeyExchange {
            ephemeral_pk: combined.clone(),
            conversation_id: peer_id.to_string(),
            timestamp: 1234567890,
        };

        let content = Content {
            content: Some(content::Content::KeyExchange(key_exchange)),
            ephemeral_reply_to: None,
            timestamp: 1234567890,
            padding: vec![],
        };

        let mut bytes = Vec::new();
        content.encode(&mut bytes).unwrap();

        // Decode
        let decoded = Content::decode(bytes.as_slice()).unwrap();
        let ke = match decoded.content {
            Some(content::Content::KeyExchange(ke)) => ke,
            _ => panic!("Expected KeyExchange"),
        };

        // Extract keys from decoded message
        assert!(DmBootstrap::is_bootstrap_message(&ke.ephemeral_pk));

        let mut extracted_mailbox = [0u8; 32];
        let mut extracted_conversation = [0u8; 32];
        extracted_mailbox.copy_from_slice(&ke.ephemeral_pk[0..32]);
        extracted_conversation.copy_from_slice(&ke.ephemeral_pk[32..64]);

        assert_eq!(extracted_mailbox, mailbox_pk);
        assert_eq!(extracted_conversation, conversation_pk);
    }
}
