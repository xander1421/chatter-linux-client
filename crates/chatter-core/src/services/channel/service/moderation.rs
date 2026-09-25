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

        // Remove local membership BEFORE rotating/distributing. If this fails
        // with anything other than NotFound (membership already absent), bail
        // out — otherwise `distribute_skdm` would fan the rotated sender key
        // back to the kicked user via the stale local membership row.
        match self.channel_repo.delete_membership(channel_id, user_id) {
            Ok(()) => {}
            Err(crate::error::StorageError::NotFound(_)) => {}
            Err(e) => {
                tracing::error!(
                    channel_id = %channel_id,
                    kicked_user = %user_id,
                    error = %e,
                    "CRITICAL: Failed to remove local membership after kick — aborting before sender-key rotation to prevent leaking the new key to the removed user"
                );
                return Err(ChannelError::Storage(e));
            }
        }

        // Defense in depth: verify the kicked user is no longer listed locally
        // before we rotate + distribute the new sender key.
        if let Ok(members) = self.channel_repo.list_members(channel_id) {
            if members.iter().any(|m| &m.user_id == user_id) {
                tracing::error!(
                    channel_id = %channel_id,
                    kicked_user = %user_id,
                    "CRITICAL: kicked user still present in local memberships after delete — aborting rotation"
                );
                return Err(ChannelError::InvalidInput(format!(
                    "local membership for kicked user {} still present",
                    user_id
                )));
            }
        }

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

    /// Ban a user from a channel, permanently until unbanned.
    ///
    /// Like kick, banning removes a member and MUST rotate the sender key
    /// to maintain forward secrecy. The caller MUST distribute the returned
    /// SKDM and then call `signal_service.unblock_channel_send()`.
    pub async fn ban_user(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
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
            .ban_user(&channel_id_str, &user_id_str, reason)
            .await
        {
            let _ = self.signal_service.unblock_channel_send(channel_id);
            return Err(e.into());
        }

        // Same membership-removal guard as kick_user — otherwise distribute_skdm
        // would leak the rotated sender key back to the banned user.
        match self.channel_repo.delete_membership(channel_id, user_id) {
            Ok(()) => {}
            Err(crate::error::StorageError::NotFound(_)) => {}
            Err(e) => {
                tracing::error!(
                    channel_id = %channel_id,
                    banned_user = %user_id,
                    error = %e,
                    "CRITICAL: Failed to remove local membership after ban — aborting before sender-key rotation"
                );
                return Err(ChannelError::Storage(e));
            }
        }
        if let Ok(members) = self.channel_repo.list_members(channel_id) {
            if members.iter().any(|m| &m.user_id == user_id) {
                return Err(ChannelError::InvalidInput(format!(
                    "local membership for banned user {} still present",
                    user_id
                )));
            }
        }

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

    /// Full kick orchestration: server kick → rotate sender key → distribute
    /// new SKDM → unblock sending. Wraps the bare `kick_user` so the SDK
    /// façade doesn't have to stitch three services together.
    ///
    /// Returns `SenderKeyRotationFailed` if SKDM distribution fails — in that
    /// case the channel stays blocked for sending and manual retry is needed.
    pub async fn kick_user_complete(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
        reason: Option<&str>,
    ) -> Result<(), ChannelError> {
        let skdm = self.kick_user(channel_id, user_id, reason).await?;

        if let Err(e) = self.message_service.distribute_skdm(channel_id, &skdm).await {
            tracing::error!(
                channel_id = %channel_id,
                error = %e,
                "Failed to distribute SKDM after kick — sending remains blocked"
            );
            return Err(ChannelError::SenderKeyRotationFailed(
                format!("SKDM distribution failed after kick: {e}"),
            ));
        }

        if let Err(e) = self.signal_service.unblock_channel_send(channel_id) {
            tracing::warn!(
                channel_id = %channel_id,
                error = %e,
                "Failed to unblock channel send after kick"
            );
        }

        Ok(())
    }

    /// Mute a member for `duration`, or until unmuted when `None`. The server
    /// enforces it (SpiceDB `send_message` excludes `muted`, so their sends are
    /// refused) and lifts a timed mute when it lapses; the local membership
    /// flag only mirrors it so the member list can mark them.
    pub async fn mute_user(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
        duration: Option<std::time::Duration>,
    ) -> Result<(), ChannelError> {
        let _ = self.settings_repo.current_user_id()?;
        self.channel_client()
            .mute_user(&channel_id.to_string(), &user_id.to_string(), duration)
            .await?;
        self.mirror_member_muted(channel_id, user_id, true)
    }

    /// Lift a mute set by [`Self::mute_user`].
    pub async fn unmute_user(&self, channel_id: &ChannelId, user_id: &UserId) -> Result<(), ChannelError> {
        let _ = self.settings_repo.current_user_id()?;
        self.channel_client()
            .unmute_user(&channel_id.to_string(), &user_id.to_string())
            .await?;
        self.mirror_member_muted(channel_id, user_id, false)
    }

    /// Mirror a server-confirmed mute onto the local membership row, if this
    /// device has one (the next member fetch refreshes it anyway).
    fn mirror_member_muted(&self, channel_id: &ChannelId, user_id: &UserId, muted: bool) -> Result<(), ChannelError> {
        if let Some(mut membership) = self.channel_repo.get_membership(channel_id, user_id)? {
            membership.is_muted = muted;
            membership.muted_until = None;
            self.channel_repo
                .update_membership(&membership)
                .map_err(ChannelError::Storage)?;
        }
        Ok(())
    }

    /// A channel's active bans, newest first, or `None` when our role may
    /// not see them. Owner/admin only — the same roles that can unban, and
    /// the server refuses anyone else. Display fields are left empty for the
    /// caller to resolve.
    pub async fn list_banned_users(
        &self,
        channel_id: &ChannelId,
    ) -> Result<Option<Vec<crate::domain::channel::BannedMember>>, ChannelError> {
        let me = self.settings_repo.current_user_id()?;
        let may_unban = self
            .channel_repo
            .get_membership(channel_id, &me)?
            .is_some_and(|m| m.role.can_unban());
        if !may_unban {
            return Ok(None);
        }
        let bans = self.channel_client().list_banned_users(&channel_id.to_string()).await?;
        Ok(Some(
            bans.into_iter()
                .filter_map(|b| {
                    let user_id = b.user_id.parse::<uuid::Uuid>().ok()?;
                    Some(crate::domain::channel::BannedMember {
                        user_id: UserId::from(user_id),
                        banned_at: chrono::DateTime::from_timestamp_nanos(b.banned_at as i64),
                        display_name: String::new(),
                        avatar_bytes: None,
                    })
                })
                .collect(),
        ))
    }

    /// Ban + distribute the rotated sender key + unblock sending. Same
    /// orchestration as `kick_user_complete` so the client façade doesn't
    /// have to stitch three services together.
    pub async fn ban_user_complete(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
        reason: Option<&str>,
    ) -> Result<(), ChannelError> {
        let skdm = self.ban_user(channel_id, user_id, reason).await?;

        if let Err(e) = self.message_service.distribute_skdm(channel_id, &skdm).await {
            tracing::error!(
                channel_id = %channel_id,
                error = %e,
                "Failed to distribute SKDM after ban — sending remains blocked"
            );
            return Err(ChannelError::SenderKeyRotationFailed(
                format!("SKDM distribution failed after ban: {e}"),
            ));
        }

        if let Err(e) = self.signal_service.unblock_channel_send(channel_id) {
            tracing::warn!(
                channel_id = %channel_id,
                error = %e,
                "Failed to unblock channel send after ban"
            );
        }

        Ok(())
    }
}
