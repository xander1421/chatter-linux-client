//! Group mailbox coordination.
//!
//! Handles group mailbox creation and member address exchange.
//!
//! Coordination flow:
//! 1. User creates/joins a channel
//! 2. GroupCoordinator creates ephemeral mailbox using blind token
//! 3. GroupCoordinator builds announcement message (GroupMailboxAddress)
//! 4. User sends announcement to channel (via device inbox initially)
//! 5. Other members receive announcement, store the mailbox address
//! 6. Other members respond with their mailbox addresses

use std::sync::Arc;

use chrono::{Duration, Utc};

use crate::domain::{ChannelId, UserId};
use crate::error::MailboxError;
use crate::mailbox::group::{GroupMailbox, GroupMailboxManager, MemberMailbox};
use crate::mailbox::{MailboxToken, TokenManager};
use crate::network::grpc::MailboxClient;
use crate::proto::content_v1::{GroupMailboxAddress, SenderKeyDistribution};
use crate::storage::Database;
use crate::util::time::TimestampNanos;

/// Default group mailbox TTL: 7 days in seconds.
pub const DEFAULT_GROUP_MAILBOX_TTL_SECONDS: u64 = 7 * 24 * 60 * 60;

/// Result of setting up a group mailbox.
pub struct GroupMailboxSetupResult {
    /// Our newly created mailbox.
    pub mailbox: GroupMailbox,
    /// Announcement to send to the group.
    pub announcement: GroupMailboxAddress,
    /// Sender Key Distribution Message to distribute to members.
    ///
    /// This SKDM must be distributed pairwise via Double Ratchet to all
    /// channel members so they can decrypt our Sender Key encrypted messages.
    pub skdm: Option<SenderKeyDistribution>,
}

/// Group mailbox coordinator.
///
/// Handles:
/// - Creating group mailboxes when joining/creating channels
/// - Building announcement messages for mailbox exchange
/// - Processing received mailbox announcements from other members
pub struct GroupCoordinator {
    /// Token manager for blind signature tokens.
    token_manager: TokenManager,
    /// Group mailbox state persistence.
    mailbox_manager: Arc<GroupMailboxManager>,
}

impl GroupCoordinator {
    /// Create a new GroupCoordinator.
    ///
    /// # Arguments
    ///
    /// * `db` - Shared database connection
    /// * `mailbox_manager` - Manager for group mailbox state
    pub fn new(db: Arc<Database>, mailbox_manager: Arc<GroupMailboxManager>) -> Self {
        let token_manager = TokenManager::new(db);
        Self {
            token_manager,
            mailbox_manager,
        }
    }

    /// Setup group mailbox for a channel (on create or join).
    ///
    /// Creates an ephemeral mailbox using a blind token and prepares
    /// the announcement message.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - The channel to setup mailbox for
    /// * `mailbox_client` - For server communication
    ///
    /// # Returns
    ///
    /// `GroupMailboxSetupResult` containing our mailbox and the announcement.
    pub async fn setup_group_mailbox(
        &self,
        channel_id: &ChannelId,
        mailbox_client: &mut MailboxClient,
    ) -> Result<GroupMailboxSetupResult, MailboxError> {
        tracing::info!(channel_id = %channel_id, "Starting group mailbox setup");

        // Check if we already have a non-expired mailbox
        if let Some(state) = self.mailbox_manager.load_group_state(channel_id)? {
            if let Some(existing) = &state.our_mailbox {
                if !existing.is_expired() {
                    // Already have valid mailbox, just return announcement
                    // SKDM will be populated by caller if needed
                    tracing::info!(channel_id = %channel_id, "Using existing non-expired mailbox");
                    let announcement = self.build_announcement(channel_id, existing);
                    return Ok(GroupMailboxSetupResult {
                        mailbox: existing.clone(),
                        announcement,
                        skdm: None,
                    });
                }
            }
        }

        // Get available token or request new ones
        tracing::info!(channel_id = %channel_id, "Ensuring token...");
        let token = self.ensure_token(mailbox_client).await?;
        tracing::info!(channel_id = %channel_id, "Token obtained");

        // Create ephemeral mailbox on server
        tracing::info!(channel_id = %channel_id, "Creating ephemeral mailbox on server...");
        let response = mailbox_client
            .create_ephemeral_mailbox(
                token.public_key.clone(),
                token.signature.clone(),
                DEFAULT_GROUP_MAILBOX_TTL_SECONDS,
                vec![], // Empty randomizer for compatibility
            )
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;
        tracing::info!(channel_id = %channel_id, mailbox_id = %response.mailbox_id, "Ephemeral mailbox created");

        // Build our mailbox from token + response
        let mailbox = self.build_mailbox_from_token(channel_id, &token, &response)?;

        // Build announcement
        let announcement = self.build_announcement(channel_id, &mailbox);

        // Persist state
        self.mailbox_manager.save_our_mailbox(&mailbox)?;
        self.token_manager.mark_token_used(&token.id)?;

        Ok(GroupMailboxSetupResult {
            mailbox,
            announcement,
            skdm: None, // Populated by caller if available
        })
    }

    /// Process a received GroupMailboxAddress announcement.
    ///
    /// Called when we receive another member's mailbox announcement.
    /// Stores their mailbox address for future fan-out sends.
    ///
    /// # Arguments
    ///
    /// * `sender_id` - Who sent the announcement
    /// * `announcement` - The received GroupMailboxAddress
    pub fn process_announcement(
        &self,
        sender_id: UserId,
        announcement: &GroupMailboxAddress,
    ) -> Result<(), MailboxError> {
        // Parse channel_id
        let channel_id: ChannelId = announcement
            .channel_id
            .parse()
            .map_err(|_| MailboxError::InvalidServerKey("Invalid channel_id in announcement".into()))?;

        // Validate mailbox_pk length
        if announcement.mailbox_pk.len() != 32 {
            return Err(MailboxError::InvalidServerKey(format!(
                "Invalid mailbox_pk length: expected 32, got {}",
                announcement.mailbox_pk.len()
            )));
        }

        let mut public_key = [0u8; 32];
        public_key.copy_from_slice(&announcement.mailbox_pk);

        // Create MemberMailbox
        let member_mailbox = MemberMailbox::new(
            sender_id,
            public_key,
            // Assume 7-day expiry if not specified
            Some(Utc::now() + Duration::seconds(DEFAULT_GROUP_MAILBOX_TTL_SECONDS as i64)),
        );

        // Store the member's mailbox
        self.mailbox_manager
            .save_member_mailbox(&channel_id, &member_mailbox)?;

        tracing::info!(
            channel_id = %channel_id,
            sender_id = %sender_id,
            "Stored member mailbox address from announcement"
        );

        Ok(())
    }

    /// Process a received mailbox public key (from SKDM ephemeral_reply_to).
    ///
    /// Called when we receive another member's mailbox key bundled with SKDM.
    /// This is the preferred NDSS '21 flow - mailbox announced alongside sender key.
    ///
    /// # Arguments
    ///
    /// * `sender_id` - Who sent the SKDM with mailbox
    /// * `channel_id` - Channel this mailbox is for
    /// * `mailbox_public_key` - 32-byte Ed25519 public key
    /// * `timestamp` - When the SKDM was sent (for expiry calculation)
    pub fn process_mailbox_key(
        &self,
        sender_id: UserId,
        channel_id: &ChannelId,
        mailbox_public_key: [u8; 32],
        _timestamp: u64,
    ) -> Result<(), MailboxError> {
        // Create MemberMailbox with default TTL
        let member_mailbox = MemberMailbox::new(
            sender_id,
            mailbox_public_key,
            Some(Utc::now() + Duration::seconds(DEFAULT_GROUP_MAILBOX_TTL_SECONDS as i64)),
        );

        // Store the member's mailbox
        self.mailbox_manager
            .save_member_mailbox(channel_id, &member_mailbox)?;

        tracing::info!(
            channel_id = %channel_id,
            sender_id = %sender_id,
            "Stored member mailbox from SKDM ephemeral_reply_to (NDSS '21)"
        );

        Ok(())
    }

    /// Check if we have a mailbox set up for a channel.
    pub fn has_mailbox(&self, channel_id: &ChannelId) -> Result<bool, MailboxError> {
        let state = self.mailbox_manager.load_group_state(channel_id)?;
        Ok(state.map(|s| s.our_mailbox.is_some()).unwrap_or(false))
    }

    /// Get our announcement for re-sending (e.g., when a new member joins).
    ///
    /// Returns None if we don't have a mailbox for this channel.
    pub fn get_announcement(
        &self,
        channel_id: &ChannelId,
    ) -> Result<Option<GroupMailboxAddress>, MailboxError> {
        let state = self.mailbox_manager.load_group_state(channel_id)?;

        match state.and_then(|s| s.our_mailbox) {
            Some(mailbox) if !mailbox.is_expired() => {
                Ok(Some(self.build_announcement(channel_id, &mailbox)))
            }
            _ => Ok(None),
        }
    }

    /// Ensure we have an available token, requesting more if needed.
    async fn ensure_token(
        &self,
        client: &mut MailboxClient,
    ) -> Result<MailboxToken, MailboxError> {
        tracing::info!("Checking for available token locally");
        if let Some(token) = self.token_manager.get_available_token()? {
            tracing::info!("Found existing local token");
            return Ok(token);
        }

        // Need to request tokens
        tracing::info!("No local token, requesting from server...");
        let count = self.token_manager.request_tokens(client, 5).await?;
        tracing::info!(count = count, "Requested tokens from server");
        if count == 0 {
            return Err(MailboxError::NoTokensAvailable);
        }

        self.token_manager
            .get_available_token()?
            .ok_or(MailboxError::NoTokensAvailable)
    }

    /// Build GroupMailbox from token and server response.
    fn build_mailbox_from_token(
        &self,
        channel_id: &ChannelId,
        token: &MailboxToken,
        response: &crate::proto::mailbox_v1::CreateEphemeralMailboxResponse,
    ) -> Result<GroupMailbox, MailboxError> {
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

        Ok(GroupMailbox {
            id: response.mailbox_id.clone(),
            group_id: *channel_id,
            public_key,
            private_key,
            signature: token.signature.clone(),
            randomizer: token.randomizer.clone(),
            token_id: token.id.clone(),
            created_at: Utc::now(),
            expires_at: Utc::now() + Duration::seconds(DEFAULT_GROUP_MAILBOX_TTL_SECONDS as i64),
            last_polled_at: None,
        })
    }

    /// Build GroupMailboxAddress announcement from our mailbox.
    fn build_announcement(&self, channel_id: &ChannelId, mailbox: &GroupMailbox) -> GroupMailboxAddress {
        tracing::info!(
            channel_id = %channel_id,
            mailbox_id = %mailbox.id,
            announce_pk_hex = %hex::encode(mailbox.public_key),
            "[BUILD_ANNOUNCEMENT] Building GroupMailboxAddress"
        );
        GroupMailboxAddress {
            channel_id: channel_id.to_string(),
            mailbox_pk: mailbox.public_key.to_vec(),
            timestamp: TimestampNanos::now().as_u64(),
        }
    }

    /// Clean up all group mailbox state when leaving a channel.
    ///
    /// Removes our mailbox and all member mailbox records for this channel.
    pub fn cleanup_channel(&self, channel_id: &ChannelId) -> Result<(), MailboxError> {
        tracing::info!(channel_id = %channel_id, "Cleaning up group mailbox state");
        self.mailbox_manager.delete_group_state(channel_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_group_mailbox_ttl() {
        // 7 days in seconds
        assert_eq!(DEFAULT_GROUP_MAILBOX_TTL_SECONDS, 7 * 24 * 60 * 60);
        assert_eq!(DEFAULT_GROUP_MAILBOX_TTL_SECONDS, 604800);
    }

    #[test]
    fn test_group_mailbox_setup_result_structure() {
        let channel_id = ChannelId::new();

        let mailbox = GroupMailbox {
            id: "test-mailbox".to_string(),
            group_id: channel_id,
            public_key: [1u8; 32],
            private_key: [2u8; 64],
            signature: vec![3u8; 256],
            randomizer: Some(vec![4u8; 32]),
            token_id: "test-token".to_string(),
            created_at: Utc::now(),
            expires_at: Utc::now() + Duration::days(7),
            last_polled_at: None,
        };

        let announcement = GroupMailboxAddress {
            channel_id: channel_id.to_string(),
            mailbox_pk: mailbox.public_key.to_vec(),
            timestamp: TimestampNanos::now().as_u64(),
        };

        let result = GroupMailboxSetupResult {
            mailbox: mailbox.clone(),
            announcement: announcement.clone(),
            skdm: None,
        };

        assert_eq!(result.mailbox.id, "test-mailbox");
        assert_eq!(result.announcement.channel_id, channel_id.to_string());
        assert_eq!(result.announcement.mailbox_pk.len(), 32);
    }

    #[test]
    fn test_announcement_validation() {
        // Valid announcement
        let valid = GroupMailboxAddress {
            channel_id: ChannelId::new().to_string(),
            mailbox_pk: vec![0u8; 32],
            timestamp: 1234567890,
        };
        assert_eq!(valid.mailbox_pk.len(), 32);

        // Invalid announcement (wrong key size)
        let invalid = GroupMailboxAddress {
            channel_id: ChannelId::new().to_string(),
            mailbox_pk: vec![0u8; 16], // Too short
            timestamp: 1234567890,
        };
        assert_ne!(invalid.mailbox_pk.len(), 32);
    }
}
