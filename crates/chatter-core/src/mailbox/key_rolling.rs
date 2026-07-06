//! Key rolling.
//!
//! Ephemeral key rotation and expiration for mailbox forward secrecy.
//!
//! Periodically rotates mailbox keys so that old mailboxes are eventually forgotten
//! by the server, limiting the window during which the server can correlate message
//! patterns.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::domain::{ChannelId, UserId};
use crate::error::MailboxError;
use crate::mailbox::dm::{DmConversationManager, DmMailbox};
use crate::mailbox::group::{GroupMailbox, GroupMailboxManager};
use crate::mailbox::TokenManager;
use crate::network::grpc::client_pool::GrpcClientPool;

/// Default rotation check interval (1 hour).
pub const DEFAULT_CHECK_INTERVAL_SECS: u64 = 3600;

/// Default threshold for rotation (24 hours before expiry).
pub const DEFAULT_ROTATION_THRESHOLD_HOURS: i64 = 24;

/// Result of a mailbox rotation.
#[derive(Debug)]
pub struct RotationResult {
    /// The conversation/group that was rotated.
    pub target_id: RotationTarget,
    /// The new mailbox that was created.
    pub new_mailbox_public_key: [u8; 32],
    /// When the old mailbox will be deleted.
    pub old_mailbox_deletion_at: DateTime<Utc>,
}

/// Target of a rotation operation.
#[derive(Debug, Clone)]
pub enum RotationTarget {
    /// DM conversation with a peer.
    Dm(UserId),
    /// Group mailbox.
    Group(ChannelId),
}

/// Pending rotation notification to be sent to peers.
#[derive(Debug, Clone)]
pub struct RotationNotification {
    /// Target (peer for DM, members for group).
    pub target: RotationTarget,
    /// New mailbox public key to send.
    pub new_mailbox_public_key: [u8; 32],
    /// When the old mailbox expires.
    pub old_mailbox_expires_at: DateTime<Utc>,
}

/// Key rolling service for mailbox forward secrecy.
///
/// Periodically rotates mailbox keys to limit the window during which
/// the server can correlate message patterns.
pub struct KeyRollingService {
    token_manager: Arc<TokenManager>,
    dm_manager: Arc<DmConversationManager>,
    group_manager: Arc<GroupMailboxManager>,
    /// For creating the rotated mailbox server-side (anonymous, no auth).
    grpc_pool: Arc<GrpcClientPool>,
}

impl KeyRollingService {
    /// Create a new KeyRollingService.
    pub fn new(
        token_manager: Arc<TokenManager>,
        dm_manager: Arc<DmConversationManager>,
        group_manager: Arc<GroupMailboxManager>,
        grpc_pool: Arc<GrpcClientPool>,
    ) -> Self {
        Self {
            token_manager,
            dm_manager,
            group_manager,
            grpc_pool,
        }
    }

    /// Check which DM conversations need rotation.
    ///
    /// Returns list of peer IDs whose mailboxes expire within rotation threshold.
    pub fn get_dm_rotation_candidates(&self) -> Result<Vec<UserId>, MailboxError> {
        self.dm_manager
            .get_expiring_mailboxes(DEFAULT_ROTATION_THRESHOLD_HOURS)
    }

    /// Rotate a DM mailbox for a conversation.
    ///
    /// Creates a new mailbox, schedules old one for deletion, and returns
    /// notification that caller should send to peer.
    ///
    /// # Arguments
    /// * `peer_id` - The conversation peer
    ///
    /// # Returns
    /// RotationNotification to send to peer, or error if rotation fails.
    pub async fn rotate_dm_mailbox(
        &self,
        peer_id: &UserId,
    ) -> Result<RotationNotification, MailboxError> {
        // 1. Load current conversation state
        let conversation = self
            .dm_manager
            .load_conversation(peer_id)?
            .ok_or(MailboxError::ConversationNotFound)?;

        let old_mailbox = conversation
            .our_mailbox
            .ok_or(MailboxError::NoMailboxConfigured)?;

        // 2. Atomically claim a fresh token for the new mailbox
        let token = self
            .token_manager
            .claim_available_token()?
            .ok_or(MailboxError::NoTokensAvailable)?;

        // 3. Create the new mailbox server-side using the TOKEN-BOUND keypair.
        //    The blind signature signs the token's public key, so the mailbox key
        //    MUST be the token key (a fresh random key would be rejected) — this is
        //    also exactly what keeps creation unlinkable to the account.
        if token.public_key.len() != 32 || token.private_key.len() != 64 {
            return Err(MailboxError::InvalidServerKey(
                "rotation token key size".to_string(),
            ));
        }
        let mut client = self.grpc_pool.anonymous_mailbox_client();
        let response = client
            .create_ephemeral_mailbox(
                token.public_key.clone(),
                token.signature.clone(),
                crate::mailbox::dm::bootstrap::DEFAULT_MAILBOX_TTL_SECONDS,
                token.randomizer.clone().unwrap_or_default(),
            )
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        // 4. Build the token-bound mailbox (key copied from the token, id from server).
        let mut public_key = [0u8; 32];
        let mut private_key = [0u8; 64];
        public_key.copy_from_slice(&token.public_key);
        private_key.copy_from_slice(&token.private_key);
        let new_mailbox = DmMailbox {
            id: response.mailbox_id.clone(),
            public_key,
            private_key,
            signature: token.signature.clone(),
            randomizer: token.randomizer.clone(),
            token_id: token.id.clone(),
            created_at: Utc::now(),
            expires_at: Utc::now()
                + chrono::Duration::seconds(
                    crate::mailbox::dm::bootstrap::DEFAULT_MAILBOX_TTL_SECONDS as i64,
                ),
            last_polled_at: None,
        };

        // 5. Save the new mailbox ALONGSIDE the old one (schema now allows two rows).
        //    The old mailbox is NOT deleted early — it stays pollable until its own
        //    expiry, giving a loss-free overlap for a peer that hasn't switched yet.
        //    Sends target the newest (load_our_mailbox_internal ORDER BY created_at).
        self.dm_manager.save_our_mailbox(peer_id, &new_mailbox)?;

        // 6. Return notification; the caller sends a DmMailboxRotation to the peer's
        //    CURRENT mailbox so they switch to depositing into the new one.
        Ok(RotationNotification {
            target: RotationTarget::Dm(*peer_id),
            new_mailbox_public_key: public_key,
            old_mailbox_expires_at: old_mailbox.expires_at,
        })
    }

    /// Check which group mailboxes need rotation.
    ///
    /// Returns list of group IDs whose mailboxes expire within rotation threshold.
    pub fn get_group_rotation_candidates(&self) -> Result<Vec<ChannelId>, MailboxError> {
        self.group_manager
            .get_expiring_group_mailboxes(DEFAULT_ROTATION_THRESHOLD_HOURS)
    }

    /// Rotate a group mailbox.
    ///
    /// Similar to DM rotation but returns notification for all group members.
    pub async fn rotate_group_mailbox(
        &self,
        group_id: &ChannelId,
    ) -> Result<RotationNotification, MailboxError> {
        // 1. Load current group mailbox state
        let state = self
            .group_manager
            .load_group_state(group_id)?
            .ok_or(MailboxError::GroupNotFound)?;

        let old_mailbox = state.our_mailbox.ok_or(MailboxError::NoMailboxConfigured)?;

        // 2. Atomically claim a fresh token
        let token = self
            .token_manager
            .claim_available_token()?
            .ok_or(MailboxError::NoTokensAvailable)?;

        // 3. Create the new group mailbox server-side using the TOKEN-BOUND key
        //    (a fresh random key would fail the blind-signature check).
        if token.public_key.len() != 32 || token.private_key.len() != 64 {
            return Err(MailboxError::InvalidServerKey(
                "rotation token key size".to_string(),
            ));
        }
        let mut client = self.grpc_pool.anonymous_mailbox_client();
        let response = client
            .create_ephemeral_mailbox(
                token.public_key.clone(),
                token.signature.clone(),
                crate::mailbox::dm::bootstrap::DEFAULT_MAILBOX_TTL_SECONDS,
                token.randomizer.clone().unwrap_or_default(),
            )
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        // 4. Build the token-bound group mailbox.
        let mut public_key = [0u8; 32];
        let mut private_key = [0u8; 64];
        public_key.copy_from_slice(&token.public_key);
        private_key.copy_from_slice(&token.private_key);
        let new_mailbox = GroupMailbox {
            id: response.mailbox_id.clone(),
            group_id: *group_id,
            public_key,
            private_key,
            signature: token.signature.clone(),
            randomizer: token.randomizer.clone(),
            token_id: token.id.clone(),
            created_at: Utc::now(),
            expires_at: Utc::now()
                + chrono::Duration::seconds(
                    crate::mailbox::dm::bootstrap::DEFAULT_MAILBOX_TTL_SECONDS as i64,
                ),
            last_polled_at: None,
        };

        // 5. Save alongside the old (old expires naturally → loss-free overlap).
        self.group_manager.save_our_mailbox(&new_mailbox)?;

        // 6. Return notification; caller re-announces via GroupMailboxAddress.
        Ok(RotationNotification {
            target: RotationTarget::Group(*group_id),
            new_mailbox_public_key: public_key,
            old_mailbox_expires_at: old_mailbox.expires_at,
        })
    }

    // Old mailboxes are not deleted explicitly: after rotation the old row stays
    // pollable (loss-free overlap for a peer that hasn't switched) until its own
    // expiry, then the server reaps it by TTL and the client's cleanup_expired
    // drops the local row. There is no server Delete RPC and none is needed.

    /// Get all rotation candidates (DM + group).
    pub fn get_all_rotation_candidates(&self) -> Result<Vec<RotationTarget>, MailboxError> {
        let mut candidates = Vec::new();

        for peer_id in self.get_dm_rotation_candidates()? {
            candidates.push(RotationTarget::Dm(peer_id));
        }

        for group_id in self.get_group_rotation_candidates()? {
            candidates.push(RotationTarget::Group(group_id));
        }

        Ok(candidates)
    }

    /// Check if any rotations are needed.
    pub fn has_pending_rotations(&self) -> Result<bool, MailboxError> {
        Ok(!self.get_all_rotation_candidates()?.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rotation_target_dm() {
        let user_id = UserId::new();
        let target = RotationTarget::Dm(user_id);

        match target {
            RotationTarget::Dm(id) => assert_eq!(id, user_id),
            _ => panic!("Expected Dm variant"),
        }
    }

    #[test]
    fn test_rotation_target_group() {
        let channel_id = ChannelId::new();
        let target = RotationTarget::Group(channel_id);

        match target {
            RotationTarget::Group(id) => assert_eq!(id, channel_id),
            _ => panic!("Expected Group variant"),
        }
    }

    #[test]
    fn test_rotation_notification_structure() {
        let user_id = UserId::new();
        let notification = RotationNotification {
            target: RotationTarget::Dm(user_id),
            new_mailbox_public_key: [1u8; 32],
            old_mailbox_expires_at: Utc::now() + Duration::days(7),
        };

        assert_eq!(notification.new_mailbox_public_key, [1u8; 32]);
        assert!(notification.old_mailbox_expires_at > Utc::now());
    }

    #[test]
    fn test_rotation_result_structure() {
        let user_id = UserId::new();
        let result = RotationResult {
            target_id: RotationTarget::Dm(user_id),
            new_mailbox_public_key: [2u8; 32],
            old_mailbox_deletion_at: Utc::now() + Duration::hours(1),
        };

        assert_eq!(result.new_mailbox_public_key, [2u8; 32]);
        assert!(result.old_mailbox_deletion_at > Utc::now());
    }

    #[test]
    fn test_default_constants() {
        assert_eq!(DEFAULT_CHECK_INTERVAL_SECS, 3600);
        assert_eq!(DEFAULT_ROTATION_THRESHOLD_HOURS, 24);
        assert_eq!(DEFAULT_OVERLAP_SECS, 3600);
    }

    #[test]
    fn test_rotation_notification_clone() {
        let user_id = UserId::new();
        let notification = RotationNotification {
            target: RotationTarget::Dm(user_id),
            new_mailbox_public_key: [3u8; 32],
            old_mailbox_expires_at: Utc::now(),
        };

        let cloned = notification.clone();
        assert_eq!(cloned.new_mailbox_public_key, notification.new_mailbox_public_key);
    }

    #[test]
    fn test_rotation_target_clone() {
        let user_id = UserId::new();
        let target = RotationTarget::Dm(user_id);
        let cloned = target.clone();

        match (target, cloned) {
            (RotationTarget::Dm(a), RotationTarget::Dm(b)) => assert_eq!(a, b),
            _ => panic!("Clone mismatch"),
        }
    }
}
