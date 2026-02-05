//! Channel service.
//!
//! Create, get, update, delete, and list channels.

use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use tokio_stream::StreamExt;

use crate::domain::channel::{
    Channel, ChannelConfig, ChannelType, MemberRole, Membership, MessagePersistence,
    NotificationPreference,
};
use crate::domain::ids::{ChannelId, UserId};
use crate::error::{NetworkError, StorageError};
use crate::network::grpc::channel_client::ChannelClient;
use crate::proto::chat_v1::{self, channel_event, ChannelRole};
use crate::storage::repositories::channel_repo::ChannelRepository;
use crate::storage::repositories::settings_repo::{keys, SettingsRepository};

/// Channel service errors.
#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    /// Operation requires authentication.
    #[error("not logged in")]
    NotLoggedIn,

    /// Target channel does not exist.
    #[error("channel not found: {0}")]
    ChannelNotFound(ChannelId),

    /// User is not authorized for operation.
    #[error("not authorized")]
    NotAuthorized,

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

/// Channel service for all channel operations.
///
/// Orchestrates channel creation, retrieval, updates, and deletion by
/// coordinating network client and local storage.
pub struct ChannelService {
    settings_repo: Arc<SettingsRepository>,
    channel_repo: Arc<ChannelRepository>,
}

impl ChannelService {
    /// Create a new ChannelService with dependencies.
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
    fn current_user_id(&self) -> Result<UserId, ChannelError> {
        let user_id_str = self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .map_err(ChannelError::Storage)?
            .ok_or(ChannelError::NotLoggedIn)?;

        user_id_str
            .parse()
            .map_err(|_| ChannelError::InvalidInput("invalid user ID in settings".to_string()))
    }

    /// Create a new channel.
    ///
    /// # Arguments
    ///
    /// * `channel_client` - Mutable reference to channel client
    /// * `name` - Channel name
    /// * `channel_type` - Type of channel (Public, Private, DirectMessage)
    /// * `config` - Optional channel configuration
    /// * `description` - Optional channel description
    /// * `image_url` - Optional channel image URL
    /// * `password` - Optional password for private channels
    /// * `initial_members` - Initial member user IDs
    #[allow(clippy::too_many_arguments)]
    pub async fn create_channel(
        &self,
        channel_client: &mut ChannelClient,
        name: &str,
        channel_type: ChannelType,
        config: Option<ChannelConfig>,
        description: Option<&str>,
        image_url: Option<&str>,
        password: Option<&str>,
        initial_members: Vec<UserId>,
    ) -> Result<Channel, ChannelError> {
        // Check logged in
        let current_user = self.current_user_id()?;

        // Validate name
        let trimmed_name = name.trim();
        if trimmed_name.is_empty() {
            return Err(ChannelError::InvalidInput(
                "channel name cannot be empty".to_string(),
            ));
        }

        // Convert to proto types
        let proto_type = domain_channel_type_to_proto(channel_type);
        let proto_config = config.as_ref().map(domain_config_to_proto);

        // Call server (returns stream)
        let mut stream = channel_client
            .create_channel(
                trimmed_name,
                proto_type,
                proto_config,
                initial_members.iter().map(|id| id.to_string()).collect(),
                password,
                description,
                image_url,
            )
            .await?;

        // Consume stream to find ChannelCreated event
        let mut channel_opt: Option<Channel> = None;
        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            if let Some(channel_event::Event::ChannelCreated(proto_channel)) = event.event {
                channel_opt = Some(proto_to_domain_channel(&proto_channel)?);
                break;
            }
        }

        let channel = channel_opt.ok_or_else(|| {
            ChannelError::InvalidInput("no channel created event received".to_string())
        })?;

        // Store in local DB
        self.channel_repo.insert(&channel)?;

        // Create membership for current user as Owner
        let membership = Membership {
            channel_id: channel.id.clone(),
            user_id: current_user,
            role: MemberRole::Owner,
            joined_at: channel.created_at,
            last_read_message_id: None,
            is_muted: false,
            muted_until: None,
            notification_preference: NotificationPreference::All,
        };
        self.channel_repo.insert_membership(&membership)?;

        Ok(channel)
    }

    /// Get a channel by ID.
    ///
    /// Fetches from server and upserts to local storage.
    ///
    /// # Arguments
    ///
    /// * `channel_client` - Mutable reference to channel client
    /// * `channel_id` - ID of channel to fetch
    pub async fn get_channel(
        &self,
        channel_client: &mut ChannelClient,
        channel_id: &ChannelId,
    ) -> Result<Channel, ChannelError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Call server (unary, returns proto Channel directly)
        let proto_channel = channel_client
            .get_channel(&channel_id.to_string())
            .await?;

        // Convert to domain
        let channel = proto_to_domain_channel(&proto_channel)?;

        // Upsert to local storage (insert if missing, update if exists)
        match self.channel_repo.get(channel_id)? {
            Some(_) => self.channel_repo.update(&channel)?,
            None => self.channel_repo.insert(&channel)?,
        }

        Ok(channel)
    }

    /// Update a channel.
    ///
    /// # Arguments
    ///
    /// * `channel_client` - Mutable reference to channel client
    /// * `channel_id` - ID of channel to update
    /// * `name` - Optional new name
    /// * `description` - Optional new description
    /// * `image_url` - Optional new image URL
    /// * `config` - Optional new configuration
    #[allow(clippy::too_many_arguments)]
    pub async fn update_channel(
        &self,
        channel_client: &mut ChannelClient,
        channel_id: &ChannelId,
        name: Option<&str>,
        description: Option<&str>,
        image_url: Option<&str>,
        config: Option<ChannelConfig>,
    ) -> Result<Channel, ChannelError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Get existing channel from local DB (need current state for update)
        let existing = self
            .channel_repo
            .get(channel_id)?
            .ok_or_else(|| ChannelError::ChannelNotFound(channel_id.clone()))?;

        // Build proto Channel with updated fields
        let proto_channel = chat_v1::Channel {
            id: channel_id.to_string(),
            r#type: domain_channel_type_to_proto(existing.channel_type) as i32,
            name: name.unwrap_or(&existing.name).to_string(),
            config: Some(config.as_ref().map(domain_config_to_proto).unwrap_or_else(|| {
                domain_config_to_proto(&existing.config)
            })),
            owner_id: existing.owner_id.to_string(),
            members: vec![],
            created_at: existing.created_at.timestamp_millis() as u64,
            description: description
                .map(|s| s.to_string())
                .or_else(|| existing.description.clone())
                .unwrap_or_default(),
            img_url: image_url
                .map(|s| s.to_string())
                .or_else(|| existing.image_url.clone())
                .unwrap_or_default(),
            archived: existing.archived,
        };

        // Build FieldMask with fields being updated
        let mut paths = Vec::new();
        if name.is_some() {
            paths.push("name".to_string());
        }
        if description.is_some() {
            paths.push("description".to_string());
        }
        if image_url.is_some() {
            paths.push("img_url".to_string());
        }
        if config.is_some() {
            paths.push("config".to_string());
        }

        let update_mask = if paths.is_empty() {
            None
        } else {
            Some(prost_types::FieldMask { paths })
        };

        // Call server (streaming)
        let channel_id_str = channel_id.to_string();
        let mut stream = channel_client
            .update_channel(&channel_id_str, proto_channel, update_mask)
            .await?;

        // Consume stream, find channel_updated event
        let mut channel_opt: Option<Channel> = None;
        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            if let Some(channel_event::Event::ChannelUpdated(proto_channel)) = event.event {
                channel_opt = Some(proto_to_domain_channel(&proto_channel)?);
                break;
            }
        }

        let channel = channel_opt.ok_or_else(|| {
            ChannelError::InvalidInput("no channel updated event received".to_string())
        })?;

        // Update local storage
        self.channel_repo.update(&channel)?;

        Ok(channel)
    }

    /// Delete a channel.
    ///
    /// # Arguments
    ///
    /// * `channel_client` - Mutable reference to channel client
    /// * `channel_id` - ID of channel to delete
    pub async fn delete_channel(
        &self,
        channel_client: &mut ChannelClient,
        channel_id: &ChannelId,
    ) -> Result<(), ChannelError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Call server (streaming)
        let channel_id_str = channel_id.to_string();
        let mut stream = channel_client
            .delete_channel(&channel_id_str)
            .await?;

        // Consume stream, find channel_deleted event
        let mut deleted = false;
        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            if let Some(channel_event::Event::ChannelDeleted(_)) = event.event {
                deleted = true;
                break;
            }
        }

        if !deleted {
            return Err(ChannelError::InvalidInput(
                "no channel deleted event received".to_string(),
            ));
        }

        // Delete from local storage (ignore NotFound - may already be deleted)
        let _ = self.channel_repo.delete(channel_id);

        // Delete memberships for this channel
        if let Ok(memberships) = self.channel_repo.list_members(channel_id) {
            for membership in memberships {
                let _ = self.channel_repo.delete_membership(channel_id, &membership.user_id);
            }
        }

        Ok(())
    }

    /// Join a channel.
    ///
    /// # Arguments
    ///
    /// * `channel_client` - Mutable reference to channel client
    /// * `channel_id` - ID of channel to join
    /// * `password` - Optional password for password-protected channels
    pub async fn join_channel(
        &self,
        channel_client: &mut ChannelClient,
        channel_id: &ChannelId,
        password: Option<&str>,
    ) -> Result<Channel, ChannelError> {
        // Check logged in
        let current_user = self.current_user_id()?;

        // Call server (streaming, don't start event streaming)
        let channel_id_str = channel_id.to_string();
        let mut stream = channel_client
            .join_channel(&channel_id_str, password, false)
            .await?;

        // Consume stream, look for member_added event
        let mut joined = false;
        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            match event.event {
                Some(channel_event::Event::MemberAdded(member)) => {
                    // Check that it's us who was added
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

        // Drop stream explicitly before using channel_client again
        drop(stream);

        // Get channel from local DB or fetch from server
        let channel = match self.channel_repo.get(channel_id)? {
            Some(ch) => ch,
            None => self.get_channel(channel_client, channel_id).await?,
        };

        // Create membership record for current user with role Member
        let membership = Membership {
            channel_id: channel_id.clone(),
            user_id: current_user,
            role: MemberRole::Member,
            joined_at: Utc::now(),
            last_read_message_id: None,
            is_muted: false,
            muted_until: None,
            notification_preference: NotificationPreference::All,
        };
        self.channel_repo.insert_membership(&membership)?;

        Ok(channel)
    }

    /// Leave a channel.
    ///
    /// # Arguments
    ///
    /// * `channel_client` - Mutable reference to channel client
    /// * `channel_id` - ID of channel to leave
    pub async fn leave_channel(
        &self,
        channel_client: &mut ChannelClient,
        channel_id: &ChannelId,
    ) -> Result<(), ChannelError> {
        // Check logged in
        let current_user = self.current_user_id()?;

        // Call server (streaming)
        let channel_id_str = channel_id.to_string();
        let mut stream = channel_client
            .leave_channel(&channel_id_str)
            .await?;

        // Consume stream, look for member_removed_id event
        let mut left = false;
        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            match event.event {
                Some(channel_event::Event::MemberRemovedId(removed_user_id)) => {
                    // Check that it's us who was removed
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

        // Delete membership from local DB (ignore NotFound - may already be deleted)
        let _ = self.channel_repo.delete_membership(channel_id, &current_user);

        Ok(())
    }

    /// Invite a user to a channel.
    ///
    /// # Arguments
    ///
    /// * `channel_client` - Mutable reference to channel client
    /// * `channel_id` - ID of channel to invite to
    /// * `invitee_id` - User ID to invite
    /// * `message` - Optional invitation message
    pub async fn invite_user(
        &self,
        channel_client: &mut ChannelClient,
        channel_id: &ChannelId,
        invitee_id: &UserId,
        message: Option<&str>,
    ) -> Result<(), ChannelError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Call server (streaming)
        let channel_id_str = channel_id.to_string();
        let invitee_id_str = invitee_id.to_string();
        let mut stream = channel_client
            .invite_user(&channel_id_str, &invitee_id_str, message)
            .await?;

        // Consume stream, look for invitation_sent event
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

        // No local storage update needed (invitation is server-side state)
        Ok(())
    }

    /// Respond to a channel invitation.
    ///
    /// # Arguments
    ///
    /// * `channel_client` - Mutable reference to channel client
    /// * `channel_id` - ID of channel being invited to
    /// * `accept` - True to accept, false to decline
    ///
    /// # Returns
    ///
    /// If accepted, returns the channel. If declined, returns None.
    pub async fn respond_to_invite(
        &self,
        channel_client: &mut ChannelClient,
        channel_id: &ChannelId,
        accept: bool,
    ) -> Result<Option<Channel>, ChannelError> {
        // Check logged in
        let current_user = self.current_user_id()?;

        // Call server (streaming)
        let channel_id_str = channel_id.to_string();
        let mut stream = channel_client
            .respond_to_invite(&channel_id_str, accept)
            .await?;

        // Consume stream
        let mut responded = false;
        let mut joined = false;
        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            match event.event {
                Some(channel_event::Event::MemberAdded(member)) => {
                    // If we accepted, we get member_added event
                    if accept && member.user_id == current_user.to_string() {
                        joined = true;
                        responded = true;
                        break;
                    }
                }
                Some(channel_event::Event::InvitationResponse(_response)) => {
                    // If we declined, we get invitation_response event
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

        // Drop stream before using channel_client again
        drop(stream);

        // If accepted, fetch channel details and create local membership
        if joined {
            let channel = self.get_channel(channel_client, channel_id).await?;

            let membership = Membership {
                channel_id: channel_id.clone(),
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
        } else {
            Ok(None)
        }
    }

    /// Get channel members with pagination.
    ///
    /// # Arguments
    ///
    /// * `channel_client` - Mutable reference to channel client
    /// * `channel_id` - ID of channel to get members from
    /// * `limit` - Maximum number of members to fetch
    /// * `offset` - Pagination offset
    pub async fn get_channel_members(
        &self,
        channel_client: &mut ChannelClient,
        channel_id: &ChannelId,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<Membership>, ChannelError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Call server (unary)
        let channel_id_str = channel_id.to_string();
        let response = channel_client
            .get_channel_members(&channel_id_str, limit as i32, offset as i32)
            .await?;

        // Convert each proto ChannelMember to domain Membership
        let mut memberships = Vec::with_capacity(response.members.len());
        for proto_member in &response.members {
            let membership = proto_to_domain_membership(channel_id, proto_member)?;

            // Upsert to local storage
            match self.channel_repo.get_membership(channel_id, &membership.user_id)? {
                Some(_) => self.channel_repo.update_membership(&membership)?,
                None => self.channel_repo.insert_membership(&membership)?,
            }

            memberships.push(membership);
        }

        Ok(memberships)
    }

    /// Update a member's role.
    ///
    /// # Arguments
    ///
    /// * `channel_client` - Mutable reference to channel client
    /// * `channel_id` - Channel containing the member
    /// * `user_id` - User whose role to update
    /// * `new_role` - The new role to assign
    pub async fn update_member_role(
        &self,
        channel_client: &mut ChannelClient,
        channel_id: &ChannelId,
        user_id: &UserId,
        new_role: MemberRole,
    ) -> Result<Membership, ChannelError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Convert domain MemberRole to proto ChannelRole
        let proto_role = domain_role_to_proto(new_role);

        // Call server (streaming)
        let channel_id_str = channel_id.to_string();
        let user_id_str = user_id.to_string();
        let mut stream = channel_client
            .update_member_role(&channel_id_str, &user_id_str, proto_role)
            .await?;

        // Consume stream, look for member_updated event
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

        // Convert to domain Membership
        let membership = proto_to_domain_membership(channel_id, &proto_member)?;

        // Update local storage
        match self.channel_repo.get_membership(channel_id, &membership.user_id)? {
            Some(_) => self.channel_repo.update_membership(&membership)?,
            None => self.channel_repo.insert_membership(&membership)?,
        }

        Ok(membership)
    }

    // ==================== Archive Operations ====================

    /// Archive a channel.
    ///
    /// # Arguments
    ///
    /// * `channel_client` - Mutable reference to channel client
    /// * `channel_id` - ID of channel to archive
    pub async fn archive_channel(
        &self,
        channel_client: &mut ChannelClient,
        channel_id: &ChannelId,
    ) -> Result<Channel, ChannelError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Call server (unary, returns ArchiveChannelResponse)
        let channel_id_str = channel_id.to_string();
        let response = channel_client.archive_channel(&channel_id_str).await?;

        // Extract archived_channel from response
        let proto_channel = response
            .archived_channel
            .ok_or_else(|| ChannelError::InvalidInput("no archived_channel in response".to_string()))?;

        // Convert to domain Channel (should have archived=true)
        let channel = proto_to_domain_channel(&proto_channel)?;

        // Update local storage
        match self.channel_repo.get(channel_id)? {
            Some(_) => self.channel_repo.update(&channel)?,
            None => self.channel_repo.insert(&channel)?,
        }

        Ok(channel)
    }

    /// Unarchive a channel.
    ///
    /// # Arguments
    ///
    /// * `channel_client` - Mutable reference to channel client
    /// * `channel_id` - ID of channel to unarchive
    pub async fn unarchive_channel(
        &self,
        channel_client: &mut ChannelClient,
        channel_id: &ChannelId,
    ) -> Result<Channel, ChannelError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Call server (unary, returns UnarchiveChannelResponse)
        let channel_id_str = channel_id.to_string();
        let response = channel_client.unarchive_channel(&channel_id_str).await?;

        // Extract unarchived_channel from response
        let proto_channel = response
            .unarchived_channel
            .ok_or_else(|| ChannelError::InvalidInput("no unarchived_channel in response".to_string()))?;

        // Convert to domain Channel (should have archived=false)
        let channel = proto_to_domain_channel(&proto_channel)?;

        // Update local storage
        match self.channel_repo.get(channel_id)? {
            Some(_) => self.channel_repo.update(&channel)?,
            None => self.channel_repo.insert(&channel)?,
        }

        Ok(channel)
    }

    /// List archived channel IDs.
    ///
    /// # Arguments
    ///
    /// * `channel_client` - Mutable reference to channel client
    /// * `limit` - Maximum number of channels to fetch
    /// * `offset` - Pagination offset
    ///
    /// # Returns
    ///
    /// Vector of ChannelIds for archived channels. Caller can fetch full details if needed.
    pub async fn list_archived_channels(
        &self,
        channel_client: &mut ChannelClient,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<ChannelId>, ChannelError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Call server (unary)
        let response = channel_client
            .list_archived_channels(limit as i32, offset as i32)
            .await?;

        // Response contains archived_channel_ids as strings - parse each to ChannelId
        let mut channel_ids = Vec::with_capacity(response.archived_channel_ids.len());
        for id_str in &response.archived_channel_ids {
            let channel_id: ChannelId = id_str.parse().map_err(|_| {
                ChannelError::InvalidInput(format!("invalid channel ID: {}", id_str))
            })?;
            channel_ids.push(channel_id);
        }

        Ok(channel_ids)
    }

    /// List channels with pagination.
    ///
    /// Fetches from server and upserts each to local storage.
    ///
    /// # Arguments
    ///
    /// * `channel_client` - Mutable reference to channel client
    /// * `limit` - Maximum number of channels to fetch
    /// * `offset` - Pagination offset
    /// * `archived` - Optional filter for archived status
    pub async fn list_channels(
        &self,
        channel_client: &mut ChannelClient,
        limit: u32,
        offset: u32,
        archived: Option<bool>,
    ) -> Result<Vec<Channel>, ChannelError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Call server (unary)
        let response = channel_client
            .list_channels(limit as i32, offset as i32, archived)
            .await?;

        // Convert each proto Channel to domain Channel
        let mut channels = Vec::with_capacity(response.channels.len());
        for proto in &response.channels {
            let channel = proto_to_domain_channel(proto)?;

            // Upsert to local storage
            match self.channel_repo.get(&channel.id)? {
                Some(_) => self.channel_repo.update(&channel)?,
                None => self.channel_repo.insert(&channel)?,
            }

            channels.push(channel);
        }

        Ok(channels)
    }

    // ==================== Search & Local Queries ====================

    /// Search for channels by query.
    ///
    /// Returns channels matching the search query. Results are NOT stored locally
    /// (search is for discovery, not membership).
    ///
    /// # Arguments
    ///
    /// * `channel_client` - Mutable reference to channel client
    /// * `query` - Search query string
    /// * `limit` - Maximum number of results
    /// * `offset` - Pagination offset
    /// * `type_filter` - Optional filter for channel types
    pub async fn search_channels(
        &self,
        channel_client: &mut ChannelClient,
        query: &str,
        limit: u32,
        offset: u32,
        type_filter: Vec<ChannelType>,
    ) -> Result<Vec<Channel>, ChannelError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Convert domain ChannelType to proto ChannelType
        let proto_type_filter: Vec<chat_v1::ChannelType> = type_filter
            .into_iter()
            .map(domain_channel_type_to_proto)
            .collect();

        // Call server (unary)
        let response = channel_client
            .search_channels(query, limit as i32, offset as i32, vec![], proto_type_filter)
            .await?;

        // Convert each result to domain Channel
        let mut channels = Vec::with_capacity(response.results.len());
        for proto in &response.results {
            let channel = proto_to_domain_channel(proto)?;
            // Do NOT store locally - search results are discovery, not membership
            channels.push(channel);
        }

        Ok(channels)
    }

    /// Get all channels the current user is a member of.
    ///
    /// This is a local-only query that returns cached channel data.
    pub fn get_channels_for_user(&self) -> Result<Vec<Channel>, ChannelError> {
        // Check logged in
        let current_user = self.current_user_id()?;

        // Get channels from local storage (sorted by last_message_at by repo)
        let channels = self.channel_repo.list_for_user(&current_user)?;

        Ok(channels)
    }

    /// Get a channel from local storage.
    ///
    /// This is a local-only query that returns cached channel data without
    /// making a network request.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - ID of channel to retrieve
    pub fn get_channel_local(&self, channel_id: &ChannelId) -> Result<Option<Channel>, ChannelError> {
        // No auth check needed - local cache access
        let channel = self.channel_repo.get(channel_id)?;
        Ok(channel)
    }
}

/// Convert a proto Channel to a domain Channel.
fn proto_to_domain_channel(proto: &chat_v1::Channel) -> Result<Channel, ChannelError> {
    // Parse channel ID
    let id: ChannelId = proto
        .id
        .parse()
        .map_err(|_| ChannelError::InvalidInput("invalid channel ID".to_string()))?;

    // Parse owner ID
    let owner_id: UserId = proto
        .owner_id
        .parse()
        .map_err(|_| ChannelError::InvalidInput("invalid owner ID".to_string()))?;

    // Convert proto ChannelType to domain ChannelType
    let channel_type = proto_channel_type_to_domain(proto.r#type);

    // Convert proto Config to domain ChannelConfig
    let config = proto_config_to_domain(proto.config.as_ref());

    // Convert timestamp (proto uses millis)
    let created_at = millis_to_datetime(proto.created_at);

    Ok(Channel {
        id,
        channel_type,
        name: proto.name.clone(),
        description: if proto.description.is_empty() {
            None
        } else {
            Some(proto.description.clone())
        },
        image_url: if proto.img_url.is_empty() {
            None
        } else {
            Some(proto.img_url.clone())
        },
        owner_id,
        config,
        created_at,
        updated_at: created_at, // Proto doesn't have updated_at, use created_at
        archived: proto.archived,
        unread_count: 0,         // Local state, not from server
        last_message_at: None,   // Local state, not from server
    })
}

/// Convert proto ChannelType (i32) to domain ChannelType.
fn proto_channel_type_to_domain(proto_type: i32) -> ChannelType {
    match proto_type {
        1 => ChannelType::Public,
        2 => ChannelType::Private,
        3 => ChannelType::DirectMessage,
        _ => ChannelType::Public, // Default for unspecified
    }
}

/// Convert domain ChannelType to proto ChannelType.
fn domain_channel_type_to_proto(channel_type: ChannelType) -> chat_v1::ChannelType {
    match channel_type {
        ChannelType::Public => chat_v1::ChannelType::ChannelPublic,
        ChannelType::Private => chat_v1::ChannelType::ChannelPrivate,
        ChannelType::DirectMessage => chat_v1::ChannelType::DirectMessage,
    }
}

/// Convert proto Config to domain ChannelConfig.
fn proto_config_to_domain(proto_config: Option<&chat_v1::channel::Config>) -> ChannelConfig {
    match proto_config {
        Some(config) => ChannelConfig {
            persistence: proto_persistence_to_domain(config.persistence),
            max_members: if config.max_members > 0 {
                Some(config.max_members as u32)
            } else {
                None
            },
            default_ttl_seconds: config.ttl.as_ref().map(|d| d.seconds as u32),
            allow_reactions: true,      // Default, not in proto
            allow_threads: true,        // Default, not in proto
            allow_attachments: true,    // Default, not in proto
            require_encryption: config.encrypted,
        },
        None => ChannelConfig::default(),
    }
}

/// Convert domain ChannelConfig to proto Config.
fn domain_config_to_proto(config: &ChannelConfig) -> chat_v1::channel::Config {
    chat_v1::channel::Config {
        persistence: domain_persistence_to_proto(config.persistence) as i32,
        ttl: config.default_ttl_seconds.map(|s| prost_types::Duration {
            seconds: s as i64,
            nanos: 0,
        }),
        max_members: config.max_members.unwrap_or(0),
        encrypted: config.require_encryption,
        password_hash: String::new(),
        friends_only: false,
    }
}

/// Convert proto Persistence to domain MessagePersistence.
fn proto_persistence_to_domain(proto_persistence: i32) -> MessagePersistence {
    match proto_persistence {
        1 => MessagePersistence::Ephemeral,
        2 => MessagePersistence::Permanent,
        _ => MessagePersistence::Permanent, // Default
    }
}

/// Convert domain MessagePersistence to proto Persistence.
fn domain_persistence_to_proto(
    persistence: MessagePersistence,
) -> chat_v1::channel::config::Persistence {
    match persistence {
        MessagePersistence::Permanent => chat_v1::channel::config::Persistence::Persistent,
        MessagePersistence::Ephemeral => chat_v1::channel::config::Persistence::Ephemeral,
        MessagePersistence::Hybrid => chat_v1::channel::config::Persistence::Persistent,
    }
}

/// Convert milliseconds timestamp to DateTime<Utc>.
fn millis_to_datetime(millis: u64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(millis as i64)
        .single()
        .unwrap_or_else(Utc::now)
}

/// Convert a proto ChannelMember to domain Membership.
fn proto_to_domain_membership(
    channel_id: &ChannelId,
    proto: &chat_v1::channel::ChannelMember,
) -> Result<Membership, ChannelError> {
    let user_id: UserId = proto
        .user_id
        .parse()
        .map_err(|_| ChannelError::InvalidInput("invalid user ID in member".to_string()))?;

    let role = proto_role_to_domain(proto.role);
    let joined_at = millis_to_datetime(proto.joined_at);

    Ok(Membership {
        channel_id: channel_id.clone(),
        user_id,
        role,
        joined_at,
        last_read_message_id: None, // Not in proto
        is_muted: proto.muted,
        muted_until: None, // Not in proto (ban_expires_at is for bans, not mutes)
        notification_preference: NotificationPreference::All, // Not in proto
    })
}

/// Convert proto ChannelRole (i32) to domain MemberRole.
fn proto_role_to_domain(proto_role: i32) -> MemberRole {
    match proto_role {
        1 => MemberRole::Member,
        2 => MemberRole::Moderator,
        3 => MemberRole::Admin,
        4 => MemberRole::Owner,
        _ => MemberRole::Member, // Default for unspecified
    }
}

/// Convert domain MemberRole to proto ChannelRole.
fn domain_role_to_proto(role: MemberRole) -> ChannelRole {
    match role {
        MemberRole::Member => ChannelRole::Member,
        MemberRole::Moderator => ChannelRole::Moderator,
        MemberRole::Admin => ChannelRole::Admin,
        MemberRole::Owner => ChannelRole::Owner,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_channel_error_display() {
        let err = ChannelError::NotLoggedIn;
        assert!(err.to_string().contains("not logged in"));

        let err = ChannelError::InvalidInput("empty name".to_string());
        assert!(err.to_string().contains("empty name"));

        let channel_id = ChannelId::new();
        let err = ChannelError::ChannelNotFound(channel_id);
        assert!(err.to_string().contains("channel not found"));

        let err = ChannelError::NotAuthorized;
        assert!(err.to_string().contains("not authorized"));
    }

    #[test]
    fn test_proto_channel_type_conversion() {
        assert_eq!(proto_channel_type_to_domain(1), ChannelType::Public);
        assert_eq!(proto_channel_type_to_domain(2), ChannelType::Private);
        assert_eq!(proto_channel_type_to_domain(3), ChannelType::DirectMessage);
        assert_eq!(proto_channel_type_to_domain(0), ChannelType::Public); // Unspecified
    }

    #[test]
    fn test_domain_channel_type_conversion() {
        assert_eq!(
            domain_channel_type_to_proto(ChannelType::Public),
            chat_v1::ChannelType::ChannelPublic
        );
        assert_eq!(
            domain_channel_type_to_proto(ChannelType::Private),
            chat_v1::ChannelType::ChannelPrivate
        );
        assert_eq!(
            domain_channel_type_to_proto(ChannelType::DirectMessage),
            chat_v1::ChannelType::DirectMessage
        );
    }

    #[test]
    fn test_millis_to_datetime() {
        let millis = 1706140800000u64; // 2024-01-25 00:00:00 UTC
        let dt = millis_to_datetime(millis);
        assert_eq!(dt.timestamp_millis(), millis as i64);
    }

    #[test]
    fn test_proto_role_to_domain() {
        assert_eq!(proto_role_to_domain(0), MemberRole::Member); // Unspecified
        assert_eq!(proto_role_to_domain(1), MemberRole::Member);
        assert_eq!(proto_role_to_domain(2), MemberRole::Moderator);
        assert_eq!(proto_role_to_domain(3), MemberRole::Admin);
        assert_eq!(proto_role_to_domain(4), MemberRole::Owner);
    }

    #[test]
    fn test_domain_role_to_proto() {
        assert_eq!(domain_role_to_proto(MemberRole::Member), ChannelRole::Member);
        assert_eq!(domain_role_to_proto(MemberRole::Moderator), ChannelRole::Moderator);
        assert_eq!(domain_role_to_proto(MemberRole::Admin), ChannelRole::Admin);
        assert_eq!(domain_role_to_proto(MemberRole::Owner), ChannelRole::Owner);
    }

    #[test]
    fn test_role_conversion_roundtrip() {
        // Test roundtrip: domain -> proto -> domain
        for role in [MemberRole::Member, MemberRole::Moderator, MemberRole::Admin, MemberRole::Owner] {
            let proto = domain_role_to_proto(role);
            let back = proto_role_to_domain(proto as i32);
            assert_eq!(back, role);
        }
    }
}
