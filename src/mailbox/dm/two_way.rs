//! Two-way anonymity.
//!
//! TwoWayAnonymity service for mailbox message sending and polling.
//! Coordinates polling and sending through ephemeral mailboxes
//! after bootstrap has established the two-way connection.

use std::sync::Arc;

use ed25519_dalek::{Signer, SigningKey};

use crate::domain::UserId;
use crate::error::MailboxError;
use crate::mailbox::dm::{DmConversationManager, DmMailbox};
use crate::network::grpc::MailboxClient;
use crate::proto::mailbox_v1::EnvelopeType;
use crate::storage::Database;

/// Maximum messages to fetch per poll.
const DEFAULT_POLL_LIMIT: i32 = 50;

/// Two-way anonymity handler for DM mailbox operations.
///
/// Coordinates polling and sending through ephemeral mailboxes
/// after bootstrap has established the two-way connection.
pub struct TwoWayAnonymity {
    /// Conversation manager for state persistence.
    conversation_manager: DmConversationManager,
}

/// Result of polling a mailbox.
#[derive(Debug)]
pub struct PollResult {
    /// Conversation this poll was for.
    pub peer_id: UserId,
    /// Messages received (encrypted envelopes).
    pub messages: Vec<ReceivedMessage>,
    /// Whether there may be more messages.
    pub has_more: bool,
}

/// A message received from a mailbox.
#[derive(Debug)]
pub struct ReceivedMessage {
    /// Server-assigned message ID (for acknowledgment).
    pub message_id: String,
    /// Encrypted envelope content.
    pub encrypted_envelope: Vec<u8>,
    /// Envelope type (SealedSender, PreKey, Message).
    pub envelope_type: i32,
    /// Server timestamp.
    pub server_timestamp: u64,
}

/// Result of sending a message.
pub struct SendResult {
    /// Conversation peer.
    pub peer_id: UserId,
    /// Server-assigned message ID.
    pub message_id: String,
    /// Server timestamp.
    pub server_timestamp: u64,
}

impl TwoWayAnonymity {
    /// Create a new TwoWayAnonymity handler.
    ///
    /// # Arguments
    ///
    /// * `db` - Shared database connection
    pub fn new(db: Arc<Database>) -> Self {
        let conversation_manager = DmConversationManager::new(db);
        Self {
            conversation_manager,
        }
    }

    /// Poll a conversation's mailbox for new messages.
    ///
    /// Authenticates with challenge-response and fetches pending messages.
    ///
    /// # Arguments
    ///
    /// * `peer_id` - The conversation peer
    /// * `mailbox_client` - For mailbox operations
    /// * `after_message_id` - Optional cursor for pagination
    pub async fn poll_mailbox(
        &self,
        peer_id: &UserId,
        mailbox_client: &mut MailboxClient,
        after_message_id: Option<&str>,
    ) -> Result<PollResult, MailboxError> {
        // Load conversation state
        let conversation = self
            .conversation_manager
            .load_conversation(peer_id)?
            .ok_or(MailboxError::ConversationNotFound)?;

        let our_mailbox = conversation
            .our_mailbox
            .ok_or(MailboxError::NoMailboxConfigured)?;

        // Check mailbox not expired
        if our_mailbox.is_expired() {
            return Err(MailboxError::TokenExpired);
        }

        // Get challenge from server
        let challenge_response = mailbox_client
            .get_mailbox_challenge(our_mailbox.public_key.to_vec())
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        // Sign challenge with our mailbox private key
        let challenge_signature =
            self.sign_challenge(&our_mailbox, &challenge_response.challenge)?;

        // Poll for messages
        let poll_response = mailbox_client
            .poll_ephemeral_mailbox(
                our_mailbox.public_key.to_vec(),
                challenge_signature,
                challenge_response.challenge,
                after_message_id,
                DEFAULT_POLL_LIMIT,
            )
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        // Update poll timestamp
        self.conversation_manager
            .update_poll_time(&our_mailbox.id)?;

        // Convert to ReceivedMessage
        let messages: Vec<ReceivedMessage> = poll_response
            .messages
            .into_iter()
            .map(|m| ReceivedMessage {
                message_id: m.id,
                encrypted_envelope: m.encrypted_envelope,
                envelope_type: m.envelope_type,
                server_timestamp: m.timestamp,
            })
            .collect();

        let has_more = messages.len() == DEFAULT_POLL_LIMIT as usize;

        Ok(PollResult {
            peer_id: *peer_id,
            messages,
            has_more,
        })
    }

    /// Poll all active mailboxes.
    ///
    /// Polls each conversation's mailbox that is active and not recently polled.
    pub async fn poll_all_mailboxes(
        &self,
        mailbox_client: &mut MailboxClient,
    ) -> Result<Vec<PollResult>, MailboxError> {
        let active_keys = self.conversation_manager.get_active_mailbox_keys()?;
        let mut results = Vec::new();

        for (peer_id, public_key, private_key, mailbox_id) in active_keys {
            match self
                .poll_single_mailbox(mailbox_client, &peer_id, &public_key, &private_key, &mailbox_id)
                .await
            {
                Ok(result) => results.push(result),
                Err(e) => {
                    tracing::warn!(peer_id = %peer_id, error = %e, "Failed to poll mailbox");
                    // Continue with other mailboxes
                }
            }
        }

        Ok(results)
    }

    /// Poll a single mailbox by key.
    ///
    /// Paginates through all available messages until the mailbox is empty.
    /// Each page requires a new challenge since challenges are single-use.
    async fn poll_single_mailbox(
        &self,
        mailbox_client: &mut MailboxClient,
        peer_id: &UserId,
        public_key: &[u8; 32],
        private_key: &[u8; 64],
        mailbox_id: &str,
    ) -> Result<PollResult, MailboxError> {
        // Extract signing key once - 32-byte secret from 64-byte keypair
        let secret_bytes: [u8; 32] = private_key[..32]
            .try_into()
            .map_err(|_| MailboxError::BlindSignature("Invalid private key format".to_string()))?;
        let signing_key = SigningKey::from_bytes(&secret_bytes);

        let mut all_messages: Vec<ReceivedMessage> = Vec::new();
        let mut after_message_id: Option<String> = None;

        // Paginate through all messages
        loop {
            // Get a fresh challenge for each poll (challenges are single-use)
            let challenge_response = mailbox_client
                .get_mailbox_challenge(public_key.to_vec())
                .await
                .map_err(|e| MailboxError::Network(e.to_string()))?;

            let signature = signing_key.sign(&challenge_response.challenge);

            // Poll with cursor
            let poll_response = mailbox_client
                .poll_ephemeral_mailbox(
                    public_key.to_vec(),
                    signature.to_bytes().to_vec(),
                    challenge_response.challenge,
                    after_message_id.as_deref(),
                    DEFAULT_POLL_LIMIT,
                )
                .await
                .map_err(|e| MailboxError::Network(e.to_string()))?;

            let page_count = poll_response.messages.len();
            let has_more = page_count == DEFAULT_POLL_LIMIT as usize;

            // Track last message ID for pagination cursor
            if let Some(last_msg) = poll_response.messages.last() {
                after_message_id = Some(last_msg.id.clone());
            }

            // Convert and collect messages
            for m in poll_response.messages {
                all_messages.push(ReceivedMessage {
                    message_id: m.id,
                    encrypted_envelope: m.encrypted_envelope,
                    envelope_type: m.envelope_type,
                    server_timestamp: m.timestamp,
                });
            }

            // Stop if no more messages
            if !has_more {
                break;
            }
        }

        // Update poll timestamp
        self.conversation_manager.update_poll_time(mailbox_id)?;

        Ok(PollResult {
            peer_id: *peer_id,
            messages: all_messages,
            has_more: false, // We've fetched everything
        })
    }

    /// Sign a challenge with mailbox private key.
    fn sign_challenge(
        &self,
        mailbox: &DmMailbox,
        challenge: &[u8],
    ) -> Result<Vec<u8>, MailboxError> {
        // Use DmMailbox's built-in sign_challenge method
        mailbox.sign_challenge(challenge)
    }

    /// Acknowledge messages (delete from server).
    pub async fn acknowledge_messages(
        &self,
        peer_id: &UserId,
        message_ids: Vec<String>,
        mailbox_client: &mut MailboxClient,
    ) -> Result<(), MailboxError> {
        let conversation = self
            .conversation_manager
            .load_conversation(peer_id)?
            .ok_or(MailboxError::ConversationNotFound)?;

        let our_mailbox = conversation
            .our_mailbox
            .ok_or(MailboxError::NoMailboxConfigured)?;

        // Get challenge
        let challenge_response = mailbox_client
            .get_mailbox_challenge(our_mailbox.public_key.to_vec())
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        // Sign challenge
        let challenge_signature =
            self.sign_challenge(&our_mailbox, &challenge_response.challenge)?;

        // Acknowledge
        mailbox_client
            .acknowledge_messages(
                our_mailbox.public_key.to_vec(),
                challenge_signature,
                challenge_response.challenge,
                message_ids,
            )
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        Ok(())
    }

    /// Send an encrypted envelope to peer's mailbox.
    ///
    /// The envelope should already be encrypted (sealed sender format).
    /// This method handles delivery to the ephemeral mailbox.
    ///
    /// # Arguments
    ///
    /// * `peer_id` - The conversation peer
    /// * `encrypted_envelope` - Pre-encrypted message content
    /// * `envelope_type` - Type of envelope (SealedSender, PreKey, Message)
    /// * `mailbox_client` - For mailbox operations
    pub async fn send_to_peer(
        &self,
        peer_id: &UserId,
        encrypted_envelope: Vec<u8>,
        envelope_type: EnvelopeType,
        mailbox_client: &mut MailboxClient,
    ) -> Result<SendResult, MailboxError> {
        // Load conversation state
        let conversation = self
            .conversation_manager
            .load_conversation(peer_id)?
            .ok_or(MailboxError::ConversationNotFound)?;

        // Check we have established shared secret for encryption
        if !conversation.is_ready_to_send() {
            return Err(MailboxError::ConversationNotEstablished);
        }

        let peer_mailbox = conversation
            .peer_mailbox
            .ok_or(MailboxError::PeerMailboxUnknown)?;

        // Send to peer's ephemeral mailbox (anonymous - no auth needed)
        let response = mailbox_client
            .send_to_ephemeral_mailbox(
                peer_mailbox.public_key.to_vec(),
                encrypted_envelope,
                envelope_type as i32,
            )
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        // SendToEphemeralMailboxResponse doesn't have a timestamp, use 0 as placeholder
        // The caller can use their local timestamp if needed
        Ok(SendResult {
            peer_id: *peer_id,
            message_id: response.message_id,
            server_timestamp: 0, // Not provided by server response
        })
    }

    /// Check if conversation is ready for sending.
    pub fn can_send(&self, peer_id: &UserId) -> Result<bool, MailboxError> {
        let conversation = self.conversation_manager.load_conversation(peer_id)?;
        Ok(conversation
            .map(|c| c.is_ready_to_send())
            .unwrap_or(false))
    }

    /// Check if conversation is ready for receiving.
    pub fn can_receive(&self, peer_id: &UserId) -> Result<bool, MailboxError> {
        let conversation = self.conversation_manager.load_conversation(peer_id)?;
        Ok(conversation
            .map(|c| c.is_ready_to_receive())
            .unwrap_or(false))
    }

    /// Get conversations that need mailbox refresh.
    pub fn get_expiring_conversations(
        &self,
        within_hours: i64,
    ) -> Result<Vec<UserId>, MailboxError> {
        self.conversation_manager.get_expiring_mailboxes(within_hours)
    }

    /// Clean up expired mailbox state.
    pub fn cleanup_expired(&self) -> Result<u32, MailboxError> {
        self.conversation_manager.cleanup_expired()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_poll_result_structure() {
        let peer_id = UserId::new();
        let result = PollResult {
            peer_id,
            messages: vec![
                ReceivedMessage {
                    message_id: "msg-1".to_string(),
                    encrypted_envelope: vec![1, 2, 3],
                    envelope_type: EnvelopeType::SealedSender as i32,
                    server_timestamp: 1234567890,
                },
                ReceivedMessage {
                    message_id: "msg-2".to_string(),
                    encrypted_envelope: vec![4, 5, 6],
                    envelope_type: EnvelopeType::Message as i32,
                    server_timestamp: 1234567891,
                },
            ],
            has_more: false,
        };

        assert_eq!(result.peer_id, peer_id);
        assert_eq!(result.messages.len(), 2);
        assert!(!result.has_more);
    }

    #[test]
    fn test_received_message_structure() {
        let msg = ReceivedMessage {
            message_id: "test-msg".to_string(),
            encrypted_envelope: vec![0u8; 512],
            envelope_type: EnvelopeType::PrekeyMessage as i32,
            server_timestamp: 1234567890,
        };

        assert_eq!(msg.message_id, "test-msg");
        assert_eq!(msg.encrypted_envelope.len(), 512);
        assert_eq!(msg.envelope_type, EnvelopeType::PrekeyMessage as i32);
        assert_eq!(msg.server_timestamp, 1234567890);
    }

    #[test]
    fn test_send_result_structure() {
        let peer_id = UserId::new();
        let result = SendResult {
            peer_id,
            message_id: "sent-msg".to_string(),
            server_timestamp: 9876543210,
        };

        assert_eq!(result.peer_id, peer_id);
        assert_eq!(result.message_id, "sent-msg");
        assert_eq!(result.server_timestamp, 9876543210);
    }

    #[test]
    fn test_default_poll_limit() {
        assert_eq!(DEFAULT_POLL_LIMIT, 50);
    }

    #[test]
    fn test_has_more_calculation() {
        // has_more should be true when we get exactly the limit
        let messages: Vec<ReceivedMessage> = (0..DEFAULT_POLL_LIMIT)
            .map(|i| ReceivedMessage {
                message_id: format!("msg-{}", i),
                encrypted_envelope: vec![],
                envelope_type: 0,
                server_timestamp: 0,
            })
            .collect();

        let has_more = messages.len() == DEFAULT_POLL_LIMIT as usize;
        assert!(has_more);

        // has_more should be false when we get less than the limit
        let fewer_messages: Vec<ReceivedMessage> = (0..10)
            .map(|i| ReceivedMessage {
                message_id: format!("msg-{}", i),
                encrypted_envelope: vec![],
                envelope_type: 0,
                server_timestamp: 0,
            })
            .collect();

        let has_more_fewer = fewer_messages.len() == DEFAULT_POLL_LIMIT as usize;
        assert!(!has_more_fewer);
    }

    #[test]
    fn test_envelope_types() {
        // Verify envelope type values
        assert_eq!(EnvelopeType::Unspecified as i32, 0);
        assert_eq!(EnvelopeType::SealedSender as i32, 1);
        assert_eq!(EnvelopeType::PrekeyMessage as i32, 2);
        assert_eq!(EnvelopeType::Message as i32, 3);
    }
}
