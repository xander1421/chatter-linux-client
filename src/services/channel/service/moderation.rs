//! Channel moderation operations.
//!
//! Kick, ban, and unban methods for ChannelService.

use tokio_stream::StreamExt;

use crate::domain::ids::{ChannelId, UserId};
use crate::proto::chat_v1::channel_event;
use crate::proto::content_v1::SenderKeyDistribution;

use crate::services::channel::error::ChannelError;

use super::ChannelService;

impl ChannelService {
    /// Kick a user from a channel.
    ///
    /// Returns the rotated SKDM which MUST be distributed to remaining members
    /// to maintain forward secrecy. The departed user won't be able to decrypt
    /// future messages encrypted with the new sender key.
    pub async fn kick_user(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
        reason: Option<&str>,
    ) -> Result<Option<SenderKeyDistribution>, ChannelError> {
        let _ = self.current_user_id()?;

        let channel_id_str = channel_id.to_string();
        let user_id_str = user_id.to_string();
        let mut channel_client = self.channel_client();
        let mut stream = channel_client
            .kick_user(&channel_id_str, &user_id_str, reason)
            .await?;

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
                    return Err(ChannelError::InvalidInput(err_msg));
                }
                _ => continue,
            }
        }

        if !kicked {
            return Err(ChannelError::InvalidInput(
                "no member_removed_id event received".to_string(),
            ));
        }

        let _ = self.channel_repo.delete_membership(channel_id, user_id);

        // Rotate sender key - departed user won't have the new key
        let skdm = match self.signal_service.rotate_sender_key(channel_id).await {
            Ok(skdm) => {
                tracing::info!(
                    channel_id = %channel_id,
                    kicked_user = %user_id,
                    "Rotated sender key after kicking user - SKDM must be distributed"
                );
                Some(skdm)
            }
            Err(e) => {
                tracing::warn!(
                    channel_id = %channel_id,
                    error = %e,
                    "Failed to rotate sender key after kick (will rotate on next message)"
                );
                None
            }
        };

        Ok(skdm)
    }

    /// Ban a user from a channel.
    pub async fn ban_user(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
        duration: Option<std::time::Duration>,
        reason: Option<&str>,
    ) -> Result<(), ChannelError> {
        let _ = self.current_user_id()?;

        let channel_id_str = channel_id.to_string();
        let user_id_str = user_id.to_string();
        let mut channel_client = self.channel_client();
        channel_client
            .ban_user(&channel_id_str, &user_id_str, duration, reason)
            .await?;

        let _ = self.channel_repo.delete_membership(channel_id, user_id);

        Ok(())
    }

    /// Unban a user from a channel.
    pub async fn unban_user(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
    ) -> Result<(), ChannelError> {
        let _ = self.current_user_id()?;

        let channel_id_str = channel_id.to_string();
        let user_id_str = user_id.to_string();
        let mut channel_client = self.channel_client();
        let mut stream = channel_client
            .unban_user(&channel_id_str, &user_id_str)
            .await?;

        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            if let Some(channel_event::Event::OperationFailed(failure)) = event.event {
                let err_msg = failure
                    .error
                    .map(|e| format!("unban failed: {:?} - {}", e.code, e.message))
                    .unwrap_or_else(|| "unban failed: unknown error".to_string());
                return Err(ChannelError::InvalidInput(err_msg));
            }
        }

        Ok(())
    }
}
