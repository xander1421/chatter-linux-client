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
use crate::mailbox::dm::{ConversationKey, DmConversationManager, DmMailbox};
use crate::mailbox::{MailboxToken, TokenManager};
use crate::network::grpc::AnonymousMailboxClient;
use crate::proto::content_v1::{Content, KeyExchange, content};
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

/// The mailbox half a bootstrap `KeyExchange.ephemeral_pk` announces, both in
/// the MESSAGE and the RESPONSE:
///
/// ```text
/// mailbox_pk (32, Ed25519: address + signing key) ||
/// conversation_pk (32, X25519 conversation ephemeral) ||
/// mailbox_encryption_pk (32, X25519: what bootstrap envelopes are sealed to)
/// ```
///
/// The signing and encryption keys are separate key pairs: no key does both
/// signing and DH (see `crypto::envelope_crypto`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxAnnouncement {
    pub mailbox_pk: [u8; 32],
    pub conversation_pk: [u8; 32],
    pub mailbox_encryption_pk: [u8; 32],
}

impl MailboxAnnouncement {
    /// Encoded length; any other `ephemeral_pk` length is not an announcement.
    pub const LEN: usize = 96;

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::LEN);
        out.extend_from_slice(&self.mailbox_pk);
        out.extend_from_slice(&self.conversation_pk);
        out.extend_from_slice(&self.mailbox_encryption_pk);
        out
    }

    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != Self::LEN {
            return None;
        }
        let part = |i: usize| <[u8; 32]>::try_from(&bytes[i * 32..(i + 1) * 32]).expect("32 bytes");
        Some(Self {
            mailbox_pk: part(0),
            conversation_pk: part(1),
            mailbox_encryption_pk: part(2),
        })
    }
}

/// Encode `announcement` as a serialized `Content { KeyExchange }`.
fn encode_announcement(
    announcement: &MailboxAnnouncement,
    conversation_id: String,
) -> Result<Vec<u8>, MailboxError> {
    let content = Content {
        content: Some(content::Content::KeyExchange(KeyExchange {
            ephemeral_pk: announcement.encode(),
            conversation_id,
            timestamp: TimestampNanos::now().as_u64(),
        })),
        ephemeral_reply_to: None,
        timestamp: TimestampNanos::now().as_u64(),
        padding: vec![],
    };
    let mut bytes = Vec::new();
    content
        .encode(&mut bytes)
        .map_err(|e| MailboxError::Storage(format!("Failed to encode announcement: {}", e)))?;
    Ok(bytes)
}

/// Response to send back to bootstrap initiator.
///
/// Created when handling an incoming bootstrap message from a peer.
/// Contains our mailbox address and ephemeral key to send back.
pub struct BootstrapResponse {
    /// Our mailbox public key (for them to send to us).
    pub our_mailbox_public_key: [u8; 32],
    /// Our mailbox's X25519 encryption key.
    pub our_mailbox_encryption_key: [u8; 32],
    /// Our ephemeral conversation key.
    pub our_ephemeral_public: [u8; 32],
    /// Their mailbox public key (where to send response).
    pub peer_mailbox_public_key: [u8; 32],
    /// Their mailbox's X25519 encryption key (what the response envelope is
    /// sealed to).
    pub peer_mailbox_encryption_key: [u8; 32],
    /// Peer user ID.
    pub peer_id: UserId,
}

impl BootstrapResponse {
    /// Build the response message to send to peer's mailbox: the same
    /// [`MailboxAnnouncement`] as a bootstrap MESSAGE.
    pub fn build_response_message(&self) -> Result<Vec<u8>, MailboxError> {
        encode_announcement(
            &MailboxAnnouncement {
                mailbox_pk: self.our_mailbox_public_key,
                conversation_pk: self.our_ephemeral_public,
                mailbox_encryption_pk: self.our_mailbox_encryption_key,
            },
            self.peer_id.to_string(),
        )
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
    /// When our mailbox is still live, re-announces it and its key instead.
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
    /// - `ConversationExists` - A bootstrap with this peer is already in progress
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
                peer_devices = conv.peer_mailboxes.len(),
                has_shared_secret = conv.conversation_key.as_ref().map(|k| k.shared_secret.is_some()).unwrap_or(false),
                "DM bootstrap: existing conversation found"
            );
            // Our mailbox is live but the peer cannot reach us (no or an
            // expired peer mailbox — or they asked, with a REQUEST): announce
            // the SAME mailbox and key again. No token, no new mailbox, and a
            // peer that already holds this key takes it as an idempotent
            // re-delivery, so repeating it is safe while a RESPONSE is due.
            if let (false, Some(mailbox), Some(key)) = (
                conv.needs_bootstrap(),
                &conv.our_mailbox,
                &conv.conversation_key,
            ) {
                tracing::info!(peer_id = %peer_id, "DM bootstrap: re-announcing our live mailbox");
                return Ok(BootstrapResult {
                    our_mailbox: mailbox.clone(),
                    our_ephemeral_public: key.our_ephemeral_public,
                    bootstrap_message: self.build_bootstrap_message(mailbox, key, &peer_id)?,
                    peer_id,
                });
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
                        vec![], // Deterministic blinding: no randomizer exists
                    )
                    .await
                {
                    Ok(resp) => break resp,
                    Err(e) => {
                        if e.is_retryable() && attempt < MAX_RETRIES {
                            let backoff =
                                std::time::Duration::from_millis(200 * u64::from(attempt));
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

        // The envelope key is generated here, beside the token's signing key:
        // the server never sees it, peers learn it from our announcement.
        let (encryption_private_key, _) =
            crate::crypto::envelope_crypto::generate_mailbox_encryption_key();
        Ok(DmMailbox {
            id: response.mailbox_id.clone(),
            public_key,
            private_key,
            encryption_private_key,
            signature: token.signature.clone(),
            randomizer: token.randomizer.clone(),
            token_id: token.id.clone(),
            created_at: Utc::now(),
            expires_at: Utc::now() + Duration::seconds(DEFAULT_MAILBOX_TTL_SECONDS as i64),
            last_polled_at: None,
        })
    }

    /// Check if an ephemeral_pk is a bootstrap [`MailboxAnnouncement`] vs
    /// a REQUEST (empty) or regular key exchange (32 bytes).
    pub fn is_bootstrap_message(ephemeral_pk: &[u8]) -> bool {
        ephemeral_pk.len() == MailboxAnnouncement::LEN
    }

    /// Handle receiving a bootstrap message from a peer.
    ///
    /// Called when KeyExchangeReceived event is processed and we detect
    /// it's a bootstrap (`ephemeral_pk` is a [`MailboxAnnouncement`]).
    ///
    /// # Arguments
    ///
    /// * `sender_id` - Who sent the bootstrap
    /// * `ephemeral_pk` - The encoded [`MailboxAnnouncement`]
    /// * `anon_mailbox_client` - For creating our response mailbox
    ///
    /// # Returns
    ///
    /// `Some(BootstrapResponse)` with our mailbox address to send back,
    /// or `None` if ephemeral_pk is not a [`MailboxAnnouncement`].
    ///
    /// # Errors
    ///
    /// - `NoTokensAvailable` - The pre-fetched token batch is empty
    /// - `Network` - Failed to communicate with server
    /// - `Storage` - Failed to persist state
    pub async fn handle_incoming_bootstrap(
        &self,
        sender_id: UserId,
        sender_device_id: u32,
        ephemeral_pk: &[u8],
        anon_mailbox_client: &mut AnonymousMailboxClient,
    ) -> Result<Option<BootstrapResponse>, MailboxError> {
        let Some(announcement) = MailboxAnnouncement::parse(ephemeral_pk) else {
            // Regular 32-byte key exchange, not bootstrap
            return Ok(None);
        };

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
            .handle_incoming_bootstrap_inner(
                sender_id,
                sender_device_id,
                &announcement,
                anon_mailbox_client,
            )
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
        sender_device_id: u32,
        announcement: &MailboxAnnouncement,
        anon_mailbox_client: &mut AnonymousMailboxClient,
    ) -> Result<Option<BootstrapResponse>, MailboxError> {
        let peer_mailbox_pk = announcement.mailbox_pk;
        let peer_conversation_pk = announcement.conversation_pk;

        // Check if we already have a conversation
        let existing = self.conversation_manager.load_conversation(&sender_id)?;

        // SECURITY: `sender_id` is authenticated, and this bootstrap is fresh,
        // before it gets here. Every `KeyExchangeReceived` is emitted only from
        // Double-Ratchet plaintext (`services/gateway/decryption.rs`): a sealed
        // envelope's sender certificate is validated against the trust root
        // and its inner message decrypted under that sender's address, whose
        // session or PreKey identity must equal the account's pinned identity
        // key (`crypto/signal/store.rs::is_trusted_identity`); libsignal
        // refuses a replayed ciphertext as a duplicate. So a new ephemeral here
        // is the pinned peer re-bootstrapping — typically because its own
        // mailbox expired, which it knows and we cannot (`expires_at` is not on
        // the wire). Like a new PreKey session from a trusted identity, it
        // replaces the peer mailbox and conversation secret; message history
        // is stored elsewhere and untouched. Anyone else's bootstrap lands on
        // their own `sender_id` conversation, never on this one. The same
        // holds per device: `sender_device_id` is the sealed sender's
        // certified device, so this bootstrap replaces that device's half of
        // the conversation and no other device's.

        // If we don't have a live mailbox of our own, create one. One mailbox
        // of ours serves every peer device.
        let our_mailbox = if existing.as_ref().map_or(true, |c| c.needs_bootstrap()) {
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
                            vec![], // Deterministic blinding: no randomizer exists
                        )
                        .await
                    {
                        Ok(resp) => break resp,
                        Err(e) => {
                            if e.is_retryable() && attempt < MAX_RETRIES {
                                let backoff =
                                    std::time::Duration::from_millis(200 * u64::from(attempt));
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
            existing.as_ref().and_then(|c| c.our_mailbox.clone())
        };

        // Our half: the one every peer device pairs with, or a fresh one.
        let conversation_key = match existing.and_then(|c| c.conversation_key) {
            Some(key) => key,
            None => {
                let key = ConversationKey::new();
                self.conversation_manager
                    .save_conversation_key(&sender_id, &key)?;
                key
            }
        };

        // That device's half: its mailbox (it made it and polls it) and its
        // ephemeral, checked against ours before anything is saved.
        self.conversation_manager.record_peer_device(
            &sender_id,
            sender_device_id,
            peer_mailbox_pk,
            peer_conversation_pk,
        )?;

        // Build response if we have a mailbox
        let Some(mailbox) = our_mailbox else {
            return Ok(None);
        };
        Ok(Some(BootstrapResponse {
            our_mailbox_public_key: mailbox.public_key,
            our_mailbox_encryption_key: mailbox.encryption_public_key()?,
            our_ephemeral_public: conversation_key.our_ephemeral_public,
            peer_mailbox_public_key: peer_mailbox_pk,
            peer_mailbox_encryption_key: announcement.mailbox_encryption_pk,
            peer_id: sender_id,
        }))
    }

    /// Build bootstrap message as serialized Content proto: our
    /// [`MailboxAnnouncement`].
    fn build_bootstrap_message(
        &self,
        mailbox: &DmMailbox,
        key: &ConversationKey,
        peer_id: &UserId,
    ) -> Result<Vec<u8>, MailboxError> {
        encode_announcement(
            &MailboxAnnouncement {
                mailbox_pk: mailbox.public_key,
                conversation_pk: key.our_ephemeral_public,
                mailbox_encryption_pk: mailbox.encryption_public_key()?,
            },
            peer_id.to_string(),
        )
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
                encryption_private_key: [2u8; 32],
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

    fn announcement() -> MailboxAnnouncement {
        MailboxAnnouncement {
            mailbox_pk: [1u8; 32],
            conversation_pk: [2u8; 32],
            mailbox_encryption_pk: [3u8; 32],
        }
    }

    #[test]
    fn announcement_roundtrips_through_key_exchange() {
        let peer_id = UserId::new();
        let bytes = encode_announcement(&announcement(), peer_id.to_string()).unwrap();
        let Some(content::Content::KeyExchange(ke)) =
            Content::decode(bytes.as_slice()).unwrap().content
        else {
            panic!("Expected KeyExchange content");
        };
        assert_eq!(ke.ephemeral_pk.len(), MailboxAnnouncement::LEN);
        assert!(DmBootstrap::is_bootstrap_message(&ke.ephemeral_pk));
        assert_eq!(
            MailboxAnnouncement::parse(&ke.ephemeral_pk),
            Some(announcement())
        );
        assert_eq!(ke.conversation_id, peer_id.to_string());
    }

    /// Every other length is a REQUEST, a regular key exchange, or garbage —
    /// including the old 64-byte announcement that carried no encryption key.
    #[test]
    fn only_full_announcements_parse() {
        for len in [0usize, 32, 64, 95, 97, 128] {
            assert!(
                MailboxAnnouncement::parse(&vec![0u8; len]).is_none(),
                "len {len}"
            );
            assert!(
                !DmBootstrap::is_bootstrap_message(&vec![0u8; len]),
                "len {len}"
            );
        }
    }

    #[test]
    fn test_bootstrap_response_build_message() {
        let peer_id = UserId::new();
        let response = BootstrapResponse {
            our_mailbox_public_key: [1u8; 32],
            our_mailbox_encryption_key: [3u8; 32],
            our_ephemeral_public: [2u8; 32],
            peer_mailbox_public_key: [4u8; 32],
            peer_mailbox_encryption_key: [5u8; 32],
            peer_id,
        };

        let message_bytes = response.build_response_message().unwrap();
        let Some(content::Content::KeyExchange(ke)) =
            Content::decode(message_bytes.as_slice()).unwrap().content
        else {
            panic!("Expected KeyExchange content");
        };
        assert_eq!(
            MailboxAnnouncement::parse(&ke.ephemeral_pk),
            Some(announcement())
        );
        assert_eq!(ke.conversation_id, peer_id.to_string());
    }
}
