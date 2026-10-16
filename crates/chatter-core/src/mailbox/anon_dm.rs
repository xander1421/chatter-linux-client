//! Anonymous DM mailbox polling.
//!
//! Polling for anonymous DM conversations (stranger messages).
//! Uses the same ephemeral mailbox pattern as groups but for 1:1 conversations.

use std::sync::Arc;

use ed25519_dalek::{Signer, SigningKey};

use crate::domain::anon_dm::AnonDmId;
use crate::error::MailboxError;
use crate::mailbox::dm::ReceivedMessage;
use crate::network::grpc::AnonymousMailboxClient;
use crate::storage::repositories::AnonDmRepository;

/// Maximum messages to fetch per poll.
const DEFAULT_POLL_LIMIT: i32 = 50;

/// Result of polling an anonymous DM mailbox.
#[derive(Debug)]
pub struct AnonDmPollResult {
    /// Conversation this poll was for.
    pub conversation_id: AnonDmId,
    /// Messages received (encrypted envelopes).
    pub messages: Vec<ReceivedMessage>,
    /// Whether there may be more messages.
    pub has_more: bool,
    /// Mailbox ID for acknowledgment.
    pub mailbox_id: String,
}

impl AnonDmPollResult {
    /// Check if any messages were received.
    pub fn has_messages(&self) -> bool {
        !self.messages.is_empty()
    }

    /// Get the number of messages received.
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }
}

/// Anonymous DM mailbox poller.
///
/// Polls anonymous DM mailboxes for incoming messages.
pub struct AnonDmPoller {
    anon_dm_repo: Arc<AnonDmRepository>,
}

impl AnonDmPoller {
    /// Create a new anonymous DM poller.
    pub fn new(anon_dm_repo: Arc<AnonDmRepository>) -> Self {
        Self { anon_dm_repo }
    }

    /// Get all active mailbox keys for polling.
    ///
    /// Returns (conversation_id, public_key, private_key) tuples.
    #[allow(clippy::type_complexity)]
    pub fn get_active_mailbox_keys(
        &self,
    ) -> Result<Vec<(AnonDmId, [u8; 32], [u8; 64])>, MailboxError> {
        self.anon_dm_repo
            .get_active_mailbox_keys()
            .map_err(|e| MailboxError::Storage(e.to_string()))
    }

    /// Poll a single anonymous DM mailbox.
    pub async fn poll_mailbox(
        &self,
        conversation_id: &AnonDmId,
        client: &mut AnonymousMailboxClient,
        limit: Option<i32>,
    ) -> Result<AnonDmPollResult, MailboxError> {
        tracing::debug!(
            conversation_id = %conversation_id,
            "[ANON_DM_POLLER] Polling mailbox for conversation"
        );

        // Get the mailbox for this conversation
        let mailbox = self
            .anon_dm_repo
            .get_mailbox(conversation_id)
            .map_err(|e| MailboxError::Storage(e.to_string()))?
            .ok_or(MailboxError::NoMailboxConfigured)?;

        let limit = limit.unwrap_or(DEFAULT_POLL_LIMIT);

        // Get challenge from server
        let challenge_response = client
            .get_mailbox_challenge(mailbox.public_key.to_vec())
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        let challenge = challenge_response.challenge;
        // Use hex of public key as mailbox ID (for acknowledgment tracking)
        let mailbox_id = hex::encode(mailbox.public_key);

        // Sign challenge with private key
        let signing_key = SigningKey::from_keypair_bytes(&mailbox.private_key).map_err(|e| {
            MailboxError::KeyDerivationFailed(format!("Invalid signing key: {}", e))
        })?;
        let signature = signing_key.sign(&challenge);

        // Poll mailbox with signed challenge
        let poll_response = client
            .poll_ephemeral_mailbox(
                mailbox.public_key.to_vec(),
                signature.to_bytes().to_vec(),
                challenge,
                None, // after_message_id - start from beginning
                limit,
            )
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        // Convert server messages to ReceivedMessage
        let messages: Vec<ReceivedMessage> = poll_response
            .messages
            .into_iter()
            .map(|msg| ReceivedMessage {
                message_id: msg.id,
                encrypted_envelope: msg.encrypted_envelope,
                envelope_type: msg.envelope_type,
                server_timestamp: msg.timestamp,
            })
            .collect();

        let has_more = poll_response.has_more;

        if messages.is_empty() {
            tracing::trace!(
                conversation_id = %conversation_id,
                mailbox_pk = hex::encode(&mailbox.public_key[..8]),
                "Polled anonymous DM mailbox (empty)"
            );
        } else {
            tracing::info!(
                conversation_id = %conversation_id,
                message_count = messages.len(),
                has_more = has_more,
                mailbox_pk = hex::encode(&mailbox.public_key[..8]),
                "Polled anonymous DM mailbox"
            );
        }

        Ok(AnonDmPollResult {
            conversation_id: *conversation_id,
            messages,
            has_more,
            mailbox_id,
        })
    }

    /// Poll all anonymous DM mailboxes.
    pub async fn poll_all(
        &self,
        client: &mut AnonymousMailboxClient,
    ) -> Result<Vec<AnonDmPollResult>, MailboxError> {
        let mailbox_keys = self.get_active_mailbox_keys()?;

        tracing::trace!(
            mailbox_count = mailbox_keys.len(),
            "[ANON_DM_POLLER] Polling all anonymous DM mailboxes"
        );

        let mut results = Vec::with_capacity(mailbox_keys.len());
        let mut first_error = None;

        for (conversation_id, _, _) in &mailbox_keys {
            match self.poll_mailbox(conversation_id, client, None).await {
                Ok(result) => {
                    if result.has_messages() {
                        tracing::debug!(
                            conversation_id = %conversation_id,
                            messages = result.message_count(),
                            "Received anonymous DM messages"
                        );
                    }
                    results.push(result);
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "Failed to poll anonymous DM mailbox"
                    );
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                }
            }
        }

        // A failed single-mailbox poll must reach the provider so it can
        // discard a stale Tor connection and retry on the next cycle.
        if results.is_empty() {
            if let Some(error) = first_error {
                return Err(error);
            }
        }

        Ok(results)
    }

    /// Acknowledge messages in an anonymous DM mailbox.
    pub async fn acknowledge_messages(
        &self,
        conversation_id: &AnonDmId,
        message_ids: Vec<String>,
        client: &mut AnonymousMailboxClient,
    ) -> Result<u32, MailboxError> {
        if message_ids.is_empty() {
            return Ok(0);
        }

        // Get the mailbox for this conversation
        let mailbox = self
            .anon_dm_repo
            .get_mailbox(conversation_id)
            .map_err(|e| MailboxError::Storage(e.to_string()))?
            .ok_or(MailboxError::NoMailboxConfigured)?;

        // Get challenge from server
        let challenge_response = client
            .get_mailbox_challenge(mailbox.public_key.to_vec())
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        let challenge = challenge_response.challenge;

        // Sign challenge with private key
        let signing_key = SigningKey::from_keypair_bytes(&mailbox.private_key).map_err(|e| {
            MailboxError::KeyDerivationFailed(format!("Invalid signing key: {}", e))
        })?;
        let signature = signing_key.sign(&challenge);

        // Acknowledge messages
        let ack_response = client
            .acknowledge_messages(
                mailbox.public_key.to_vec(),
                signature.to_bytes().to_vec(), // challenge_signature
                challenge,                     // challenge
                message_ids.clone(),
            )
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        let acknowledged = ack_response.acknowledged_count as u32;
        tracing::debug!(
            conversation_id = %conversation_id,
            acknowledged = acknowledged,
            "Acknowledged anonymous DM messages"
        );

        Ok(acknowledged)
    }
}
