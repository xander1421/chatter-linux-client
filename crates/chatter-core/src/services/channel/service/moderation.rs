//! Channel moderation operations.
//!
//! Kick and ban are signed `RemoveMember` changes; unban, mute and unmute
//! stay unsigned server actions (they do not change who holds keys).

use tokio_stream::StreamExt;

use crate::domain::ids::{ChannelId, UserId};
use crate::proto::chat_v1::{self as pb, channel_event};
use crate::proto::content_v1::SenderKeyDistribution;
use crate::services::channel::membership_log::AuthorBase;
use crate::services::current_user::CurrentUserProvider;

use crate::services::channel::error::ChannelError;

use super::membership::{action, drain};
use super::ChannelService;
use pb::channel_action::Action;
use pb::remove_member::Reason;

impl ChannelService {
    /// Kick a user from a channel.
    ///
    /// Returns the rotated SKDM which MUST be distributed to remaining members
    /// to maintain forward secrecy. Sending is blocked first; the caller MUST
    /// distribute the SKDM and then `signal_service.unblock_channel_send()`.
    pub async fn kick_user(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
        reason: Option<&str>,
    ) -> Result<SenderKeyDistribution, ChannelError> {
        self.remove_member(channel_id, user_id, Reason::Kicked, reason).await
    }

    /// Ban a user from a channel, permanently until unbanned. Like kick, the
    /// sender key is rotated and the caller distributes the returned SKDM.
    pub async fn ban_user(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
        reason: Option<&str>,
    ) -> Result<SenderKeyDistribution, ChannelError> {
        self.remove_member(channel_id, user_id, Reason::Banned, reason).await
    }

    /// Signed `RemoveMember{KICKED|BANNED}`, then rotate. The rotation is
    /// only done once our VERIFIED state no longer lists the target — that
    /// set, not the server roster, is who the new key goes to.
    async fn remove_member(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
        why: Reason,
        reason: Option<&str>,
    ) -> Result<SenderKeyDistribution, ChannelError> {
        let _ = self.settings_repo.current_user_id()?;
        if let Err(e) = self.signal_service.block_channel_send(channel_id) {
            tracing::error!(%channel_id, error = %e, "Failed to block channel send before removal");
        }
        let target = user_id.to_string();
        let cid = channel_id.to_string();
        let reason = reason.map(str::to_string);
        let removed = self
            .membership_log
            .author(
                channel_id,
                AuthorBase::Verified,
                None,
                |_| Ok(vec![action(Action::RemoveMember(pb::RemoveMember { user_id: target.clone(), reason: why as i32 }))]),
                |change| {
                    let mut client = self.channel_client();
                    let (cid, reason) = (cid.clone(), reason.clone());
                    async move {
                        if why == Reason::Banned {
                            client.ban_user(&cid, change, reason.as_deref()).await
                        } else {
                            drain(client.kick_user(&cid, change, reason.as_deref()).await?, "kick").await
                        }
                    }
                },
            )
            .await;
        if let Err(e) = removed {
            // Nobody was removed: nothing to rotate.
            let _ = self.signal_service.unblock_channel_send(channel_id);
            return Err(e.into());
        }

        // Our accepted change is committed by `author`; if that failed, catch
        // up before trusting the distribution set.
        let still_listed = |log: &crate::services::channel::membership_log::MembershipLog| {
            log.load(channel_id).ok().flatten().is_some_and(|v| v.state.members.contains_key(&target))
        };
        if still_listed(&self.membership_log) {
            if let Err(e) = self.membership_log.catch_up(channel_id).await {
                tracing::warn!(%channel_id, error = %e, "Catch-up after removal failed");
            }
        }
        if still_listed(&self.membership_log) {
            let _ = self.signal_service.mark_channel_for_rotation(channel_id);
            return Err(ChannelError::SenderKeyRotationFailed(format!(
                "{user_id} is still in the verified membership — not rotating to a set that includes them"
            )));
        }

        let skdm = self.signal_service.rotate_sender_key(channel_id).await.map_err(|e| {
            tracing::error!(%channel_id, removed = %user_id, error = %e, "CRITICAL: Sender key rotation failed after removal — sending remains blocked");
            let _ = self.signal_service.mark_channel_for_rotation(channel_id);
            ChannelError::SenderKeyRotationFailed(format!(
                "rotation failed after removing {user_id}: {e} — message sending blocked until resolved"
            ))
        })?;
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
