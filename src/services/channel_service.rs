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
use crate::proto::chat_v1::{self, channel_event};
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
}
