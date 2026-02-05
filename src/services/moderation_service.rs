//! Moderation service.
//!
//! Ban, mute, kick, and permission management for channels.

use std::sync::Arc;

use chrono::{Duration, Utc};

use crate::domain::channel::{MemberRole, Permissions};
use crate::domain::ids::{ChannelId, UserId};
use crate::domain::moderation::{Ban, ModerationAction, ModerationLogEntry, Mute};
use crate::error::{NetworkError, StorageError};
use crate::network::grpc::channel_client::ChannelClient;
use crate::proto::chat_v1::{channel_moderation, ChannelModeration};
use crate::storage::repositories::channel_repo::ChannelRepository;
use crate::storage::repositories::settings_repo::{keys, SettingsRepository};

/// Moderation service errors.
#[derive(Debug, thiserror::Error)]
pub enum ModerationError {
    /// Operation requires authentication.
    #[error("not logged in")]
    NotLoggedIn,

    /// User is not authorized for operation.
    #[error("not authorized")]
    NotAuthorized,

    /// Target channel does not exist.
    #[error("channel not found: {0}")]
    ChannelNotFound(ChannelId),

    /// Target user does not exist.
    #[error("user not found: {0}")]
    UserNotFound(UserId),

    /// Input validation failed.
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// Network operation failed.
    #[error("network error: {0}")]
    Network(#[from] NetworkError),

    /// Storage operation failed.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

/// Moderation service for channel moderation operations.
///
/// Provides higher-level moderation operations with permission validation,
/// local state tracking, and event handling.
pub struct ModerationService {
    settings_repo: Arc<SettingsRepository>,
    channel_repo: Arc<ChannelRepository>,
}

impl ModerationService {
    /// Create a new ModerationService with dependencies.
    pub fn new(
        settings_repo: Arc<SettingsRepository>,
        channel_repo: Arc<ChannelRepository>,
    ) -> Self {
        Self {
            settings_repo,
            channel_repo,
        }
    }

    /// Get the currently logged-in user ID.
    fn current_user_id(&self) -> Result<UserId, ModerationError> {
        let user_id_str = self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .map_err(ModerationError::Storage)?
            .ok_or(ModerationError::NotLoggedIn)?;

        user_id_str
            .parse()
            .map_err(|_| ModerationError::InvalidInput("invalid user ID in settings".to_string()))
    }

    /// Kick a user from a channel.
    ///
    /// Delegates to ChannelClient::kick_user and removes local membership.
    ///
    /// # Arguments
    ///
    /// * `channel_client` - Mutable reference to channel client
    /// * `channel_id` - Channel to kick from
    /// * `user_id` - User to kick
    /// * `reason` - Optional reason for the kick
    pub async fn kick_user(
        &self,
        channel_client: &mut ChannelClient,
        channel_id: &ChannelId,
        user_id: &UserId,
        reason: Option<&str>,
    ) -> Result<(), ModerationError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Call server via ChannelClient
        let channel_id_str = channel_id.to_string();
        let user_id_str = user_id.to_string();
        let mut stream = channel_client
            .kick_user(&channel_id_str, &user_id_str, reason)
            .await?;

        // Consume stream, look for member_removed_id event
        use crate::proto::chat_v1::channel_event;
        use tokio_stream::StreamExt;

        let mut kicked = false;
        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            match event.event {
                Some(channel_event::Event::MemberRemovedId(removed_user_id)) => {
                    if removed_user_id == user_id.to_string() {
                        kicked = true;
                        break;
                    }
                }
                Some(channel_event::Event::OperationFailed(failure)) => {
                    let err_msg = failure
                        .error
                        .map(|e| format!("kick failed: {:?} - {}", e.code, e.message))
                        .unwrap_or_else(|| "kick failed: unknown error".to_string());
                    return Err(ModerationError::InvalidInput(err_msg));
                }
                _ => continue,
            }
        }

        if !kicked {
            return Err(ModerationError::InvalidInput(
                "no member_removed_id event received".to_string(),
            ));
        }

        // Remove membership from local storage
        let _ = self.channel_repo.delete_membership(channel_id, user_id);

        Ok(())
    }

    /// Ban a user from a channel.
    ///
    /// Delegates to ChannelClient::ban_user, creates domain::Ban, and logs entry.
    ///
    /// # Arguments
    ///
    /// * `channel_client` - Mutable reference to channel client
    /// * `channel_id` - Channel to ban from
    /// * `user_id` - User to ban
    /// * `duration` - Optional ban duration (None = permanent)
    /// * `reason` - Optional reason for the ban
    ///
    /// # Returns
    ///
    /// The created Ban domain object.
    pub async fn ban_user(
        &self,
        channel_client: &mut ChannelClient,
        channel_id: &ChannelId,
        user_id: &UserId,
        duration: Option<std::time::Duration>,
        reason: Option<&str>,
    ) -> Result<Ban, ModerationError> {
        // Check logged in
        let current_user = self.current_user_id()?;

        // Call server via ChannelClient
        let channel_id_str = channel_id.to_string();
        let user_id_str = user_id.to_string();
        channel_client
            .ban_user(&channel_id_str, &user_id_str, duration, reason)
            .await?;

        // Create domain Ban
        let mut ban = Ban::new(*channel_id, *user_id, current_user);
        if let Some(dur) = duration {
            ban = ban.with_expiry(Duration::from_std(dur).unwrap_or(Duration::days(365)));
        }
        if let Some(r) = reason {
            ban = ban.with_reason(r.to_string());
        }

        // Create log entry (could be stored locally if needed)
        let _log_entry = ModerationLogEntry::for_ban(&ban);

        // Remove membership from local storage for banned user
        let _ = self.channel_repo.delete_membership(channel_id, user_id);

        Ok(ban)
    }

    /// Unban a user from a channel.
    ///
    /// Delegates to ChannelClient::unban_user.
    ///
    /// # Arguments
    ///
    /// * `channel_client` - Mutable reference to channel client
    /// * `channel_id` - Channel to unban from
    /// * `user_id` - User to unban
    pub async fn unban_user(
        &self,
        channel_client: &mut ChannelClient,
        channel_id: &ChannelId,
        user_id: &UserId,
    ) -> Result<(), ModerationError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Call server via ChannelClient
        let channel_id_str = channel_id.to_string();
        let user_id_str = user_id.to_string();
        let mut stream = channel_client
            .unban_user(&channel_id_str, &user_id_str)
            .await?;

        // Consume stream for completion
        use crate::proto::chat_v1::channel_event;
        use tokio_stream::StreamExt;

        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            if let Some(channel_event::Event::OperationFailed(failure)) = event.event {
                let err_msg = failure
                    .error
                    .map(|e| format!("unban failed: {:?} - {}", e.code, e.message))
                    .unwrap_or_else(|| "unban failed: unknown error".to_string());
                return Err(ModerationError::InvalidInput(err_msg));
            }
        }

        // No local storage update needed - user can rejoin if they want
        Ok(())
    }

    /// Mute a user in a channel.
    ///
    /// Since there's no dedicated mute RPC, this updates local membership state
    /// to track the mute. The server sends mute events via ChannelModeration.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel to mute in
    /// * `user_id` - User to mute
    /// * `duration` - Mute duration (required, unlike permanent bans)
    /// * `reason` - Optional reason for the mute
    ///
    /// # Returns
    ///
    /// The created Mute domain object.
    pub fn mute_user(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
        duration: std::time::Duration,
        reason: Option<&str>,
    ) -> Result<Mute, ModerationError> {
        // Check logged in
        let current_user = self.current_user_id()?;

        // Get existing membership
        let mut membership = self
            .channel_repo
            .get_membership(channel_id, user_id)?
            .ok_or_else(|| ModerationError::UserNotFound(*user_id))?;

        // Update membership with mute state
        let chrono_duration = Duration::from_std(duration).unwrap_or(Duration::hours(1));
        membership.is_muted = true;
        membership.muted_until = Some(Utc::now() + chrono_duration);

        // Save updated membership
        self.channel_repo.update_membership(&membership)?;

        // Create domain Mute
        let mut mute = Mute::new(
            *channel_id,
            *user_id,
            current_user,
            chrono_duration,
        );
        if let Some(r) = reason {
            mute = mute.with_reason(r.to_string());
        }

        // Create log entry (could be stored locally if needed)
        let _log_entry = ModerationLogEntry::for_mute(&mute);

        Ok(mute)
    }

    /// Unmute a user in a channel.
    ///
    /// Clears the is_muted flag on local membership.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel to unmute in
    /// * `user_id` - User to unmute
    pub fn unmute_user(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
    ) -> Result<(), ModerationError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Get existing membership
        let mut membership = self
            .channel_repo
            .get_membership(channel_id, user_id)?
            .ok_or_else(|| ModerationError::UserNotFound(*user_id))?;

        // Clear mute state
        membership.is_muted = false;
        membership.muted_until = None;

        // Save updated membership
        self.channel_repo.update_membership(&membership)?;

        Ok(())
    }

    /// Get permissions for a user in a channel.
    ///
    /// Looks up the membership role and returns the corresponding permissions.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel to check permissions for
    /// * `user_id` - User to get permissions for
    pub fn get_user_permissions(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
    ) -> Result<Permissions, ModerationError> {
        // Get membership for user
        let membership = self
            .channel_repo
            .get_membership(channel_id, user_id)?
            .ok_or_else(|| ModerationError::UserNotFound(*user_id))?;

        // Return permissions based on role
        Ok(Permissions::for_role(membership.role))
    }

    /// Check if a user can moderate another user.
    ///
    /// Moderator can only moderate users with lower roles.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel to check in
    /// * `user_id` - User attempting to moderate
    /// * `target_user_id` - User being moderated
    ///
    /// # Returns
    ///
    /// True if user_id has a higher role than target_user_id and can moderate.
    pub fn can_moderate(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
        target_user_id: &UserId,
    ) -> Result<bool, ModerationError> {
        // Get moderator's membership
        let moderator_membership = self
            .channel_repo
            .get_membership(channel_id, user_id)?
            .ok_or_else(|| ModerationError::UserNotFound(*user_id))?;

        // Get target's membership
        let target_membership = self
            .channel_repo
            .get_membership(channel_id, target_user_id)?
            .ok_or_else(|| ModerationError::UserNotFound(*target_user_id))?;

        // Convert roles to numeric values for comparison
        let moderator_level = role_level(moderator_membership.role);
        let target_level = role_level(target_membership.role);

        // Can moderate if moderator has higher level and at least Moderator role
        Ok(moderator_level > target_level && moderator_level >= role_level(MemberRole::Moderator))
    }
}

/// Convert MemberRole to numeric level for comparison.
fn role_level(role: MemberRole) -> u8 {
    match role {
        MemberRole::Member => 0,
        MemberRole::Moderator => 1,
        MemberRole::Admin => 2,
        MemberRole::Owner => 3,
    }
}

/// Convert proto ChannelModeration action to domain ModerationAction.
pub fn proto_moderation_action_to_domain(action: channel_moderation::Action) -> ModerationAction {
    match action {
        channel_moderation::Action::Kick => ModerationAction::Kick,
        channel_moderation::Action::Ban => ModerationAction::Ban,
        channel_moderation::Action::Mute => ModerationAction::Mute,
        channel_moderation::Action::Unban | channel_moderation::Action::Unmute => {
            // These are reversals, not standalone actions - default to Kick
            ModerationAction::Kick
        }
        channel_moderation::Action::Unspecified => ModerationAction::Warn,
    }
}

impl ModerationService {
    /// Log a moderation action.
    ///
    /// Creates a ModerationLogEntry that could be stored locally if needed.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel where the action took place
    /// * `target_user_id` - User who was the target of the action
    /// * `action` - Type of moderation action
    /// * `reason` - Optional reason for the action
    ///
    /// # Returns
    ///
    /// The created ModerationLogEntry.
    pub fn log_action(
        &self,
        channel_id: &ChannelId,
        target_user_id: &UserId,
        action: ModerationAction,
        reason: Option<&str>,
    ) -> Result<ModerationLogEntry, ModerationError> {
        let current_user = self.current_user_id()?;

        let mut entry = ModerationLogEntry::new(
            *channel_id,
            *target_user_id,
            current_user,
            action,
        );

        if let Some(r) = reason {
            entry = entry.with_reason(r.to_string());
        }

        // Could store locally in a moderation log table if needed
        // For now, just return the entry
        Ok(entry)
    }

    /// Handle a moderation event from the server.
    ///
    /// Converts the proto ChannelModeration to domain action and updates local state.
    ///
    /// # Arguments
    ///
    /// * `event` - The ChannelModeration event from the server
    pub fn handle_moderation_event(&self, event: &ChannelModeration) -> Result<(), ModerationError> {
        // Parse channel_id
        let channel_id: ChannelId = event
            .channel_id
            .parse()
            .map_err(|_| ModerationError::InvalidInput("invalid channel ID".to_string()))?;

        // Parse user_id (target of moderation)
        let user_id: UserId = event
            .user_id
            .parse()
            .map_err(|_| ModerationError::InvalidInput("invalid user ID".to_string()))?;

        // Parse action
        let action = channel_moderation::Action::try_from(event.action)
            .unwrap_or(channel_moderation::Action::Unspecified);

        match action {
            channel_moderation::Action::Kick => {
                // Remove membership from local storage
                let _ = self.channel_repo.delete_membership(&channel_id, &user_id);
            }
            channel_moderation::Action::Ban => {
                // Remove membership from local storage (banned users are no longer members)
                let _ = self.channel_repo.delete_membership(&channel_id, &user_id);
            }
            channel_moderation::Action::Mute => {
                // Update local membership with mute state
                if let Ok(Some(mut membership)) =
                    self.channel_repo.get_membership(&channel_id, &user_id)
                {
                    membership.is_muted = true;
                    // Set muted_until from event duration if provided
                    if let Some(duration) = &event.duration {
                        let chrono_duration = Duration::seconds(duration.seconds)
                            + Duration::nanoseconds(duration.nanos as i64);
                        membership.muted_until = Some(Utc::now() + chrono_duration);
                    }
                    let _ = self.channel_repo.update_membership(&membership);
                }
            }
            channel_moderation::Action::Unban => {
                // No action needed - user would need to rejoin
            }
            channel_moderation::Action::Unmute => {
                // Clear mute state on local membership
                if let Ok(Some(mut membership)) =
                    self.channel_repo.get_membership(&channel_id, &user_id)
                {
                    membership.is_muted = false;
                    membership.muted_until = None;
                    let _ = self.channel_repo.update_membership(&membership);
                }
            }
            channel_moderation::Action::Unspecified => {
                // Unknown action, ignore
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proto_action_conversion() {
        assert_eq!(
            proto_moderation_action_to_domain(channel_moderation::Action::Kick),
            ModerationAction::Kick
        );
        assert_eq!(
            proto_moderation_action_to_domain(channel_moderation::Action::Ban),
            ModerationAction::Ban
        );
        assert_eq!(
            proto_moderation_action_to_domain(channel_moderation::Action::Mute),
            ModerationAction::Mute
        );
        // Unban/Unmute are reversals, not standalone actions
        assert_eq!(
            proto_moderation_action_to_domain(channel_moderation::Action::Unban),
            ModerationAction::Kick // Default fallback
        );
        assert_eq!(
            proto_moderation_action_to_domain(channel_moderation::Action::Unspecified),
            ModerationAction::Warn
        );
    }

    #[test]
    fn test_permissions_for_role() {
        // Member permissions
        let member_perms = Permissions::for_role(MemberRole::Member);
        assert!(member_perms.can_send_messages);
        assert!(member_perms.can_edit_own_messages);
        assert!(!member_perms.can_delete_any_message);
        assert!(!member_perms.can_manage_members);

        // Moderator permissions
        let mod_perms = Permissions::for_role(MemberRole::Moderator);
        assert!(mod_perms.can_send_messages);
        assert!(mod_perms.can_delete_any_message);
        assert!(mod_perms.can_manage_members);
        assert!(!mod_perms.can_manage_roles);

        // Admin permissions
        let admin_perms = Permissions::for_role(MemberRole::Admin);
        assert!(admin_perms.can_manage_roles);
        assert!(admin_perms.can_edit_channel);
        assert!(admin_perms.can_delete_channel);

        // Owner permissions (same as admin)
        let owner_perms = Permissions::for_role(MemberRole::Owner);
        assert_eq!(owner_perms, admin_perms);
    }

    #[test]
    fn test_role_level() {
        assert_eq!(role_level(MemberRole::Member), 0);
        assert_eq!(role_level(MemberRole::Moderator), 1);
        assert_eq!(role_level(MemberRole::Admin), 2);
        assert_eq!(role_level(MemberRole::Owner), 3);

        // Verify ordering
        assert!(role_level(MemberRole::Owner) > role_level(MemberRole::Admin));
        assert!(role_level(MemberRole::Admin) > role_level(MemberRole::Moderator));
        assert!(role_level(MemberRole::Moderator) > role_level(MemberRole::Member));
    }

    #[test]
    fn test_moderation_error_display() {
        let err = ModerationError::NotLoggedIn;
        assert!(err.to_string().contains("not logged in"));

        let err = ModerationError::NotAuthorized;
        assert!(err.to_string().contains("not authorized"));

        let channel_id = ChannelId::new();
        let err = ModerationError::ChannelNotFound(channel_id);
        assert!(err.to_string().contains("channel not found"));

        let user_id = UserId::new();
        let err = ModerationError::UserNotFound(user_id);
        assert!(err.to_string().contains("user not found"));

        let err = ModerationError::InvalidInput("bad input".to_string());
        assert!(err.to_string().contains("bad input"));
    }
}
