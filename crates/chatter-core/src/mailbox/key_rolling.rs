//! Key rolling.
//!
//! Ephemeral key rotation and expiration for mailbox forward secrecy.
//!
//! Periodically rotates mailbox keys so that old mailboxes are eventually forgotten
//! by the server, limiting the window during which the server can correlate message
//! patterns.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use tokio::sync::RwLock;

use crate::domain::{ChannelId, UserId};
use crate::error::MailboxError;
use crate::mailbox::dm::{DmConversationManager, DmMailbox};
use crate::mailbox::group::{GroupMailbox, GroupMailboxManager};
use crate::mailbox::TokenManager;

/// Default rotation check interval (1 hour).
pub const DEFAULT_CHECK_INTERVAL_SECS: u64 = 3600;

/// Default threshold for rotation (24 hours before expiry).
pub const DEFAULT_ROTATION_THRESHOLD_HOURS: i64 = 24;

/// Default overlap period for old mailboxes (1 hour).
/// Old mailbox stays active briefly for in-flight messages.
pub const DEFAULT_OVERLAP_SECS: u64 = 3600;

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

/// Tracks old mailboxes pending deletion.
struct PendingDeletion {
    mailbox_id: String,
    delete_at: DateTime<Utc>,
}

/// Key rolling service for mailbox forward secrecy.
///
/// Periodically rotates mailbox keys to limit the window during which
/// the server can correlate message patterns.
pub struct KeyRollingService {
    token_manager: Arc<TokenManager>,
    dm_manager: Arc<DmConversationManager>,
    group_manager: Arc<GroupMailboxManager>,
    /// Pending old mailboxes to be deleted after overlap period.
    pending_deletions: RwLock<Vec<PendingDeletion>>,
    /// Overlap period for old mailboxes.
    overlap_period: Duration,
}

impl KeyRollingService {
    /// Create a new KeyRollingService.
    pub fn new(
        token_manager: Arc<TokenManager>,
        dm_manager: Arc<DmConversationManager>,
        group_manager: Arc<GroupMailboxManager>,
    ) -> Self {
        Self {
            token_manager,
            dm_manager,
            group_manager,
            pending_deletions: RwLock::new(Vec::new()),
            overlap_period: Duration::seconds(DEFAULT_OVERLAP_SECS as i64),
        }
    }

    /// Create with custom thresholds.
    pub fn with_thresholds(
        token_manager: Arc<TokenManager>,
        dm_manager: Arc<DmConversationManager>,
        group_manager: Arc<GroupMailboxManager>,
        overlap_secs: u64,
    ) -> Self {
        Self {
            token_manager,
            dm_manager,
            group_manager,
            pending_deletions: RwLock::new(Vec::new()),
            overlap_period: Duration::seconds(overlap_secs as i64),
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

        // 3. Generate new Ed25519 keypair
        let mut secret_bytes = [0u8; 32];
        {use rand::TryRngCore; rand::rngs::OsRng.try_fill_bytes(&mut secret_bytes).expect("OS CSPRNG must not fail");}
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&secret_bytes);
        let verifying_key = signing_key.verifying_key();

        let public_key = verifying_key.to_bytes();
        let private_key = {
            let mut combined = [0u8; 64];
            combined[..32].copy_from_slice(&secret_bytes);
            combined[32..].copy_from_slice(&public_key);
            combined
        };

        // 4. Create new mailbox (server creation happens via MailboxClient - prepare here)
        let new_mailbox = DmMailbox {
            id: uuid::Uuid::new_v4().to_string(),
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

        // 5. Save new mailbox
        self.dm_manager.save_our_mailbox(peer_id, &new_mailbox)?;

        // 7. Schedule old mailbox for deletion after overlap period
        let delete_at = Utc::now() + self.overlap_period;
        {
            let mut deletions = self.pending_deletions.write().await;
            deletions.push(PendingDeletion {
                mailbox_id: old_mailbox.id.clone(),
                delete_at,
            });
        }

        // 8. Return notification for caller to send to peer
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

        // 3. Generate new Ed25519 keypair
        let mut secret_bytes = [0u8; 32];
        {use rand::TryRngCore; rand::rngs::OsRng.try_fill_bytes(&mut secret_bytes).expect("OS CSPRNG must not fail");}
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&secret_bytes);
        let verifying_key = signing_key.verifying_key();

        let public_key = verifying_key.to_bytes();
        let private_key = {
            let mut combined = [0u8; 64];
            combined[..32].copy_from_slice(&secret_bytes);
            combined[32..].copy_from_slice(&public_key);
            combined
        };

        // 4. Create new group mailbox
        let new_mailbox = GroupMailbox {
            id: uuid::Uuid::new_v4().to_string(),
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

        // 5. Save new mailbox
        self.group_manager.save_our_mailbox(&new_mailbox)?;

        // 6. Schedule old mailbox deletion
        let delete_at = Utc::now() + self.overlap_period;
        {
            let mut deletions = self.pending_deletions.write().await;
            deletions.push(PendingDeletion {
                mailbox_id: old_mailbox.id.clone(),
                delete_at,
            });
        }

        // 8. Return notification for all group members
        Ok(RotationNotification {
            target: RotationTarget::Group(*group_id),
            new_mailbox_public_key: public_key,
            old_mailbox_expires_at: old_mailbox.expires_at,
        })
    }

    /// Process pending deletions whose overlap period has elapsed.
    ///
    /// Call periodically (e.g., every hour) to clean up old mailboxes.
    pub async fn process_pending_deletions(&self) -> Result<u32, MailboxError> {
        let now = Utc::now();
        let mut deleted_count = 0u32;

        let mut deletions = self.pending_deletions.write().await;

        // Partition into ready and not-ready
        let (ready, not_ready): (Vec<_>, Vec<_>) =
            deletions.drain(..).partition(|d| d.delete_at <= now);

        // Put back the not-ready ones
        *deletions = not_ready;

        drop(deletions); // Release lock before logging

        // Delete ready ones (in practice, these are already replaced in DB)
        // The old mailbox records should be cleaned up
        for pending in ready {
            // Old mailboxes are replaced by new ones, so just log
            // Actual deletion from server happens when server TTL expires
            deleted_count += 1;
            tracing::debug!(mailbox_id = %pending.mailbox_id, "Old mailbox overlap period elapsed");
        }

        Ok(deleted_count)
    }

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
