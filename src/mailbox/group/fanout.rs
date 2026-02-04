//! Group fanout.
//!
//! Client-side N-way message distribution for group messaging.
//! Sends encrypted messages to all group members' ephemeral mailboxes in parallel.

use std::sync::Arc;

use futures::future::join_all;
use tokio::sync::Mutex;

use crate::domain::{ChannelId, UserId};
use crate::error::MailboxError;
use crate::mailbox::group::{GroupMailboxManager, MemberMailbox};
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
}
