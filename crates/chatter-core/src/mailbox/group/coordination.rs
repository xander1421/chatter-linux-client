//! Group mailbox coordination.
//!
//! Handles member address exchange for existing group mailboxes.

use std::sync::Arc;

use chrono::{Duration, Utc};

use crate::domain::{ChannelId, UserId};
use crate::error::MailboxError;
use crate::mailbox::group::{GroupMailboxManager, MemberMailbox};
use crate::proto::content_v1::GroupMailboxAddress;

/// Default group mailbox TTL: 48 hours in seconds. Matches the DM TTL so the
/// shared rotation threshold (12h) gives the same ~36h epoch / 12h overlap.
pub const DEFAULT_GROUP_MAILBOX_TTL_SECONDS: u64 = 48 * 60 * 60;

/// Group mailbox coordinator.
///
/// Handles:
/// - Processing received mailbox announcements from other members
pub struct GroupCoordinator {
    /// Group mailbox state persistence.
    mailbox_manager: Arc<GroupMailboxManager>,
}

impl GroupCoordinator {
    /// Create a new GroupCoordinator.
    ///
    /// # Arguments
    ///
    /// * `mailbox_manager` - Manager for group mailbox state
    pub fn new(mailbox_manager: Arc<GroupMailboxManager>) -> Self {
        Self { mailbox_manager }
    }

    /// Get the underlying mailbox manager.
    pub fn mailbox_manager(&self) -> &GroupMailboxManager {
        &self.mailbox_manager
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
        let channel_id: ChannelId = announcement.channel_id.parse().map_err(|_| {
            MailboxError::InvalidServerKey("Invalid channel_id in announcement".into())
        })?;

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
            "Stored member mailbox from SKDM ephemeral_reply_to (NDSS '21)"
        );

        Ok(())
    }

    /// Check if we have a mailbox set up for a channel.
    pub fn has_mailbox(&self, channel_id: &ChannelId) -> Result<bool, MailboxError> {
        let state = self.mailbox_manager.load_group_state(channel_id)?;
        Ok(state.map(|s| s.our_mailbox.is_some()).unwrap_or(false))
    }

    /// Clean up all group mailbox state when leaving a channel.
    ///
    /// Removes our mailbox and all member mailbox records for this channel.
    pub fn cleanup_channel(&self, channel_id: &ChannelId) -> Result<(), MailboxError> {
        tracing::info!("Cleaning up group mailbox state");
        self.mailbox_manager.delete_group_state(channel_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Database;
    use crate::util::time::TimestampNanos;

    #[test]
    fn test_default_group_mailbox_ttl() {
        // 7 days in seconds
        assert_eq!(DEFAULT_GROUP_MAILBOX_TTL_SECONDS, 48 * 60 * 60);
        assert_eq!(DEFAULT_GROUP_MAILBOX_TTL_SECONDS, 172800);
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

    #[test]
    fn test_process_announcement_overwrites_member_mailbox() {
        // Option A rotation inbound: a member re-announcing a rotated
        // GroupMailboxAddress overwrites their stored address (UNIQUE(group_id,
        // user_id) → INSERT OR REPLACE), switching our send target to their new
        // mailbox. This is what makes GroupMailboxAddress double as the rotation
        // announcement — no new proto needed.
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("coordination_test.db");
        let db = Arc::new(Database::open(&db_path, "testpass").unwrap());
        db.run_migrations().unwrap();

        let mgr = Arc::new(GroupMailboxManager::new(db.clone()));
        let coordinator = GroupCoordinator::new(mgr.clone());

        let channel_id = ChannelId::new();
        let member = UserId::new();

        // First announcement: member's original mailbox.
        let first = GroupMailboxAddress {
            channel_id: channel_id.to_string(),
            mailbox_pk: vec![1u8; 32],
            timestamp: TimestampNanos::now().as_u64(),
        };
        coordinator.process_announcement(member, &first).unwrap();

        let state = mgr.load_group_state(&channel_id).unwrap().unwrap();
        assert_eq!(
            state.member_mailboxes.get(&member).unwrap().public_key,
            [1u8; 32]
        );

        // Rotated announcement: same member, new mailbox pk → must overwrite.
        let rotated = GroupMailboxAddress {
            channel_id: channel_id.to_string(),
            mailbox_pk: vec![2u8; 32],
            timestamp: TimestampNanos::now().as_u64(),
        };
        coordinator.process_announcement(member, &rotated).unwrap();

        let state = mgr.load_group_state(&channel_id).unwrap().unwrap();
        assert_eq!(
            state.member_mailboxes.len(),
            1,
            "must overwrite, not duplicate"
        );
        assert_eq!(
            state.member_mailboxes.get(&member).unwrap().public_key,
            [2u8; 32],
            "send target must switch to the rotated mailbox"
        );
    }
}
