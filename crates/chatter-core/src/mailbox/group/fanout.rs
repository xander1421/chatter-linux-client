//! Group fanout.
//!
//! Client-side N-way message distribution for group messaging.
//! Sends encrypted messages to all group members' ephemeral mailboxes in parallel.
//! Also provides polling support for receiving messages from group mailboxes.

use std::sync::Arc;

use ed25519_dalek::{Signer, SigningKey};
use futures::{stream, Stream, StreamExt};
use tokio::sync::Mutex;

use crate::domain::{ChannelId, UserId};
use crate::error::MailboxError;
use crate::mailbox::dm::ReceivedMessage;
use crate::mailbox::group::{GroupMailboxManager, MemberMailbox};
use crate::network::grpc::AnonymousMailboxClient;
use crate::network::grpc::client_pool::GrpcClientPool;
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

/// Result of polling a group mailbox.
#[derive(Debug)]
pub struct GroupPollResult {
    /// The group this poll was for.
    pub group_id: ChannelId,
    /// Messages received (encrypted envelopes).
    pub messages: Vec<ReceivedMessage>,
    /// Whether there may be more messages.
    pub has_more: bool,
    /// Mailbox ID for acknowledgment (not secret - just an identifier).
    pub mailbox_id: String,
}

/// A completed mailbox fetch. The caller processes each result before waiting
/// for the remaining mailboxes and acknowledges only after durable handling.
pub enum GroupPollOutcome {
    Received(ChannelId, bool, GroupPollResult),
    Failed(ChannelId, MailboxError),
    TimedOut(ChannelId),
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
    /// gRPC client for private-channel mailbox sends (follows the pool's
    /// transport). Polls use independent clients from the pool.
    mailbox_client: Arc<Mutex<AnonymousMailboxClient>>,
    /// Pool for the secret-chat client: anonymous-group mailboxes are polled
    /// and acknowledged over Tor only (`GrpcClientPool::secret_channel`).
    pool: Arc<GrpcClientPool>,
}

impl GroupFanout {
    /// Create a new GroupFanout.
    ///
    /// # Arguments
    ///
    /// * `mailbox_manager` - Manager for group mailbox state
    /// * `mailbox_client` - Client for private-channel mailbox operations
    /// * `pool` - Source of the secret-chat (Tor) client for anonymous groups
    pub fn new(
        mailbox_manager: Arc<GroupMailboxManager>,
        mailbox_client: Arc<Mutex<AnonymousMailboxClient>>,
        pool: Arc<GrpcClientPool>,
    ) -> Self {
        Self {
            mailbox_manager,
            mailbox_client,
            pool,
        }
    }

    /// Rebuild the mailbox client from the pool so subsequent group sends and
    /// sends re-dial over the pool's CURRENT transport. Polls draw fresh
    /// clients from the pool each pass.
    ///
    /// The client is captured at login (`init_group_fanout`) and a tonic
    /// `Channel` is pinned to the transport it was built with, so a direct
    /// channel can never route over onion. When anon mode toggles the pool's
    /// transport this swap is required — otherwise every group-message deposit
    /// keeps egressing the real client IP over the pre-switch transport.
    pub async fn refresh_client(&self, pool: &crate::network::grpc::GrpcClientPool) {
        *self.mailbox_client.lock().await = pool.anonymous_mailbox_client();
        tracing::info!("group fanout: rebuilt mailbox client for new transport");
    }

    /// Send pairwise-encrypted messages to group member mailboxes.
    ///
    /// Per NDSS '21, each group message is encrypted separately for each member
    /// using their pairwise Double Ratchet session (sealed sender). This provides
    /// proper forward secrecy and post-compromise security per member.
    ///
    /// # Arguments
    ///
    /// * `group_id` - The group channel ID
    /// * `member_ciphertexts` - Map of member UserId to their sealed sender ciphertext
    /// * `envelope_type` - Type of message (Message, KeyExchange, etc.)
    ///
    /// # Returns
    ///
    /// GroupFanoutResult with per-member delivery results.
    pub async fn send_to_group_pairwise(
        &self,
        group_id: &ChannelId,
        member_ciphertexts: std::collections::HashMap<UserId, Vec<u8>>,
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

        let total_members = member_ciphertexts.len();
        let envelope_type_i32 = envelope_type as i32;

        // Create futures for parallel delivery - each member gets their unique ciphertext
        let futures: Vec<_> = member_ciphertexts
            .into_iter()
            .filter_map(|(user_id, ciphertext)| {
                // Look up member's mailbox
                group_state.member_mailboxes.get(&user_id).map(|member| {
                    let member = member.clone();
                    let client = Arc::clone(&self.mailbox_client);
                    async move {
                        Self::send_to_member_internal(
                            client,
                            &member,
                            &ciphertext,
                            envelope_type_i32,
                        )
                        .await
                    }
                })
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
        client: Arc<Mutex<AnonymousMailboxClient>>,
        member: &MemberMailbox,
        encrypted_envelope: &[u8],
        envelope_type: i32,
    ) -> FanoutResult {
        let mut client_guard = client.lock().await;

        tracing::info!(
            envelope_len = encrypted_envelope.len(),
            "[FANOUT_SEND] Sending to member mailbox"
        );

        match client_guard
            .send_to_ephemeral_mailbox(
                member.public_key.to_vec(),
                encrypted_envelope.to_vec(),
                envelope_type,
            )
            .await
        {
            Ok(response) => {
                tracing::info!("[FANOUT_SEND] Success");
                FanoutResult::success(member.user_id, response.message_id)
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "[FANOUT_SEND] Failed"
                );
                FanoutResult::failure(member.user_id, e.to_string())
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

    /// Get the group mailbox state for a channel.
    ///
    /// Returns the group state including our mailbox and member mailboxes.
    /// Used for pairwise encryption where we need to know which members to encrypt for.
    pub fn get_group_state(
        &self,
        group_id: &ChannelId,
    ) -> Result<Option<super::GroupMailboxState>, MailboxError> {
        self.mailbox_manager.load_group_state(group_id)
    }

    // ==================== Polling Methods ====================

    /// Poll all group mailboxes for new messages.
    ///
    /// Polls each group's mailbox independently and yields results as they finish.
    ///
    /// # Returns
    ///
    /// The number of mailboxes and a bounded stream of completed fetches.
    pub fn poll_all_group_mailboxes(
        &self,
    ) -> Result<(usize, impl Stream<Item = GroupPollOutcome>), MailboxError> {
        // Get all active group mailboxes
        let group_mailboxes = self.mailbox_manager.get_all_group_mailboxes()?;

        let mailbox_count = group_mailboxes.len();

        tracing::debug!(
            mailbox_count = group_mailboxes.len(),
            "[GROUP_FANOUT] Found group mailboxes to poll"
        );

        // Give each mailbox its own client, with at most four fetches active.
        let futures: Vec<_> = group_mailboxes
            .into_iter()
            .map(|(group_id, public_key, private_key, mailbox_id, secret)| {
                let pool = Arc::clone(&self.pool);
                let manager = Arc::clone(&self.mailbox_manager);
                async move {
                    let client = if secret {
                        pool.secret_mailbox_client()
                    } else {
                        Ok(pool.anonymous_mailbox_client())
                    };
                    // Only the fetch is timed out. The caller processes and
                    // acknowledges every successful result without a deadline.
                    let result = match client {
                        Ok(client) => tokio::time::timeout(
                            std::time::Duration::from_secs(95),
                            Self::poll_single_group_internal(
                                Arc::new(Mutex::new(client)),
                                manager,
                                &group_id,
                                &public_key,
                                &private_key,
                                &mailbox_id,
                            ),
                        )
                        .await
                        .map_err(|_| ()),
                        Err(e) => Ok(Err(MailboxError::Network(e.to_string()))),
                    };
                    match result {
                        Ok(Ok(poll_result)) => {
                            GroupPollOutcome::Received(group_id, secret, poll_result)
                        }
                        Ok(Err(error)) => GroupPollOutcome::Failed(group_id, error),
                        Err(()) => GroupPollOutcome::TimedOut(group_id),
                    }
                }
            })
            .collect();
        Ok((mailbox_count, stream::iter(futures).buffer_unordered(4)))
    }

    /// Internal helper for polling a single group mailbox.
    async fn poll_single_group_internal(
        client: Arc<Mutex<AnonymousMailboxClient>>,
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
            mailbox_id: mailbox_id.to_string(),
        })
    }

    /// Acknowledge messages by mailbox ID.
    ///
    /// Looks up keys by mailbox_id and acknowledges messages. Works with both
    /// server-managed and anonymous group mailboxes.
    ///
    /// # Arguments
    ///
    /// * `mailbox_id` - The mailbox identifier
    /// * `message_ids` - Message IDs to acknowledge (will be deleted)
    ///
    /// # Returns
    ///
    /// Number of messages acknowledged.
    pub async fn acknowledge_by_mailbox_id(
        &self,
        mailbox_id: &str,
        message_ids: Vec<String>,
    ) -> Result<u32, MailboxError> {
        if message_ids.is_empty() {
            return Ok(0);
        }

        // Look up keys by mailbox_id (checks both tables)
        let (public_key, private_key, secret) =
            self.mailbox_manager.get_mailbox_keys_by_id(mailbox_id)?;

        // An anonymous-group mailbox is acknowledged over Tor or not at all.
        let mut secret_client;
        let mut shared_client;
        let client: &mut AnonymousMailboxClient = if secret {
            secret_client = self
                .pool
                .secret_mailbox_client()
                .map_err(|e| MailboxError::Network(e.to_string()))?;
            &mut secret_client
        } else {
            shared_client = self.mailbox_client.lock().await;
            &mut shared_client
        };

        // Get challenge from server
        let challenge_response = client
            .get_mailbox_challenge(public_key.to_vec())
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        // Sign challenge
        let secret_bytes: [u8; 32] = private_key[..32]
            .try_into()
            .map_err(|_| MailboxError::BlindSignature("Invalid private key format".to_string()))?;
        let signing_key = SigningKey::from_bytes(&secret_bytes);
        let signature = signing_key.sign(&challenge_response.challenge);

        // Acknowledge
        let ack_count = message_ids.len() as u32;
        client
            .acknowledge_messages(
                public_key.to_vec(),
                signature.to_bytes().to_vec(),
                challenge_response.challenge,
                message_ids,
            )
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        Ok(ack_count)
    }

    /// Check if the group is ready for receiving messages.
    ///
    /// Returns true if we have a non-expired mailbox.
    pub fn is_ready_to_receive(&self, group_id: &ChannelId) -> Result<bool, MailboxError> {
        let state = self.mailbox_manager.load_group_state(group_id)?;
        Ok(state.is_some_and(|s| s.is_ready_to_receive()))
    }

    // =========================================================================
    // Pending Channel Message Queue (Bootstrap Support)
    // =========================================================================

    /// Queue a pending message for the channel bootstrap phase.
    ///
    /// Messages are queued when member mailboxes aren't available yet.
    /// They're flushed when mailbox addresses are received via GroupMailboxAddress.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel the message is for
    /// * `member_id` - Target member who we don't have a mailbox for yet
    /// * `plaintext` - Message content (plaintext proto bytes)
    /// * `timestamp` - Message timestamp (nanoseconds)
    pub fn queue_pending_message(
        &self,
        channel_id: &ChannelId,
        member_id: &UserId,
        plaintext: &[u8],
        timestamp: u64,
    ) -> Result<(), MailboxError> {
        self.mailbox_manager
            .queue_pending_message(channel_id, member_id, plaintext, timestamp)
    }

    /// Drain all pending messages for a specific channel/member.
    ///
    /// Called when we receive a member's mailbox address. Returns all queued
    /// messages for that member and deletes them from the queue.
    ///
    /// # Returns
    ///
    /// Vector of (plaintext, timestamp) tuples for messages to send.
    pub fn drain_pending_messages(
        &self,
        channel_id: &ChannelId,
        member_id: &UserId,
    ) -> Result<Vec<(Vec<u8>, u64)>, MailboxError> {
        self.mailbox_manager
            .drain_pending_messages(channel_id, member_id)
    }

    /// Get count of pending messages for a channel.
    pub fn pending_message_count(&self, channel_id: &ChannelId) -> Result<usize, MailboxError> {
        self.mailbox_manager.pending_message_count(channel_id)
    }

    /// Get the mailbox manager reference for pending message operations.
    ///
    /// Used by GroupCoordinator to flush pending messages when a new member
    /// mailbox is received.
    pub fn mailbox_manager(&self) -> &Arc<GroupMailboxManager> {
        &self.mailbox_manager
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
            mailbox_id: "test-mailbox".to_string(),
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
            mailbox_id: "test-mailbox".to_string(),
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
