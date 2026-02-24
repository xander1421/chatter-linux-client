//! Channel service implementation.
//!
//! Create, get, update, delete, and list channels.

use std::sync::Arc;

use chrono::Utc;
use tokio_stream::StreamExt;

use crate::domain::channel::{
    Channel, ChannelConfig, ChannelType, MemberRole, Membership, NotificationPreference,
};
use crate::domain::ids::{ChannelId, UserId};
use crate::mailbox::group::{GroupCoordinator, GroupMailboxSetupResult};
use crate::network::grpc::channel_client::ChannelClient;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::mailbox_client::MailboxClient;
use crate::proto::chat_v1::{self, channel_event};
use crate::proto::content_v1::SenderKeyDistribution;
use crate::storage::repositories::channel_repo::ChannelRepository;
use crate::storage::repositories::settings_repo::{keys, SettingsRepository};

use super::converters::{
    domain_channel_type_to_proto, domain_config_to_proto, domain_role_to_proto,
    proto_to_domain_channel, proto_to_domain_membership,
};
use super::error::ChannelError;

/// Channel service for all channel operations.
///
/// Orchestrates channel creation, retrieval, updates, and deletion by
/// coordinating network client and local storage.
pub struct ChannelService {
    grpc_pool: Arc<GrpcClientPool>,
    settings_repo: Arc<SettingsRepository>,
    channel_repo: Arc<ChannelRepository>,
    signal_service: Arc<crate::services::signal_service::SignalService>,
    group_coordinator: Arc<GroupCoordinator>,
}

impl ChannelService {
    /// Create a new ChannelService with dependencies.
    pub fn new(
        grpc_pool: Arc<GrpcClientPool>,
        settings_repo: Arc<SettingsRepository>,
        channel_repo: Arc<ChannelRepository>,
        signal_service: Arc<crate::services::signal_service::SignalService>,
        group_coordinator: Arc<GroupCoordinator>,
    ) -> Self {
        Self {
            grpc_pool,
            settings_repo,
            channel_repo,
            signal_service,
            group_coordinator,
        }
    }

    /// Create a channel client for network operations.
    fn channel_client(&self) -> ChannelClient {
        ChannelClient::new(self.grpc_pool.channel(), self.grpc_pool.auth_clone())
    }

    /// Create a mailbox client for network operations.
    fn mailbox_client(&self) -> MailboxClient {
        MailboxClient::new(self.grpc_pool.channel(), self.grpc_pool.auth_clone())
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

    // ==================== Create Operations ====================

    /// Create a new channel.
    ///
    /// # Arguments
    ///
    /// * `name` - Channel name
    /// * `channel_type` - Type of channel (Public, Private, DirectMessage)
    /// * `config` - Optional channel configuration
    /// * `description` - Optional channel description
    /// * `image_url` - Optional channel image URL
    /// * `password` - Optional password for private channels
    /// * `initial_members` - Initial member user IDs
    /// * `setup_mailbox` - Whether to setup group mailbox for encrypted channels
    ///
    /// # Group Mailbox Setup
    ///
    /// If `setup_mailbox` is true and the channel requires encryption,
    /// this method will automatically:
    /// 1. Create an ephemeral group mailbox for receiving messages
    /// 2. The caller should send the returned announcement via
    ///    `MessageService::send_group_mailbox_announcement()`
    #[allow(clippy::too_many_arguments)]
    pub async fn create_channel(
        &self,
        name: &str,
        channel_type: ChannelType,
        config: Option<ChannelConfig>,
        description: Option<&str>,
        image_url: Option<&str>,
        password: Option<&str>,
        initial_members: Vec<UserId>,
        setup_mailbox: bool,
    ) -> Result<(Channel, Option<GroupMailboxSetupResult>), ChannelError> {
        let current_user = self.current_user_id()?;

        let trimmed_name = name.trim();
        if trimmed_name.is_empty() {
            return Err(ChannelError::InvalidInput(
                "channel name cannot be empty".to_string(),
            ));
        }

        let proto_type = domain_channel_type_to_proto(channel_type);
        let proto_config = config.as_ref().map(domain_config_to_proto);

        let mut channel_client = self.channel_client();
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

        let mut channel_opt: Option<Channel> = None;
        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            tracing::debug!(
                channel_id = %event.channel_id,
                creation_status = event.creation_status,
                has_event = event.event.is_some(),
                "Received channel creation stream event"
            );
            if let Some(channel_event::Event::ChannelCreated(proto_channel)) = event.event {
                tracing::info!(
                    channel_id = %proto_channel.id,
                    channel_name = %proto_channel.name,
                    "Received ChannelCreated event from server"
                );
                channel_opt = Some(proto_to_domain_channel(&proto_channel)?);
                break;
            }
        }

        // Drop stream to release borrow of channel_client
        drop(stream);

        if channel_opt.is_none() {
            tracing::error!("Stream ended without receiving ChannelCreated event");
        }

        let channel = channel_opt.ok_or_else(|| {
            ChannelError::InvalidInput("no channel created event received".to_string())
        })?;

        tracing::info!(channel_id = %channel.id, "Inserting channel to local repo");
        self.channel_repo.insert(&channel)?;
        tracing::info!(channel_id = %channel.id, "Channel inserted");

        let membership = Membership {
            channel_id: channel.id,
            user_id: current_user,
            role: MemberRole::Owner,
            joined_at: channel.created_at,
            last_read_message_id: None,
            is_muted: false,
            muted_until: None,
            notification_preference: NotificationPreference::All,
        };
        tracing::info!(channel_id = %channel.id, "Inserting membership to local repo");
        self.channel_repo.insert_membership(&membership)?;
        tracing::info!(channel_id = %channel.id, "Membership inserted");

        // Setup group mailbox for encrypted channels (NDSS '21 paper)
        tracing::info!(channel_id = %channel.id, setup_mailbox = setup_mailbox, "About to setup group mailbox");
        let mailbox_setup = if setup_mailbox {
            let mut mailbox_client = self.mailbox_client();
            match self.setup_group_mailbox_internal(&channel.id, &mut mailbox_client).await {
                Ok(Some(setup)) => Some(setup),
                Ok(None) => None,
                Err(e) => {
                    tracing::error!(
                        channel_id = %channel.id,
                        error = %e,
                        "Group mailbox setup failed during channel creation - group encryption unavailable"
                    );
                    None
                }
            }
        } else {
            None
        };

        // Sync members (in case initial_members were added by server)
        if let Err(e) = self.sync_channel_members_internal(&mut channel_client, &channel.id).await {
            tracing::debug!(
                channel_id = %channel.id,
                error = %e,
                "Failed to sync members after creation (may be normal for new channel)"
            );
        }

        Ok((channel, mailbox_setup))
    }

    // ==================== CRUD Operations ====================

    /// Get a channel by ID.
    ///
    /// Fetches from server and upserts to local storage.
    pub async fn get_channel(&self, channel_id: &ChannelId) -> Result<Channel, ChannelError> {
        let _ = self.current_user_id()?;

        let mut channel_client = self.channel_client();
        let proto_channel = channel_client
            .get_channel(&channel_id.to_string())
            .await?;

        let channel = proto_to_domain_channel(&proto_channel)?;

        match self.channel_repo.get(channel_id)? {
            Some(_) => self.channel_repo.update(&channel)?,
            None => self.channel_repo.insert(&channel)?,
        }

        Ok(channel)
    }

    /// Get a channel by ID and sync its memberships.
    ///
    /// Fetches from server, upserts channel to local storage, and syncs all memberships.
    /// Useful for DM channels where we need membership info immediately.
    pub async fn get_channel_with_memberships(
        &self,
        channel_id: &ChannelId,
    ) -> Result<Channel, ChannelError> {
        let _ = self.current_user_id()?;

        let mut channel_client = self.channel_client();
        let proto_channel = channel_client
            .get_channel(&channel_id.to_string())
            .await?;

        let channel = proto_to_domain_channel(&proto_channel)?;

        // Upsert channel
        match self.channel_repo.get(channel_id)? {
            Some(_) => self.channel_repo.update(&channel)?,
            None => self.channel_repo.insert(&channel)?,
        }

        // Sync memberships
        for proto_member in &proto_channel.members {
            match proto_to_domain_membership(channel_id, proto_member) {
                Ok(membership) => {
                    if let Err(e) = self.channel_repo.insert_or_ignore_membership(&membership) {
                        tracing::warn!(
                            channel_id = %channel_id,
                            user_id = %membership.user_id,
                            error = %e,
                            "Failed to sync membership"
                        );
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

        Ok(channel)
    }

    /// Internal: Get a channel using an existing client.
    async fn get_channel_internal(
        &self,
        channel_client: &mut ChannelClient,
        channel_id: &ChannelId,
    ) -> Result<Channel, ChannelError> {
        let proto_channel = channel_client
            .get_channel(&channel_id.to_string())
            .await?;

        let channel = proto_to_domain_channel(&proto_channel)?;

        match self.channel_repo.get(channel_id)? {
            Some(_) => self.channel_repo.update(&channel)?,
            None => self.channel_repo.insert(&channel)?,
        }

        Ok(channel)
    }

    /// Update a channel.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_channel(
        &self,
        channel_id: &ChannelId,
        name: Option<&str>,
        description: Option<&str>,
        image_url: Option<&str>,
        config: Option<ChannelConfig>,
    ) -> Result<Channel, ChannelError> {
        let _ = self.current_user_id()?;

        let existing = self
            .channel_repo
            .get(channel_id)?
            .ok_or_else(|| ChannelError::ChannelNotFound(*channel_id))?;

        let proto_channel = chat_v1::Channel {
            id: channel_id.to_string(),
            r#type: domain_channel_type_to_proto(existing.channel_type) as i32,
            name: name.unwrap_or(&existing.name).to_string(),
            config: Some(
                config
                    .as_ref()
                    .map(domain_config_to_proto)
                    .unwrap_or_else(|| domain_config_to_proto(&existing.config)),
            ),
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

        let channel_id_str = channel_id.to_string();
        let mut channel_client = self.channel_client();
        let mut stream = channel_client
            .update_channel(&channel_id_str, proto_channel, update_mask)
            .await?;

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

        self.channel_repo.update(&channel)?;

        Ok(channel)
    }

    /// Delete a channel.
    pub async fn delete_channel(&self, channel_id: &ChannelId) -> Result<(), ChannelError> {
        let _ = self.current_user_id()?;

        let channel_id_str = channel_id.to_string();
        let mut channel_client = self.channel_client();
        let mut stream = channel_client.delete_channel(&channel_id_str).await?;

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

        let _ = self.channel_repo.delete(channel_id);

        if let Ok(memberships) = self.channel_repo.list_members(channel_id) {
            for membership in memberships {
                let _ = self
                    .channel_repo
                    .delete_membership(channel_id, &membership.user_id);
            }
        }

        Ok(())
    }

    // ==================== Membership Operations ====================

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
    ) -> Result<(Channel, Option<GroupMailboxSetupResult>), ChannelError> {
        let current_user = self.current_user_id()?;

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
    async fn sync_channel_members_internal(
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
        let current_user = self.current_user_id()?;

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

        if let Err(e) = self.signal_service.clear_channel_sender_keys(channel_id) {
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
        let _ = self.current_user_id()?;

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
    ) -> Result<Option<(Channel, Option<GroupMailboxSetupResult>)>, ChannelError> {
        let current_user = self.current_user_id()?;

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
        let _ = self.current_user_id()?;

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
        let _ = self.current_user_id()?;

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

    // ==================== Archive Operations ====================

    /// Archive a channel.
    pub async fn archive_channel(&self, channel_id: &ChannelId) -> Result<Channel, ChannelError> {
        let _ = self.current_user_id()?;

        let channel_id_str = channel_id.to_string();
        let mut channel_client = self.channel_client();
        let response = channel_client.archive_channel(&channel_id_str).await?;

        let proto_channel = response.archived_channel.ok_or_else(|| {
            ChannelError::InvalidInput("no archived_channel in response".to_string())
        })?;

        let channel = proto_to_domain_channel(&proto_channel)?;

        match self.channel_repo.get(channel_id)? {
            Some(_) => self.channel_repo.update(&channel)?,
            None => self.channel_repo.insert(&channel)?,
        }

        Ok(channel)
    }

    /// Unarchive a channel.
    pub async fn unarchive_channel(&self, channel_id: &ChannelId) -> Result<Channel, ChannelError> {
        let _ = self.current_user_id()?;

        let channel_id_str = channel_id.to_string();
        let mut channel_client = self.channel_client();
        let response = channel_client.unarchive_channel(&channel_id_str).await?;

        let proto_channel = response.unarchived_channel.ok_or_else(|| {
            ChannelError::InvalidInput("no unarchived_channel in response".to_string())
        })?;

        let channel = proto_to_domain_channel(&proto_channel)?;

        match self.channel_repo.get(channel_id)? {
            Some(_) => self.channel_repo.update(&channel)?,
            None => self.channel_repo.insert(&channel)?,
        }

        Ok(channel)
    }

    /// List archived channel IDs.
    pub async fn list_archived_channels(
        &self,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<ChannelId>, ChannelError> {
        let _ = self.current_user_id()?;

        let mut channel_client = self.channel_client();
        let response = channel_client
            .list_archived_channels(limit as i32, offset as i32)
            .await?;

        let mut channel_ids = Vec::with_capacity(response.archived_channel_ids.len());
        for id_str in &response.archived_channel_ids {
            let channel_id: ChannelId = id_str.parse().map_err(|_| {
                ChannelError::InvalidInput(format!("invalid channel ID: {}", id_str))
            })?;
            channel_ids.push(channel_id);
        }

        Ok(channel_ids)
    }

    // ==================== List & Search Operations ====================

    /// List channels with pagination.
    pub async fn list_channels(
        &self,
        limit: u32,
        offset: u32,
        archived: Option<bool>,
    ) -> Result<Vec<Channel>, ChannelError> {
        let _ = self.current_user_id()?;

        let mut channel_client = self.channel_client();
        let response = channel_client
            .list_channels(limit as i32, offset as i32, archived)
            .await?;

        let mut channels = Vec::with_capacity(response.channels.len());
        for proto in &response.channels {
            let channel = proto_to_domain_channel(proto)?;

            match self.channel_repo.get(&channel.id)? {
                Some(_) => self.channel_repo.update(&channel)?,
                None => self.channel_repo.insert(&channel)?,
            }

            // Sync memberships from the proto response
            for proto_member in &proto.members {
                match proto_to_domain_membership(&channel.id, proto_member) {
                    Ok(membership) => {
                        if let Err(e) = self.channel_repo.insert_or_ignore_membership(&membership) {
                            tracing::warn!(
                                channel_id = %channel.id,
                                user_id = %membership.user_id,
                                error = %e,
                                "Failed to sync membership"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            channel_id = %channel.id,
                            error = %e,
                            "Failed to convert proto membership"
                        );
                    }
                }
            }

            channels.push(channel);
        }

        Ok(channels)
    }

    /// Search for channels by query.
    pub async fn search_channels(
        &self,
        query: &str,
        limit: u32,
        offset: u32,
        type_filter: Vec<ChannelType>,
    ) -> Result<Vec<Channel>, ChannelError> {
        let _ = self.current_user_id()?;

        let proto_type_filter: Vec<chat_v1::ChannelType> = type_filter
            .into_iter()
            .map(domain_channel_type_to_proto)
            .collect();

        let mut channel_client = self.channel_client();
        let response = channel_client
            .search_channels(query, limit as i32, offset as i32, vec![], proto_type_filter)
            .await?;

        let mut channels = Vec::with_capacity(response.results.len());
        for proto in &response.results {
            let channel = proto_to_domain_channel(proto)?;
            channels.push(channel);
        }

        Ok(channels)
    }

    /// Get all channels the current user is a member of (local query).
    pub fn get_channels_for_user(&self) -> Result<Vec<Channel>, ChannelError> {
        let current_user = self.current_user_id()?;
        let channels = self.channel_repo.list_for_user(&current_user)?;
        Ok(channels)
    }

    /// Get a channel from local storage (no network request).
    pub fn get_channel_local(
        &self,
        channel_id: &ChannelId,
    ) -> Result<Option<Channel>, ChannelError> {
        let channel = self.channel_repo.get(channel_id)?;
        Ok(channel)
    }

    // ==================== Moderation Operations ====================

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

    // ==================== Group Mailbox Operations ====================

    /// Setup group mailbox for a channel.
    ///
    /// Creates an ephemeral mailbox for receiving group messages (NDSS '21 paper).
    /// Returns the setup result containing the mailbox and announcement proto.
    ///
    /// The caller should send the announcement via `MessageService::send_control_content()`
    /// so other members can learn our mailbox address.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - The channel to setup mailbox for
    ///
    /// # Returns
    ///
    /// * `Ok(Some(result))` - Mailbox created, announcement ready to send
    /// * `Ok(None)` - Skipped (DM or non-encrypted channel)
    /// * `Err(_)` - Setup failed
    pub async fn setup_group_mailbox(
        &self,
        channel_id: &ChannelId,
    ) -> Result<Option<GroupMailboxSetupResult>, ChannelError> {
        let mut mailbox_client = self.mailbox_client();
        self.setup_group_mailbox_internal(channel_id, &mut mailbox_client).await
    }

    /// Internal: Setup group mailbox using an existing client.
    async fn setup_group_mailbox_internal(
        &self,
        channel_id: &ChannelId,
        mailbox_client: &mut MailboxClient,
    ) -> Result<Option<GroupMailboxSetupResult>, ChannelError> {
        let _ = self.current_user_id()?;

        // Check if this is an encrypted channel that needs group mailbox
        let channel = self
            .channel_repo
            .get(channel_id)?
            .ok_or_else(|| ChannelError::ChannelNotFound(*channel_id))?;

        // Skip DMs - they use two-way anonymous mailboxes
        if channel.channel_type == ChannelType::DirectMessage {
            tracing::debug!(
                channel_id = %channel_id,
                "Skipping group mailbox setup for DM channel"
            );
            return Ok(None);
        }

        // Skip non-encrypted channels
        if !channel.config.require_encryption {
            tracing::debug!(
                channel_id = %channel_id,
                "Skipping group mailbox setup for non-encrypted channel"
            );
            return Ok(None);
        }

        // Setup the ephemeral mailbox via GroupCoordinator
        match self.group_coordinator.setup_group_mailbox(channel_id, mailbox_client).await {
            Ok(setup_result) => {
                tracing::info!(
                    channel_id = %channel_id,
                    mailbox_id = %setup_result.mailbox.id,
                    "Created group mailbox"
                );
                Ok(Some(setup_result))
            }
            Err(e) => {
                tracing::warn!(
                    channel_id = %channel_id,
                    error = %e,
                    "Failed to setup group mailbox"
                );
                Err(ChannelError::InvalidInput(format!(
                    "Failed to setup group mailbox: {}",
                    e
                )))
            }
        }
    }
}
