//! Channel membership operations.
//!
//! Join, leave, invite, and member management methods for ChannelService.

use chrono::Utc;
use tokio_stream::StreamExt;

use crate::domain::channel::{MemberRole, Membership, NotificationPreference};
use crate::domain::ids::{ChannelId, UserId};
use crate::mailbox::group::GroupMailboxSetupResult;
use crate::network::grpc::channel_client::ChannelClient;
use crate::proto::chat_v1::{self, channel_event};
use crate::services::current_user::CurrentUserProvider;

use crate::services::channel::converters::{domain_role_to_proto, proto_to_domain_membership};
use crate::services::channel::error::ChannelError;

use super::ChannelService;

impl ChannelService {
    /// Join a channel.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - The channel to join
    /// * `password` - Optional password for private channels
    /// * `setup_mailbox` - Whether to setup group mailbox for encrypted channels
    ///
    /// # Group Mailbox Setup
    ///
    /// If `setup_mailbox` is true and the channel requires encryption,
    /// this method will automatically:
    /// 1. Create an ephemeral group mailbox for receiving messages
    /// 2. The caller should send the returned announcement via
    ///    `MessageService::send_group_mailbox_announcement()`
    pub async fn join_channel(
        &self,
        channel_id: &ChannelId,
        password: Option<&str>,
        setup_mailbox: bool,
    ) -> Result<(crate::domain::channel::Channel, Option<GroupMailboxSetupResult>), ChannelError> {
        let current_user = self.settings_repo.current_user_id()?;

        let channel_id_str = channel_id.to_string();
        let mut channel_client = self.channel_client();
        let mut stream = channel_client
            .join_channel(&channel_id_str, password, false)
            .await?;

        let mut joined = false;
        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            match event.event {
                Some(channel_event::Event::MemberAdded(member)) => {
                    if member.user_id == current_user.to_string() {
                        joined = true;
                        break;
                    }
                }
                Some(channel_event::Event::OperationFailed(failure)) => {
                    let err_msg = failure
                        .error
                        .map(|e| format!("join failed: {:?} - {}", e.code, e.message))
                        .unwrap_or_else(|| "join failed: unknown error".to_string());
                    return Err(ChannelError::InvalidInput(err_msg));
                }
                _ => continue,
            }
        }

        if !joined {
            return Err(ChannelError::InvalidInput(
                "no member_added event received".to_string(),
            ));
        }

        drop(stream);

        let channel = match self.channel_repo.get(channel_id)? {
            Some(ch) => ch,
            None => self.get_channel_internal(&mut channel_client, channel_id).await?,
        };

        let membership = Membership {
            channel_id: *channel_id,
            user_id: current_user,
            role: MemberRole::Member,
            joined_at: Utc::now(),
            last_read_message_id: None,
            is_muted: false,
            muted_until: None,
            notification_preference: NotificationPreference::All,
        };
        self.channel_repo.insert_membership(&membership)?;

        // Create and prepare our sender key for this channel.
        // The SKDM must be distributed to all channel members via Double Ratchet.
        // This is mandatory — without it, other members cannot decrypt our messages.
        let skdm = self.signal_service.create_sender_key(channel_id).await
            .map_err(|e| {
                tracing::error!(
                    channel_id = %channel_id,
                    error = %e,
                    "Failed to create sender key on join — group encryption unavailable"
                );
                ChannelError::SenderKeyRotationFailed(format!(
                    "failed to create sender key on join: {}", e
                ))
            })?;

        tracing::info!(
            channel_id = %channel_id,
            skdm_len = skdm.distribution_message.len(),
            "Created sender key for joined channel"
        );

        // Server-managed channels no longer use per-group mailboxes — control
        // + SKDM fanout ride the device inbox directly. The `setup_mailbox`
        // toggle is retained only so call sites stay source-compatible.
        let _ = setup_mailbox;

        // Sync other channel members so the upcoming SKDM distribution hits
        // every member (SKDM distribute_skdm reads the local membership list).
        if let Err(e) = self.sync_channel_members_internal(&mut channel_client, channel_id).await {
            tracing::warn!(
                channel_id = %channel_id,
                error = %e,
                "Failed to sync channel members after join (SKDM distribution may fail)"
            );
        }

        // Distribute our freshly minted SKDM via pairwise sealed-sender to
        // every member's device inbox. Best-effort: peers that miss this
        // blast will recover via SenderKeyRequest on the first decrypt miss.
        if let Err(e) = self.message_service.distribute_skdm(channel_id, &skdm).await {
            tracing::warn!(
                channel_id = %channel_id,
                error = %e,
                "Failed to distribute SKDM on join (peers will heal via SenderKeyRequest)"
            );
        }

        Ok((channel, None))
    }

    /// Sync channel members from the server to local storage.
    ///
    /// Fetches all channel members and stores their memberships locally.
    /// This is essential for SKDM distribution which needs to know all members.
    pub async fn sync_channel_members(&self, channel_id: &ChannelId) -> Result<usize, ChannelError> {
        let mut channel_client = self.channel_client();
        self.sync_channel_members_internal(&mut channel_client, channel_id).await
    }

    /// Internal: Sync channel members using an existing client.
    pub(super) async fn sync_channel_members_internal(
        &self,
        channel_client: &mut ChannelClient,
        channel_id: &ChannelId,
    ) -> Result<usize, ChannelError> {
        let channel_id_str = channel_id.to_string();

        // Full membership, paginated. This sync is what SKDM distribution
        // reads, so a single-page fetch here meant members past the server's
        // page size never appeared locally and never received sender keys —
        // the "(paginate if needed)" comment that used to sit here described
        // an intention, not the code (OPEN-ITEMS §16).
        let proto_members = channel_client
            .get_all_channel_members(&channel_id_str)
            .await?;

        let total_from_server = proto_members.len();
        let mut synced_count = 0;
        for proto_member in &proto_members {
            // Convert proto to domain membership
            match proto_to_domain_membership(channel_id, proto_member) {
                Ok(membership) => {
                    // Insert or update membership
                    if let Err(e) = self.channel_repo.insert_membership(&membership) {
                        tracing::debug!(
                            channel_id = %channel_id,
                            user_id = %membership.user_id,
                            error = %e,
                            "Failed to save membership (may already exist)"
                        );
                    } else {
                        synced_count += 1;
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        channel_id = %channel_id,
                        error = %e,
                        "Failed to convert proto membership"
                    );
                }
            }
        }

        tracing::info!(
            channel_id = %channel_id,
            synced_count = synced_count,
            total_from_server = total_from_server,
            "Synced channel members for SKDM distribution"
        );

        Ok(synced_count)
    }

    /// Leave a channel.
    ///
    /// Before leaving, rotates sender key and returns the SKDM so the caller
    /// can distribute it to remaining members. This matches Signal's behavior:
    /// sender keys MUST be rotated on ANY member departure (voluntary or forced)
    /// to maintain forward secrecy.
    ///
    /// The caller MUST:
    /// 1. Distribute the returned SKDM to remaining members
    /// 2. Then call `signal_service.unblock_channel_send()` — though since we're
    ///    leaving, remaining members handle unblocking on their side.
    pub async fn leave_channel(
        &self,
        channel_id: &ChannelId,
    ) -> Result<crate::proto::content_v1::SenderKeyDistribution, ChannelError> {
        let current_user = self.settings_repo.current_user_id()?;

        // Block message sending BEFORE leave to prevent encryption with old key.
        // The leaving user still has the old key — any message encrypted with it
        // after departure would violate forward secrecy for remaining members.
        if let Err(e) = self.signal_service.block_channel_send(channel_id) {
            tracing::error!(
                channel_id = %channel_id,
                error = %e,
                "Failed to block channel send before leave"
            );
        }

        // Rotate sender key BEFORE leaving (we're still a member at this point).
        // This gives remaining members a fresh key via SKDM distribution.
        let skdm = self.signal_service.rotate_sender_key(channel_id).await
            .map_err(|e| {
                tracing::error!(
                    channel_id = %channel_id,
                    error = %e,
                    "CRITICAL: Sender key rotation failed before leave — sending remains blocked"
                );
                let _ = self.signal_service.mark_channel_for_rotation(channel_id);
                ChannelError::SenderKeyRotationFailed(format!(
                    "rotation failed before leaving: {} — message sending blocked until resolved", e
                ))
            })?;

        tracing::info!(
            channel_id = %channel_id,
            "Rotated sender key before leave — caller MUST distribute SKDM"
        );

        // Now perform the actual leave
        let channel_id_str = channel_id.to_string();
        let mut channel_client = self.channel_client();
        let mut stream = channel_client.leave_channel(&channel_id_str).await?;

        let mut left = false;
        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            match event.event {
                Some(channel_event::Event::MemberRemovedId(removed_user_id)) => {
                    if removed_user_id == current_user.to_string() {
                        left = true;
                        break;
                    }
                }
                Some(channel_event::Event::OperationFailed(failure)) => {
                    // Leave failed AFTER we already rotated the sender key. Do
                    // NOT unblock sending — the rotated key has not been
                    // distributed to remaining members, so any message we
                    // encrypted with it would be undecryptable. Mark the
                    // channel for rotation so the next successful leave/retry
                    // redistributes fresh key material to peers.
                    let _ = self.signal_service.mark_channel_for_rotation(channel_id);
                    let err_msg = failure
                        .error
                        .map(|e| format!("leave failed: {:?} - {}", e.code, e.message))
                        .unwrap_or_else(|| "leave failed: unknown error".to_string());
                    tracing::error!(
                        channel_id = %channel_id,
                        error = %err_msg,
                        "Leave failed after pre-leave key rotation — channel remains blocked, marked for re-rotation"
                    );
                    return Err(ChannelError::InvalidInput(err_msg));
                }
                _ => continue,
            }
        }

        if !left {
            // Same rationale as above: rotated key is not distributed, keep
            // channel blocked and flagged for rotation.
            let _ = self.signal_service.mark_channel_for_rotation(channel_id);
            tracing::error!(
                channel_id = %channel_id,
                "Leave stream ended without MemberRemovedId after key rotation — channel remains blocked, marked for re-rotation"
            );
            return Err(ChannelError::InvalidInput(
                "no member_removed_id event received".to_string(),
            ));
        }

        let _ = self
            .channel_repo
            .delete_membership(channel_id, &current_user);

        // Clear our local sender keys (we no longer need them)
        if let Err(e) = self.signal_service.clear_channel_sender_keys(channel_id).await {
            tracing::warn!(
                channel_id = %channel_id,
                error = %e,
                "Failed to clear sender keys when leaving channel"
            );
        }

        // Clean up group mailbox state (NDSS '21 ephemeral mailboxes)
        if let Err(e) = self.group_coordinator.cleanup_channel(channel_id) {
            tracing::warn!(
                channel_id = %channel_id,
                error = %e,
                "Failed to clean up group mailbox when leaving channel"
            );
        }

        Ok(skdm)
    }

    /// Invite a user to a channel.
    pub async fn invite_user(
        &self,
        channel_id: &ChannelId,
        invitee_id: &UserId,
        message: Option<&str>,
    ) -> Result<(), ChannelError> {
        let _ = self.settings_repo.current_user_id()?;

        let channel_id_str = channel_id.to_string();
        let invitee_id_str = invitee_id.to_string();
        let mut channel_client = self.channel_client();
        let mut stream = channel_client
            .invite_user(&channel_id_str, &invitee_id_str, message)
            .await?;

        let mut invited = false;
        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            match event.event {
                Some(channel_event::Event::InvitationSent(_invite)) => {
                    invited = true;
                    break;
                }
                Some(channel_event::Event::OperationFailed(failure)) => {
                    let err_msg = failure
                        .error
                        .map(|e| format!("invite failed: {:?} - {}", e.code, e.message))
                        .unwrap_or_else(|| "invite failed: unknown error".to_string());
                    return Err(ChannelError::InvalidInput(err_msg));
                }
                _ => continue,
            }
        }

        if !invited {
            return Err(ChannelError::InvalidInput(
                "no invitation_sent event received".to_string(),
            ));
        }

        Ok(())
    }

    /// Respond to a channel invitation.
    ///
    /// On accept, returns the channel and leaves this device in the same local
    /// state a join does: the channel row upserted and our membership row
    /// written. On decline, returns `None`.
    ///
    /// # Which event ends the stream
    ///
    /// The server's `RespondToInvite` emits `OperationStarted` and then an
    /// accepted/declined payload, which the handler maps to
    /// `InvitationResponse` (`internal/services/channel/handlers/invitation.go`).
    /// It never sends `MemberAdded` — waiting for one, as this method used to,
    /// made every accept run the stream to exhaustion and return
    /// "no response event received" *after* the server had already made the
    /// user a member (OPEN-ITEMS §70).
    pub async fn respond_to_invite(
        &self,
        channel_id: &ChannelId,
        accept: bool,
    ) -> Result<Option<crate::domain::channel::Channel>, ChannelError> {
        let current_user = self.settings_repo.current_user_id()?;

        let channel_id_str = channel_id.to_string();
        let mut channel_client = self.channel_client();
        let mut stream = channel_client
            .respond_to_invite(&channel_id_str, accept)
            .await?;

        let mut responded = false;
        let mut joined = false;
        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            match event.event {
                Some(channel_event::Event::InvitationResponse(response)) => {
                    responded = true;
                    joined = accept && response.accepted;
                    break;
                }
                Some(channel_event::Event::OperationFailed(failure)) => {
                    let err_msg = failure
                        .error
                        .map(|e| format!("respond to invite failed: {:?} - {}", e.code, e.message))
                        .unwrap_or_else(|| "respond to invite failed: unknown error".to_string());
                    return Err(ChannelError::InvalidInput(err_msg));
                }
                _ => continue,
            }
        }

        if !responded {
            return Err(ChannelError::InvalidInput(
                "no response event received".to_string(),
            ));
        }

        drop(stream);

        if !joined {
            return Ok(None);
        }

        // Accepting an invitation IS a join as far as this device is
        // concerned, so the local rows a join writes get written here too —
        // otherwise the user is a member on the server with nothing in their
        // own store, and the channel is missing from the sidebar until the
        // next sync (OPEN-ITEMS §69).
        let channel = self.get_channel_internal(&mut channel_client, channel_id).await?;

        let membership = Membership {
            channel_id: *channel_id,
            user_id: current_user,
            role: MemberRole::Member,
            joined_at: Utc::now(),
            last_read_message_id: None,
            is_muted: false,
            muted_until: None,
            notification_preference: NotificationPreference::All,
        };
        self.channel_repo.insert_membership(&membership)?;

        Ok(Some(channel))
    }

    /// Get channel members with pagination.
    pub async fn get_channel_members(
        &self,
        channel_id: &ChannelId,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<Membership>, ChannelError> {
        let _ = self.settings_repo.current_user_id()?;

        let channel_id_str = channel_id.to_string();
        let mut channel_client = self.channel_client();
        let response = channel_client
            .get_channel_members(&channel_id_str, limit as i32, offset as i32)
            .await?;

        let mut memberships = Vec::with_capacity(response.members.len());
        for proto_member in &response.members {
            let membership = proto_to_domain_membership(channel_id, proto_member)?;

            match self
                .channel_repo
                .get_membership(channel_id, &membership.user_id)?
            {
                Some(_) => self.channel_repo.update_membership(&membership)?,
                None => self.channel_repo.insert_membership(&membership)?,
            }

            memberships.push(membership);
        }

        Ok(memberships)
    }

    /// Get EVERY channel member, paginating past the server page size.
    ///
    /// The single-page `get_channel_members(_, 100, 0)` pattern silently
    /// truncated at 100 in every caller whose answer had to be "everyone" —
    /// group-call key distribution and rotation left members past the first
    /// page unable to decrypt the call, and SKDM membership sync never learned
    /// they existed (OPEN-ITEMS §16). Use the paged method above only for UI
    /// that genuinely wants one page.
    pub async fn get_all_channel_members(
        &self,
        channel_id: &ChannelId,
    ) -> Result<Vec<Membership>, ChannelError> {
        let _ = self.settings_repo.current_user_id()?;

        let mut channel_client = self.channel_client();
        let proto_members = channel_client
            .get_all_channel_members(&channel_id.to_string())
            .await?;

        let mut memberships = Vec::with_capacity(proto_members.len());
        for proto_member in &proto_members {
            let membership = proto_to_domain_membership(channel_id, proto_member)?;
            match self
                .channel_repo
                .get_membership(channel_id, &membership.user_id)?
            {
                Some(_) => self.channel_repo.update_membership(&membership)?,
                None => self.channel_repo.insert_membership(&membership)?,
            }
            memberships.push(membership);
        }

        Ok(memberships)
    }

    /// Update a member's role.
    pub async fn update_member_role(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
        new_role: MemberRole,
    ) -> Result<Membership, ChannelError> {
        let _ = self.settings_repo.current_user_id()?;
        // Ownership is not grantable; the server refuses it too (members.go).
        if new_role == MemberRole::Owner {
            return Err(ChannelError::InvalidInput("ownership cannot be granted".to_string()));
        }

        let proto_role = domain_role_to_proto(new_role);

        let channel_id_str = channel_id.to_string();
        let user_id_str = user_id.to_string();
        let mut channel_client = self.channel_client();
        let mut stream = channel_client
            .update_member_role(&channel_id_str, &user_id_str, proto_role)
            .await?;

        let mut updated_member: Option<chat_v1::channel::ChannelMember> = None;
        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            match event.event {
                Some(channel_event::Event::MemberUpdated(member)) => {
                    if member.user_id == user_id.to_string() {
                        updated_member = Some(member);
                        break;
                    }
                }
                Some(channel_event::Event::OperationFailed(failure)) => {
                    let err_msg = failure
                        .error
                        .map(|e| format!("update role failed: {:?} - {}", e.code, e.message))
                        .unwrap_or_else(|| "update role failed: unknown error".to_string());
                    return Err(ChannelError::InvalidInput(err_msg));
                }
                _ => continue,
            }
        }

        let proto_member = updated_member.ok_or_else(|| {
            ChannelError::InvalidInput("no member_updated event received".to_string())
        })?;

        let membership = proto_to_domain_membership(channel_id, &proto_member)?;

        match self
            .channel_repo
            .get_membership(channel_id, &membership.user_id)?
        {
            Some(_) => self.channel_repo.update_membership(&membership)?,
            None => self.channel_repo.insert_membership(&membership)?,
        }

        Ok(membership)
    }

    /// Full leave orchestration: block sending → rotate sender key → server
    /// leave → distribute final SKDM to remaining members → unblock sending
    /// → purge local sender keys for this channel. Wraps `leave_channel` so
    /// the SDK façade isn't stitching four services.
    ///
    /// SKDM distribution failure is best-effort: remaining members will
    /// rotate themselves via `mark_channel_for_rotation` on their next send.
    pub async fn leave_channel_complete(
        &self,
        channel_id: &ChannelId,
    ) -> Result<(), ChannelError> {
        let skdm = self.leave_channel(channel_id).await?;

        if let Err(e) = self.message_service.distribute_skdm(channel_id, &skdm).await {
            tracing::warn!(
                channel_id = %channel_id,
                error = %e,
                "Failed to distribute SKDM on leave — remaining members will rotate on next send"
            );
        }

        if let Err(e) = self.signal_service.unblock_channel_send(channel_id) {
            tracing::warn!(channel_id = %channel_id, error = %e, "Failed to unblock channel send after leave");
        }

        // Drop every sender key for this channel — we're no longer a member.
        if let Err(e) = self.signal_service.clear_channel_sender_keys(channel_id).await {
            tracing::warn!(
                channel_id = %channel_id,
                error = %e,
                "Failed to purge sender keys after leaving channel"
            );
        }

        Ok(())
    }
}
