//! Channel moderation operations.
//!
//! Kick, ban, and unban methods for ChannelService.

use tokio_stream::StreamExt;

use crate::domain::ids::{ChannelId, UserId};
use crate::proto::chat_v1::channel_event;
use crate::proto::content_v1::SenderKeyDistribution;
use crate::services::current_user::CurrentUserProvider;

use crate::services::channel::error::ChannelError;

use super::ChannelService;

impl ChannelService {
    /// Kick a user from a channel.
    ///
    /// Returns the rotated SKDM which MUST be distributed to remaining members
    /// to maintain forward secrecy. The departed user won't be able to decrypt
    /// future messages encrypted with the new sender key.
    ///
    /// # Atomicity
    ///
    /// Message sending is blocked on this channel BEFORE the kick to prevent
    /// any messages from being encrypted with the old key. The caller MUST
    /// distribute the returned SKDM and then call
    /// `signal_service.unblock_channel_send()` to resume message sending.
    pub async fn kick_user(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
        reason: Option<&str>,
    ) -> Result<SenderKeyDistribution, ChannelError> {
        let _ = self.settings_repo.current_user_id()?;

        // Block message sending BEFORE kick to prevent encryption with old key.
        // The kicked user still has the old key — any message encrypted with it
        // would violate forward secrecy.
        if let Err(e) = self.signal_service.block_channel_send(channel_id) {
            tracing::error!(
                channel_id = %channel_id,
                error = %e,
                "Failed to block channel send before kick"
            );
        }

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
                    // Kick failed — unblock sending since no member was removed
                    let _ = self.signal_service.unblock_channel_send(channel_id);
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
            let _ = self.signal_service.unblock_channel_send(channel_id);
            return Err(ChannelError::InvalidInput(
                "no member_removed_id event received".to_string(),
            ));
        }

        let _ = self.channel_repo.delete_membership(channel_id, user_id);

        // Rotate sender key — this is MANDATORY after member removal.
        // If rotation fails, sending stays blocked to prevent forward secrecy violation.
        let skdm = self.signal_service.rotate_sender_key(channel_id).await
            .map_err(|e| {
                tracing::error!(
                    channel_id = %channel_id,
                    kicked_user = %user_id,
                    error = %e,
                    "CRITICAL: Sender key rotation failed after kick — sending remains blocked"
                );
                // Mark for rotation so it will be forced on next unblock attempt
                let _ = self.signal_service.mark_channel_for_rotation(channel_id);
                ChannelError::SenderKeyRotationFailed(format!(
                    "rotation failed after kicking {}: {} — message sending blocked until resolved",
                    user_id, e
                ))
            })?;

        tracing::info!(
            channel_id = %channel_id,
            kicked_user = %user_id,
            "Rotated sender key after kick — caller MUST distribute SKDM then unblock"
        );

        Ok(skdm)
    }

    /// Ban a user from a channel.
    ///
    /// Like kick, banning removes a member and MUST rotate the sender key
    /// to maintain forward secrecy. The caller MUST distribute the returned
    /// SKDM and then call `signal_service.unblock_channel_send()`.
    pub async fn ban_user(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
        duration: Option<std::time::Duration>,
        reason: Option<&str>,
    ) -> Result<SenderKeyDistribution, ChannelError> {
        let _ = self.settings_repo.current_user_id()?;

        // Block message sending BEFORE ban (same rationale as kick)
        if let Err(e) = self.signal_service.block_channel_send(channel_id) {
            tracing::error!(
                channel_id = %channel_id,
                error = %e,
                "Failed to block channel send before ban"
            );
        }

        let channel_id_str = channel_id.to_string();
        let user_id_str = user_id.to_string();
        let mut channel_client = self.channel_client();
        if let Err(e) = channel_client
            .ban_user(&channel_id_str, &user_id_str, duration, reason)
            .await
        {
            let _ = self.signal_service.unblock_channel_send(channel_id);
            return Err(e.into());
        }

        let _ = self.channel_repo.delete_membership(channel_id, user_id);

        // Rotate sender key — mandatory after member removal
        let skdm = self.signal_service.rotate_sender_key(channel_id).await
            .map_err(|e| {
                tracing::error!(
                    channel_id = %channel_id,
                    banned_user = %user_id,
                    error = %e,
                    "CRITICAL: Sender key rotation failed after ban — sending remains blocked"
                );
                let _ = self.signal_service.mark_channel_for_rotation(channel_id);
                ChannelError::SenderKeyRotationFailed(format!(
                    "rotation failed after banning {}: {} — message sending blocked until resolved",
                    user_id, e
                ))
            })?;

        tracing::info!(
            channel_id = %channel_id,
            banned_user = %user_id,
            "Rotated sender key after ban — caller MUST distribute SKDM then unblock"
        );

        Ok(skdm)
    }

    /// Unban a user from a channel.
    pub async fn unban_user(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
    ) -> Result<(), ChannelError> {
        let _ = self.settings_repo.current_user_id()?;

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
