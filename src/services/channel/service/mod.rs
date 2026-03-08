//! Channel service implementation.
//!
//! Create, get, update, delete, and list channels.
//!
//! Additional functionality provided by submodules:
//! - membership.rs: Join, leave, invite operations
//! - moderation.rs: Kick, ban, unban operations
//! - listing.rs: List, search, archive operations
//! - mailbox.rs: Group mailbox setup

mod listing;
mod mailbox;
mod membership;
mod moderation;

use std::sync::Arc;

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
use crate::services::current_user::CurrentUserProvider;
use crate::storage::repositories::channel_repo::ChannelRepository;
use crate::storage::repositories::settings_repo::SettingsRepository;

use super::converters::{
    domain_channel_type_to_proto, domain_config_to_proto,
    proto_to_domain_channel, proto_to_domain_membership,
};
use super::error::ChannelError;

/// Channel service for all channel operations.
///
/// Orchestrates channel creation, retrieval, updates, and deletion by
/// coordinating network client and local storage.
#[derive(Clone)]
pub struct ChannelService {
    pub(crate) grpc_pool: Arc<GrpcClientPool>,
    pub(crate) settings_repo: Arc<SettingsRepository>,
    pub(crate) channel_repo: Arc<ChannelRepository>,
    pub(crate) signal_service: Arc<crate::services::signal_service::SignalService>,
    pub(crate) group_coordinator: Arc<GroupCoordinator>,
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

    /// Get the current user's ID.
    pub fn current_user_id(&self) -> Result<UserId, ChannelError> {
        self.settings_repo.current_user_id().map_err(ChannelError::from)
    }

    /// Create a channel client for network operations.
    pub(crate) fn channel_client(&self) -> ChannelClient {
        ChannelClient::new(self.grpc_pool.channel(), self.grpc_pool.auth_clone())
    }

    /// Create a mailbox client for network operations.
    pub(crate) fn mailbox_client(&self) -> MailboxClient {
        MailboxClient::new(self.grpc_pool.channel(), self.grpc_pool.auth_clone())
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
        let current_user = self.settings_repo.current_user_id()?;

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
        let _ = self.settings_repo.current_user_id()?;

        let mut channel_client = self.channel_client();
        let proto_channel = channel_client
            .get_channel(&channel_id.to_string())
            .await?;

        let channel = proto_to_domain_channel(&proto_channel)?;
        self.channel_repo.upsert(&channel)?;

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
        let _ = self.settings_repo.current_user_id()?;

        let mut channel_client = self.channel_client();
        let proto_channel = channel_client
            .get_channel(&channel_id.to_string())
            .await?;

        let channel = proto_to_domain_channel(&proto_channel)?;
        self.channel_repo.upsert(&channel)?;

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
    pub(crate) async fn get_channel_internal(
        &self,
        channel_client: &mut ChannelClient,
        channel_id: &ChannelId,
    ) -> Result<Channel, ChannelError> {
        let proto_channel = channel_client
            .get_channel(&channel_id.to_string())
            .await?;

        let channel = proto_to_domain_channel(&proto_channel)?;
        self.channel_repo.upsert(&channel)?;

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
        let _ = self.settings_repo.current_user_id()?;

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
        let _ = self.settings_repo.current_user_id()?;

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
}
