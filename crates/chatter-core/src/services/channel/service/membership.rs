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
        let skdm_opt = match self.signal_service.create_sender_key(channel_id).await {
            Ok(skdm) => {
                tracing::info!(
                    channel_id = %channel_id,
                    skdm_len = skdm.distribution_message.len(),
                    "Created sender key for joined channel"
                );
                Some(skdm)
            }
            Err(e) => {
                // Non-fatal - key will be created on first message
                tracing::warn!(
                    channel_id = %channel_id,
                    error = %e,
                    "Failed to create sender key on join (will create on first message)"
                );
                None
            }
        };

        // Setup group mailbox for encrypted channels (NDSS '21 paper)
        let mailbox_setup = if setup_mailbox {
            let mut mailbox_client = self.mailbox_client();
            match self.setup_group_mailbox_internal(channel_id, &mut mailbox_client).await {
                Ok(Some(mut setup)) => {
                    // Include the SKDM in the result so caller can distribute it
                    setup.skdm = skdm_opt;
                    Some(setup)
                }
                Ok(None) => None,
                Err(e) => {
                    tracing::error!(
                        channel_id = %channel_id,
                        error = %e,
                        "Group mailbox setup failed during channel join - group encryption unavailable"
                    );
                    None
                }
            }
        } else {
            None
        };

        // Sync other channel members for SKDM distribution
        if let Err(e) = self.sync_channel_members_internal(&mut channel_client, channel_id).await {
            tracing::warn!(
                channel_id = %channel_id,
                error = %e,
                "Failed to sync channel members after join (SKDM distribution may fail)"
            );
        }

        Ok((channel, mailbox_setup))
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

        // Fetch members from server (paginate if needed)
        let response = channel_client
            .get_channel_members(&channel_id_str, 100, 0)
            .await?;

        let total_from_server = response.members.len();
        let mut synced_count = 0;
        for proto_member in &response.members {
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
    pub async fn leave_channel(&self, channel_id: &ChannelId) -> Result<(), ChannelError> {
        let current_user = self.settings_repo.current_user_id()?;

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
                    let err_msg = failure
                        .error
                        .map(|e| format!("leave failed: {:?} - {}", e.code, e.message))
                        .unwrap_or_else(|| "leave failed: unknown error".to_string());
                    return Err(ChannelError::InvalidInput(err_msg));
                }
                _ => continue,
            }
        }

        if !left {
            return Err(ChannelError::InvalidInput(
                "no member_removed_id event received".to_string(),
            ));
        }

        let _ = self
            .channel_repo
            .delete_membership(channel_id, &current_user);

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

        Ok(())
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
    /// # Arguments
    ///
    /// * `channel_id` - The channel invitation to respond to
    /// * `accept` - Whether to accept or decline the invitation
    /// * `setup_mailbox` - Whether to setup group mailbox if accepting
    ///
    /// # Group Mailbox Setup
    ///
    /// If `setup_mailbox` is true, accepting an invitation will automatically:
    /// 1. Create an ephemeral group mailbox for receiving messages
    /// 2. The caller should send the returned announcement via
    ///    `MessageService::send_group_mailbox_announcement()`
    pub async fn respond_to_invite(
        &self,
        channel_id: &ChannelId,
        accept: bool,
        setup_mailbox: bool,
    ) -> Result<Option<(crate::domain::channel::Channel, Option<GroupMailboxSetupResult>)>, ChannelError> {
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
                Some(channel_event::Event::MemberAdded(member)) => {
                    if accept && member.user_id == current_user.to_string() {
                        joined = true;
                        responded = true;
                        break;
                    }
                }
                Some(channel_event::Event::InvitationResponse(_response)) => {
                    if !accept {
                        responded = true;
                        break;
                    }
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

        if joined {
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

            // Setup group mailbox for encrypted channels (NDSS '21 paper)
            let mailbox_setup = if setup_mailbox {
                let mut mailbox_client = self.mailbox_client();
                match self.setup_group_mailbox_internal(channel_id, &mut mailbox_client).await {
                    Ok(Some(setup)) => Some(setup),
                    Ok(None) => None,
                    Err(e) => {
                        tracing::error!(
                            channel_id = %channel_id,
                            error = %e,
                            "Group mailbox setup failed during invite accept - group encryption unavailable"
                        );
                        None
                    }
                }
            } else {
                None
            };

            Ok(Some((channel, mailbox_setup)))
        } else {
            Ok(None)
        }
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

    /// Update a member's role.
    pub async fn update_member_role(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
        new_role: MemberRole,
    ) -> Result<Membership, ChannelError> {
        let _ = self.settings_repo.current_user_id()?;

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
}
