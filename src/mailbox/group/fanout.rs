//! Group fanout.
//!
//! Client-side N-way message distribution for group messaging.
//! Sends encrypted messages to all group members' ephemeral mailboxes in parallel.
//! Also provides polling support for receiving messages from group mailboxes.

use std::sync::Arc;

use ed25519_dalek::{Signer, SigningKey};
use futures::future::join_all;
use tokio::sync::Mutex;

use crate::domain::{ChannelId, UserId};
use crate::error::MailboxError;
use crate::mailbox::dm::ReceivedMessage;
use crate::mailbox::group::{GroupMailbox, GroupMailboxManager, MemberMailbox};
use crate::network::grpc::MailboxClient;
use crate::proto::mailbox_v1::EnvelopeType;

/// Maximum messages to fetch per poll.
const DEFAULT_POLL_LIMIT: i32 = 50;

/// Result of a single fanout delivery attempt.
#[derive(Debug, Clone)]
pub struct FanoutResult {
    /// The target member's user ID.
    pub user_id: UserId,
    /// Whether delivery succeeded.
    pub success: bool,
    /// Server-assigned message ID (if successful).
    pub message_id: Option<String>,
    /// Error message (if failed).
    pub error: Option<String>,
}

impl FanoutResult {
    /// Create a successful fanout result.
    pub fn success(user_id: UserId, message_id: String) -> Self {
        Self {
            user_id,
            success: true,
            message_id: Some(message_id),
            error: None,
        }
    }

    /// Create a failed fanout result.
    pub fn failure(user_id: UserId, error: String) -> Self {
        Self {
            user_id,
            success: false,
            message_id: None,
            error: Some(error),
        }
    }
}

/// Result of a group fanout operation.
#[derive(Debug)]
pub struct GroupFanoutResult {
    /// The group this fanout was for.
    pub group_id: ChannelId,
    /// Total number of members attempted.
    pub total_members: usize,
    /// Number of successful deliveries.
    pub successful: usize,
    /// Number of failed deliveries.
    pub failed: usize,
    /// Per-member delivery results.
    pub results: Vec<FanoutResult>,
}

impl GroupFanoutResult {
    /// Check if all deliveries succeeded.
    pub fn is_complete_success(&self) -> bool {
        self.failed == 0
    }

    /// Check if all deliveries failed.
    pub fn is_complete_failure(&self) -> bool {
        self.successful == 0
    }

    /// Get list of failed member IDs.
    pub fn failed_members(&self) -> Vec<UserId> {
        self.results
            .iter()
            .filter(|r| !r.success)
            .map(|r| r.user_id)
            .collect()
    }

    /// Get list of successful member IDs.
    pub fn successful_members(&self) -> Vec<UserId> {
        self.results
            .iter()
            .filter(|r| r.success)
            .map(|r| r.user_id)
            .collect()
    }
}

/// Result of polling a group mailbox.
#[derive(Debug)]
pub struct GroupPollResult {
    /// The group this poll was for.
    pub group_id: ChannelId,
    /// Messages received (encrypted envelopes).
    pub messages: Vec<ReceivedMessage>,
    /// Whether there may be more messages.
    pub has_more: bool,
}

impl GroupPollResult {
    /// Check if any messages were received.
    pub fn has_messages(&self) -> bool {
        !self.messages.is_empty()
    }

    /// Get the number of messages received.
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    /// Get message IDs for acknowledgment.
    pub fn message_ids(&self) -> Vec<String> {
        self.messages.iter().map(|m| m.message_id.clone()).collect()
    }
}

/// N-way message fanout for group mailboxes.
///
/// Coordinates parallel delivery of encrypted messages to all members
/// of a group via their ephemeral mailboxes.
pub struct GroupFanout {
    /// Manager for group mailbox state persistence.
    mailbox_manager: Arc<GroupMailboxManager>,
    /// gRPC client for mailbox operations.
    mailbox_client: Arc<Mutex<MailboxClient>>,
}

impl GroupFanout {
    /// Create a new GroupFanout.
    ///
    /// # Arguments
    ///
    /// * `mailbox_manager` - Manager for group mailbox state
    /// * `mailbox_client` - Client for mailbox network operations
    pub fn new(
        mailbox_manager: Arc<GroupMailboxManager>,
        mailbox_client: Arc<Mutex<MailboxClient>>,
    ) -> Self {
        Self {
            mailbox_manager,
            mailbox_client,
        }
    }

    /// Send an encrypted envelope to all group members.
    ///
    /// Delivers the message to each member's ephemeral mailbox in parallel.
    /// The envelope should already be encrypted (sealed sender format).
    ///
    /// # Arguments
    ///
    /// * `group_id` - The group to send to
    /// * `encrypted_envelope` - Pre-encrypted message content
    /// * `envelope_type` - Type of envelope (SealedSender, PreKey, Message)
    ///
    /// # Returns
    ///
    /// GroupFanoutResult with per-member delivery results.
    ///
    /// # Errors
    ///
    /// Returns `MailboxError::GroupNotFound` if group state doesn't exist.
    /// Returns `MailboxError::GroupNotReady` if no member mailboxes are configured.
    pub async fn send_to_group(
        &self,
        group_id: &ChannelId,
        encrypted_envelope: &[u8],
        envelope_type: EnvelopeType,
    ) -> Result<GroupFanoutResult, MailboxError> {
        // Load group state to get member mailboxes
        let group_state = self
            .mailbox_manager
            .load_group_state(group_id)?
            .ok_or(MailboxError::GroupNotFound)?;

        // Check we have member mailboxes
        if group_state.member_mailboxes.is_empty() {
            return Err(MailboxError::GroupNotReady);
        }

        let members: Vec<MemberMailbox> =
            group_state.member_mailboxes.values().cloned().collect();
        let total_members = members.len();

        // Create futures for parallel delivery
        let envelope_type_i32 = envelope_type as i32;
        let futures: Vec<_> = members
            .into_iter()
            .map(|member| {
                let envelope = encrypted_envelope.to_vec();
                let client = Arc::clone(&self.mailbox_client);
                async move { Self::send_to_member_internal(client, &member, &envelope, envelope_type_i32).await }
            })
            .collect();

        // Execute all deliveries in parallel
        let results = join_all(futures).await;

        // Count successes and failures
        let successful = results.iter().filter(|r| r.success).count();
        let failed = results.iter().filter(|r| !r.success).count();

        Ok(GroupFanoutResult {
            group_id: *group_id,
            total_members,
            successful,
            failed,
            results,
        })
    }

    /// Send to a single member's mailbox.
    ///
    /// # Arguments
    ///
    /// * `member` - The member's mailbox info
    /// * `encrypted_envelope` - Pre-encrypted message content
    /// * `envelope_type` - Type of envelope as i32
    pub async fn send_to_member(
        &self,
        member: &MemberMailbox,
        encrypted_envelope: &[u8],
        envelope_type: i32,
    ) -> FanoutResult {
        Self::send_to_member_internal(
            Arc::clone(&self.mailbox_client),
            member,
            encrypted_envelope,
            envelope_type,
        )
        .await
    }

    /// Internal helper for sending to a member.
    async fn send_to_member_internal(
        client: Arc<Mutex<MailboxClient>>,
        member: &MemberMailbox,
        encrypted_envelope: &[u8],
        envelope_type: i32,
    ) -> FanoutResult {
        let mut client_guard = client.lock().await;

        match client_guard
            .send_to_ephemeral_mailbox(
                member.public_key.to_vec(),
                encrypted_envelope.to_vec(),
                envelope_type,
            )
            .await
        {
            Ok(response) => FanoutResult::success(member.user_id, response.message_id),
            Err(e) => FanoutResult::failure(member.user_id, e.to_string()),
        }
    }

    /// Get list of members we don't have mailboxes for.
    ///
    /// Compares expected member IDs against known mailboxes.
    /// Used to trigger mailbox exchange for new members.
    ///
    /// # Arguments
    ///
    /// * `group_id` - The group to check
    /// * `member_ids` - Expected member IDs (from group membership)
    ///
    /// # Returns
    ///
    /// List of member IDs we need mailboxes for.
    pub fn get_missing_member_mailboxes(
        &self,
        group_id: &ChannelId,
        member_ids: &[UserId],
    ) -> Result<Vec<UserId>, MailboxError> {
        let group_state = self.mailbox_manager.load_group_state(group_id)?;

        match group_state {
            Some(state) => {
                let missing: Vec<UserId> = member_ids
                    .iter()
                    .filter(|id| !state.member_mailboxes.contains_key(id))
                    .copied()
                    .collect();
                Ok(missing)
            }
            None => {
                // No state at all - all members are missing
                Ok(member_ids.to_vec())
            }
        }
    }

    /// Check if the group is ready for sending messages.
    ///
    /// Returns true if we have at least one member mailbox.
    pub fn is_ready_to_send(&self, group_id: &ChannelId) -> Result<bool, MailboxError> {
        let state = self.mailbox_manager.load_group_state(group_id)?;
        Ok(state.is_some_and(|s| s.is_ready_to_send()))
    }

    // ==================== Polling Methods ====================

    /// Poll a group's mailbox for new messages.
    ///
    /// Authenticates with challenge-response and fetches pending messages
    /// from our group mailbox.
    ///
    /// # Arguments
    ///
    /// * `group_id` - The group to poll
    ///
    /// # Returns
    ///
    /// GroupPollResult with messages and has_more indicator.
    ///
    /// # Errors
    ///
    /// Returns `MailboxError::GroupNotFound` if no group state exists.
    /// Returns `MailboxError::NoMailboxConfigured` if we don't have a mailbox for this group.
    pub async fn poll_group_mailbox(
        &self,
        group_id: &ChannelId,
    ) -> Result<GroupPollResult, MailboxError> {
        // Load our mailbox for this group
        let group_state = self
            .mailbox_manager
            .load_group_state(group_id)?
            .ok_or(MailboxError::GroupNotFound)?;

        let our_mailbox = group_state
            .our_mailbox
            .ok_or(MailboxError::NoMailboxConfigured)?;

        // Check mailbox not expired
        if our_mailbox.is_expired() {
            return Err(MailboxError::TokenExpired);
        }

        // Get challenge from server
        let mut client = self.mailbox_client.lock().await;
        let challenge_response = client
            .get_mailbox_challenge(our_mailbox.public_key.to_vec())
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        // Sign challenge with our mailbox private key
        let challenge_signature = self.sign_challenge(&our_mailbox, &challenge_response.challenge)?;

        // Poll for messages
        let poll_response = client
            .poll_ephemeral_mailbox(
                our_mailbox.public_key.to_vec(),
                challenge_signature,
                challenge_response.challenge,
                None,
                DEFAULT_POLL_LIMIT,
            )
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        // Update poll timestamp
        self.mailbox_manager.update_poll_time(&our_mailbox.id)?;

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

        Ok(GroupPollResult {
            group_id: *group_id,
            messages,
            has_more,
        })
    }

    /// Poll all group mailboxes for new messages.
    ///
    /// Polls each group's mailbox in parallel and returns results per group.
    /// Continues polling other groups if one fails.
    ///
    /// # Returns
    ///
    /// Vector of (ChannelId, GroupPollResult) for each group with results.
    pub async fn poll_all_group_mailboxes(
        &self,
    ) -> Result<Vec<(ChannelId, GroupPollResult)>, MailboxError> {
        // Get all active group mailboxes
        let group_mailboxes = self.mailbox_manager.get_all_group_mailboxes()?;

        if group_mailboxes.is_empty() {
            return Ok(Vec::new());
        }

        // Create futures for parallel polling
        let futures: Vec<_> = group_mailboxes
            .into_iter()
            .map(|(group_id, public_key, private_key, mailbox_id)| {
                let client = Arc::clone(&self.mailbox_client);
                let manager = Arc::clone(&self.mailbox_manager);
                async move {
                    let result = Self::poll_single_group_internal(
                        client,
                        manager,
                        &group_id,
                        &public_key,
                        &private_key,
                        &mailbox_id,
                    )
                    .await;
                    (group_id, result)
                }
            })
            .collect();

        // Execute all polls in parallel
        let results = join_all(futures).await;

        // Filter to only successful results
        let successful: Vec<(ChannelId, GroupPollResult)> = results
            .into_iter()
            .filter_map(|(group_id, result)| match result {
                Ok(poll_result) => Some((group_id, poll_result)),
                Err(e) => {
                    tracing::warn!(group_id = %group_id, error = %e, "Failed to poll group mailbox");
                    None
                }
            })
            .collect();

        Ok(successful)
    }

    /// Internal helper for polling a single group mailbox.
    async fn poll_single_group_internal(
        client: Arc<Mutex<MailboxClient>>,
        manager: Arc<GroupMailboxManager>,
        group_id: &ChannelId,
        public_key: &[u8; 32],
        private_key: &[u8; 64],
        mailbox_id: &str,
    ) -> Result<GroupPollResult, MailboxError> {
        let mut client_guard = client.lock().await;

        // Get challenge
        let challenge_response = client_guard
            .get_mailbox_challenge(public_key.to_vec())
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        // Sign challenge - extract 32-byte secret from 64-byte keypair
        let secret_bytes: [u8; 32] = private_key[..32]
            .try_into()
            .map_err(|_| MailboxError::BlindSignature("Invalid private key format".to_string()))?;
        let signing_key = SigningKey::from_bytes(&secret_bytes);
        let signature = signing_key.sign(&challenge_response.challenge);

        // Poll
        let poll_response = client_guard
            .poll_ephemeral_mailbox(
                public_key.to_vec(),
                signature.to_bytes().to_vec(),
                challenge_response.challenge,
                None,
                DEFAULT_POLL_LIMIT,
            )
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        // Update poll timestamp
        manager.update_poll_time(mailbox_id)?;

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

        Ok(GroupPollResult {
            group_id: *group_id,
            messages,
            has_more,
        })
    }

    /// Acknowledge messages from a group mailbox.
    ///
    /// Acknowledges (and deletes) messages from the group mailbox.
    /// Requires challenge-response authentication.
    ///
    /// # Arguments
    ///
    /// * `group_id` - The group whose mailbox to acknowledge from
    /// * `message_ids` - Message IDs to acknowledge (will be deleted)
    ///
    /// # Returns
    ///
    /// Number of messages acknowledged.
    pub async fn acknowledge_group_messages(
        &self,
        group_id: &ChannelId,
        message_ids: Vec<String>,
    ) -> Result<u32, MailboxError> {
        if message_ids.is_empty() {
            return Ok(0);
        }

        // Load our mailbox for this group
        let group_state = self
            .mailbox_manager
            .load_group_state(group_id)?
            .ok_or(MailboxError::GroupNotFound)?;

        let our_mailbox = group_state
            .our_mailbox
            .ok_or(MailboxError::NoMailboxConfigured)?;

        // Get challenge from server
        let mut client = self.mailbox_client.lock().await;
        let challenge_response = client
            .get_mailbox_challenge(our_mailbox.public_key.to_vec())
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        // Sign challenge
        let challenge_signature = self.sign_challenge(&our_mailbox, &challenge_response.challenge)?;

        // Acknowledge
        let ack_count = message_ids.len() as u32;
        client
            .acknowledge_messages(
                our_mailbox.public_key.to_vec(),
                challenge_signature,
                challenge_response.challenge,
                message_ids,
            )
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        Ok(ack_count)
    }

    /// Sign a challenge with mailbox private key.
    fn sign_challenge(
        &self,
        mailbox: &GroupMailbox,
        challenge: &[u8],
    ) -> Result<Vec<u8>, MailboxError> {
        mailbox.sign_challenge(challenge)
    }

    /// Check if the group is ready for receiving messages.
    ///
    /// Returns true if we have a non-expired mailbox.
    pub fn is_ready_to_receive(&self, group_id: &ChannelId) -> Result<bool, MailboxError> {
        let state = self.mailbox_manager.load_group_state(group_id)?;
        Ok(state.is_some_and(|s| s.is_ready_to_receive()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fanout_result_success() {
        let user_id = UserId::new();
        let result = FanoutResult::success(user_id, "msg-123".to_string());

        assert_eq!(result.user_id, user_id);
        assert!(result.success);
        assert_eq!(result.message_id, Some("msg-123".to_string()));
        assert!(result.error.is_none());
    }

    #[test]
    fn test_fanout_result_failure() {
        let user_id = UserId::new();
        let result = FanoutResult::failure(user_id, "network error".to_string());

        assert_eq!(result.user_id, user_id);
        assert!(!result.success);
        assert!(result.message_id.is_none());
        assert_eq!(result.error, Some("network error".to_string()));
    }

    #[test]
    fn test_group_fanout_result_complete_success() {
        let group_id = ChannelId::new();
        let user1 = UserId::new();
        let user2 = UserId::new();

        let result = GroupFanoutResult {
            group_id,
            total_members: 2,
            successful: 2,
            failed: 0,
            results: vec![
                FanoutResult::success(user1, "msg-1".to_string()),
                FanoutResult::success(user2, "msg-2".to_string()),
            ],
        };

        assert!(result.is_complete_success());
        assert!(!result.is_complete_failure());
        assert!(result.failed_members().is_empty());
        assert_eq!(result.successful_members().len(), 2);
    }

    #[test]
    fn test_group_fanout_result_partial_failure() {
        let group_id = ChannelId::new();
        let user1 = UserId::new();
        let user2 = UserId::new();

        let result = GroupFanoutResult {
            group_id,
            total_members: 2,
            successful: 1,
            failed: 1,
            results: vec![
                FanoutResult::success(user1, "msg-1".to_string()),
                FanoutResult::failure(user2, "network error".to_string()),
            ],
        };

        assert!(!result.is_complete_success());
        assert!(!result.is_complete_failure());
        assert_eq!(result.failed_members(), vec![user2]);
        assert_eq!(result.successful_members(), vec![user1]);
    }

    #[test]
    fn test_group_fanout_result_complete_failure() {
        let group_id = ChannelId::new();
        let user1 = UserId::new();
        let user2 = UserId::new();

        let result = GroupFanoutResult {
            group_id,
            total_members: 2,
            successful: 0,
            failed: 2,
            results: vec![
                FanoutResult::failure(user1, "error 1".to_string()),
                FanoutResult::failure(user2, "error 2".to_string()),
            ],
        };

        assert!(!result.is_complete_success());
        assert!(result.is_complete_failure());
        assert_eq!(result.failed_members().len(), 2);
        assert!(result.successful_members().is_empty());
    }

    #[test]
    fn test_default_poll_limit() {
        assert_eq!(DEFAULT_POLL_LIMIT, 50);
    }

    #[test]
    fn test_group_poll_result_empty() {
        let group_id = ChannelId::new();
        let result = GroupPollResult {
            group_id,
            messages: vec![],
            has_more: false,
        };

        assert!(!result.has_messages());
        assert_eq!(result.message_count(), 0);
        assert!(result.message_ids().is_empty());
    }

    #[test]
    fn test_group_poll_result_with_messages() {
        use crate::proto::mailbox_v1::EnvelopeType;

        let group_id = ChannelId::new();
        let result = GroupPollResult {
            group_id,
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
            has_more: true,
        };

        assert!(result.has_messages());
        assert_eq!(result.message_count(), 2);
        assert_eq!(result.message_ids(), vec!["msg-1", "msg-2"]);
        assert!(result.has_more);
    }

    #[test]
    fn test_group_poll_result_has_more_calculation() {
        // When we have fewer than the limit, has_more should be false
        let messages: Vec<ReceivedMessage> = (0..10)
            .map(|i| ReceivedMessage {
                message_id: format!("msg-{}", i),
                encrypted_envelope: vec![],
                envelope_type: 0,
                server_timestamp: 0,
            })
            .collect();

        let has_more = messages.len() == DEFAULT_POLL_LIMIT as usize;
        assert!(!has_more);

        // When we have exactly the limit, has_more should be true
        let full_messages: Vec<ReceivedMessage> = (0..DEFAULT_POLL_LIMIT)
            .map(|i| ReceivedMessage {
                message_id: format!("msg-{}", i),
                encrypted_envelope: vec![],
                envelope_type: 0,
                server_timestamp: 0,
            })
            .collect();

        let has_more_full = full_messages.len() == DEFAULT_POLL_LIMIT as usize;
        assert!(has_more_full);
    }
}
