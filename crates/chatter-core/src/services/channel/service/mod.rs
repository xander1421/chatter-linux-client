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
    Channel, ChannelConfig, ChannelType,
};
use crate::domain::ids::{ChannelId, UserId};
use crate::mailbox::group::{GroupCoordinator, GroupMailboxSetupResult};
use crate::network::grpc::channel_client::ChannelClient;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::mailbox_client::MailboxClient;
use crate::network::grpc::AnonymousMailboxClient;
use crate::proto::chat_v1::{self, self as pb, channel_event};
use pb::channel_action::Action;
use crate::services::current_user::CurrentUserProvider;
use crate::storage::repositories::channel_repo::ChannelRepository;
use crate::storage::repositories::section_repo::SectionRepository;
use crate::storage::repositories::settings_repo::SettingsRepository;

use super::converters::{
    domain_channel_type_to_proto, domain_config_to_proto,
    proto_to_domain_channel,
};
use super::error::ChannelError;
use super::membership_log::{password_invite_key, AuthorBase, MembershipLog};
use super::verified_state::{ChannelRole, Policy};
use membership::action;

/// Channel service for all channel operations.
///
/// Orchestrates channel creation, retrieval, updates, and deletion by
/// coordinating network client and local storage.
#[derive(Clone)]
pub struct ChannelService {
    pub(crate) grpc_pool: Arc<GrpcClientPool>,
    pub(crate) settings_repo: Arc<SettingsRepository>,
    pub(crate) channel_repo: Arc<ChannelRepository>,
    pub(crate) section_repo: Arc<SectionRepository>,
    pub(crate) signal_service: Arc<crate::services::signal_service::SignalService>,
    pub(crate) group_coordinator: Arc<GroupCoordinator>,
    /// For SKDM redistribution after kick/leave; set after MessageService
    /// exists. Read access via `message_service()`.
    pub(crate) message_service: Arc<crate::services::message::MessageService>,
    /// Authoring and verification of the signed membership log.
    pub(crate) membership_log: Arc<MembershipLog>,
}

impl ChannelService {
    /// Create a new ChannelService with dependencies.
    pub fn new(
        grpc_pool: Arc<GrpcClientPool>,
        settings_repo: Arc<SettingsRepository>,
        channel_repo: Arc<ChannelRepository>,
        section_repo: Arc<SectionRepository>,
        signal_service: Arc<crate::services::signal_service::SignalService>,
        group_coordinator: Arc<GroupCoordinator>,
        message_service: Arc<crate::services::message::MessageService>,
        membership_log: Arc<MembershipLog>,
    ) -> Self {
        Self {
            grpc_pool,
            settings_repo,
            channel_repo,
            section_repo,
            signal_service,
            group_coordinator,
            message_service,
            membership_log,
        }
    }

    /// The channel membership log (verified state, authoring, catch-up).
    pub fn membership_log(&self) -> &Arc<MembershipLog> {
        &self.membership_log
    }

    /// Get the current user's ID.
    pub fn current_user_id(&self) -> Result<UserId, ChannelError> {
        self.settings_repo.current_user_id().map_err(ChannelError::from)
    }

    /// Create a channel client for network operations.
    pub(crate) fn channel_client(&self) -> ChannelClient {
        self.grpc_pool.channel_client()
    }

    /// Create an authenticated mailbox client for token requests.
    pub(crate) fn mailbox_client(&self) -> MailboxClient {
        self.grpc_pool.mailbox_client()
    }

    /// Create an anonymous mailbox client for mailbox creation/polling.
    pub(crate) fn anonymous_mailbox_client(&self) -> AnonymousMailboxClient {
        self.grpc_pool.anonymous_mailbox_client()
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
    /// * `icon` - Optional raw image bytes (PNG/JPEG/WebP, ≤64KB)
    /// * `password` - Optional channel password. Never sent: it seeds the
    ///   join capability in the genesis (`password_invite_key`)
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
        icon: Option<&[u8]>,
        password: Option<&str>,
        initial_members: Vec<UserId>,
        setup_mailbox: bool,
    ) -> Result<(Channel, Option<GroupMailboxSetupResult>), ChannelError> {
        let _ = self.settings_repo.current_user_id()?;
        let _ = setup_mailbox;

        let trimmed_name = name.trim();
        if trimmed_name.is_empty() {
            return Err(ChannelError::InvalidInput(
                "channel name cannot be empty".to_string(),
            ));
        }

        let proto_type = domain_channel_type_to_proto(channel_type);
        let proto_config = config.as_ref().map(domain_config_to_proto);
        let password = password.filter(|p| !p.is_empty());

        let channel = if matches!(channel_type, ChannelType::Public | ChannelType::Private) {
            // Revision 1 of the membership log names the channel id (picked
            // here). A password becomes a join capability whose key is
            // derived from it; a public channel without one is OPEN.
            let channel_id = ChannelId::from(uuid::Uuid::new_v4());
            let mut policy = Policy::defaults(channel_type == ChannelType::Public);
            let mut genesis = Vec::new();
            if let Some(p) = password {
                policy.join = super::verified_state::JoinMode::InviteOnly;
                let key = password_invite_key(&channel_id, p)?;
                genesis.push(action(Action::CreateInvite(pb::CreateInvite {
                    capability: Some(pb::InviteCapability {
                        invite_pub: super::membership_log::invite_pub_of(&key).to_vec(),
                        role: ChannelRole::Member as i32,
                        expires_at: 0,
                        max_uses: 0,
                        needs_approval: false,
                        uses: 0,
                    }),
                })));
            }
            genesis.insert(
                0,
                action(Action::Genesis(pb::Genesis { policy: Some(policy.to_proto()), disappearing_seconds: 0 })),
            );
            self.membership_log
                .author(
                    &channel_id,
                    AuthorBase::Genesis,
                    None,
                    |_| Ok(genesis.clone()),
                    |change| self.create_on_server(trimmed_name, proto_type, proto_config.clone(), description, icon, Some(change)),
                )
                .await?
        } else {
            self.create_on_server(trimmed_name, proto_type, proto_config.clone(), description, icon, None)
                .await?
        };

        // Initial members are invitees: each gets a signed invite and joins
        // by its own AcceptInvite.
        for member in initial_members {
            if let Err(e) = self.invite_user(&channel.id, &member, None).await {
                tracing::warn!(channel_id = %channel.id, user_id = %member, error = %e, "Failed to invite initial member");
            }
        }

        Ok((channel, None))
    }

    /// `CreateChannel` until its `ChannelCreated`; stores the channel row.
    #[allow(clippy::too_many_arguments)]
    async fn create_on_server(
        &self,
        name: &str,
        proto_type: chat_v1::ChannelType,
        proto_config: Option<chat_v1::channel::Config>,
        description: Option<&str>,
        icon: Option<&[u8]>,
        genesis: Option<chat_v1::ChannelChange>,
    ) -> Result<Channel, crate::error::NetworkError> {
        let mut stream = self
            .grpc_pool
            .channel_client()
            .create_channel(name, proto_type, proto_config, description, icon, genesis)
            .await?;
        while let Some(event) = stream.next().await {
            match event?.event {
                Some(channel_event::Event::ChannelCreated(proto_channel)) => {
                    let channel = proto_to_domain_channel(&proto_channel)
                        .map_err(|e| crate::error::NetworkError::InvalidRequest(e.to_string()))?;
                    self.channel_repo
                        .insert(&channel)
                        .map_err(|e| crate::error::NetworkError::Unknown(e.to_string()))?;
                    return Ok(channel);
                }
                Some(channel_event::Event::OperationFailed(failure)) => {
                    let msg = failure.error.map(|e| e.message).unwrap_or_else(|| "channel creation failed".into());
                    return Err(crate::error::NetworkError::InvalidRequest(msg));
                }
                _ => {}
            }
        }
        Err(crate::error::NetworkError::InvalidRequest("no channel created event received".into()))
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
        icon: Option<&[u8]>,
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
            icon: icon
                .map(|b| b.to_vec())
                .or_else(|| existing.icon.clone())
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
        if icon.is_some() {
            paths.push("icon".to_string());
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
        let _ = self.membership_log.forget(channel_id);

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
