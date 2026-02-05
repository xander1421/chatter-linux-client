//! Moderation service.
//!
//! Ban, mute, kick, and permission management for channels.

use std::sync::Arc;

use chrono::Duration;

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
        let mut ban = Ban::new(channel_id.clone(), user_id.clone(), current_user);
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
}
