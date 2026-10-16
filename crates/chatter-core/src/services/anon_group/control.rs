//! Blocking, muting, and rate limiting for anonymous groups.

use crate::domain::UserId;
use crate::domain::anon_group::{AnonGroupId, AnonGroupStatus};

use super::{AnonGroupError, AnonGroupService};

impl AnonGroupService {
    fn check_group_not_removed(&self, group_id: &AnonGroupId) -> Result<(), AnonGroupError> {
        let group = self
            .anon_group_repo
            .get_group(group_id)?
            .ok_or(AnonGroupError::GroupNotFound(*group_id))?;
        if group.status == AnonGroupStatus::Removed {
            return Err(AnonGroupError::GroupBlocked(*group_id));
        }
        Ok(())
    }

    /// Default rate limit: 60 messages per minute per group.
    const RATE_LIMIT_MESSAGES: u32 = 60;
    /// Default rate limit window: 1 minute.
    const RATE_LIMIT_WINDOW: chrono::Duration = chrono::Duration::minutes(1);

    /// Block a group (ignore all messages, stop polling mailbox).
    pub fn block_group(&self, group_id: &AnonGroupId) -> Result<(), AnonGroupError> {
        self.check_group_not_removed(group_id)?;
        self.anon_group_repo
            .update_status(group_id, AnonGroupStatus::Blocked)?;
        self.held_posts.forget(group_id);
        tracing::info!("Blocked group");
        Ok(())
    }

    /// Unblock a group (resume normal operation).
    pub fn unblock_group(&self, group_id: &AnonGroupId) -> Result<(), AnonGroupError> {
        self.check_group_not_removed(group_id)?;
        self.anon_group_repo
            .update_status(group_id, AnonGroupStatus::Active)?;
        tracing::info!("Unblocked group");
        Ok(())
    }

    /// Mute a group (receive messages but no notifications).
    pub fn mute_group(&self, group_id: &AnonGroupId) -> Result<(), AnonGroupError> {
        self.check_group_not_removed(group_id)?;
        self.anon_group_repo
            .update_status(group_id, AnonGroupStatus::Muted)?;
        tracing::info!("Muted group");
        Ok(())
    }

    /// Unmute a group (resume notifications).
    pub fn unmute_group(&self, group_id: &AnonGroupId) -> Result<(), AnonGroupError> {
        self.check_group_not_removed(group_id)?;
        self.anon_group_repo
            .update_status(group_id, AnonGroupStatus::Active)?;
        tracing::info!("Unmuted group");
        Ok(())
    }

    /// Check if a group is blocked.
    pub fn is_group_blocked(&self, group_id: &AnonGroupId) -> Result<bool, AnonGroupError> {
        match self.anon_group_repo.get_group(group_id)? {
            Some(group) => Ok(group.is_blocked()),
            None => Err(AnonGroupError::GroupNotFound(*group_id)),
        }
    }

    /// Block a user within a group (ignore their messages).
    pub fn block_user_in_group(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
        reason: Option<&str>,
    ) -> Result<(), AnonGroupError> {
        // Verify group exists
        let _ = self
            .anon_group_repo
            .get_group(group_id)?
            .ok_or(AnonGroupError::GroupNotFound(*group_id))?;

        self.anon_group_repo.block_user(group_id, user_id, reason)?;
        tracing::info!("Blocked user in group");
        Ok(())
    }

    /// Unblock a user within a group.
    pub fn unblock_user_in_group(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
    ) -> Result<(), AnonGroupError> {
        self.anon_group_repo.unblock_user(group_id, user_id)?;
        tracing::info!("Unblocked user in group");
        Ok(())
    }

    /// Check if a user is blocked within a group.
    pub fn is_user_blocked_in_group(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
    ) -> Result<bool, AnonGroupError> {
        Ok(self.anon_group_repo.is_user_blocked(group_id, user_id)?)
    }

    /// Get all blocked users in a group.
    pub fn get_blocked_users(&self, group_id: &AnonGroupId) -> Result<Vec<UserId>, AnonGroupError> {
        Ok(self.anon_group_repo.get_blocked_users(group_id)?)
    }

    /// Check if sending a message would exceed rate limits.
    ///
    /// Returns Ok(()) if allowed, Err if rate limited.
    pub fn check_rate_limit(&self, group_id: &AnonGroupId) -> Result<(), AnonGroupError> {
        let count = self
            .anon_group_repo
            .increment_rate_limit(group_id, Self::RATE_LIMIT_WINDOW)?;

        if count > Self::RATE_LIMIT_MESSAGES {
            tracing::warn!(
                "Rate limit exceeded for group: {} messages in window",
                count
            );
            return Err(AnonGroupError::RateLimitExceeded(Self::RATE_LIMIT_MESSAGES));
        }

        Ok(())
    }

    /// Check if the group is blocked (cannot send/receive).
    pub fn check_group_not_blocked(&self, group_id: &AnonGroupId) -> Result<(), AnonGroupError> {
        let group = self
            .anon_group_repo
            .get_group(group_id)?
            .ok_or(AnonGroupError::GroupNotFound(*group_id))?;

        if !group.status.can_send() {
            return Err(AnonGroupError::GroupBlocked(*group_id));
        }

        Ok(())
    }
}
