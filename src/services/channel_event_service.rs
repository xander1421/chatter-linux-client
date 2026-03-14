//! Channel event service for handling member events and system messages.
//!
//! Centralizes system message creation for member join/leave events,
//! separating business logic from UI handlers.

use std::sync::Arc;

use chrono::Utc;

use crate::domain::channel::Membership;
use crate::domain::ids::{ChannelId, MessageId, UserId};
use crate::domain::message::{Message, MessageContent, SystemMessage};
use crate::error::StorageError;
use crate::storage::repositories::channel_repo::ChannelRepository;
use crate::storage::repositories::message_repo::MessageRepository;
use crate::storage::repositories::user_repo::UserRepository;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::channel_client::ChannelClient;
use crate::proto::chat_v1::channel_event;
use crate::events::EventBus;
use crate::inbox::DomainEvent;
use crate::error::NetworkError;
use tokio_stream::StreamExt;

#[derive(Debug, thiserror::Error)]
pub enum ChannelEventError {
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("stream error: {0}")]
    Stream(String),
    #[error("grpc error: {0}")]
    Grpc(#[from] tonic::Status),
    #[error("network error: {0}")]
    Network(#[from] NetworkError),
}

/// Service for handling channel events (member join/leave) and creating system messages.
/// 
/// This service now owns the gRPC stream and handles all reconnection logic,
/// converting proto events to domain events for the UI to consume.
pub struct ChannelEventService {
    message_repo: Arc<MessageRepository>,
    channel_repo: Arc<ChannelRepository>,
    user_repo: Arc<UserRepository>,
    grpc_pool: Arc<GrpcClientPool>,
    event_bus: EventBus,
}

impl ChannelEventService {
    /// Create a new ChannelEventService.
    pub fn new(
        message_repo: Arc<MessageRepository>,
        channel_repo: Arc<ChannelRepository>,
        user_repo: Arc<UserRepository>,
        grpc_pool: Arc<GrpcClientPool>,
        event_bus: EventBus,
    ) -> Self {
        Self {
            message_repo,
            channel_repo,
            user_repo,
            grpc_pool,
            event_bus,
        }
    }
    
    /// Start the channel event stream.
    ///
    /// This spawns a background task that owns the gRPC stream,
    /// handles reconnection logic, and emits domain events.
    pub fn start_event_stream(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                match self.run_stream_with_retry().await {
                    Ok(()) => {
                        tracing::info!("Channel event stream ended normally");
                        break;
                    }
                    Err(e) => {
                        tracing::warn!("Channel event stream error: {}. Reconnecting in 5s...", e);
                        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    }
                }
            }
        })
    }
    
    /// Run the event stream with automatic retry on failure.
    async fn run_stream_with_retry(&self) -> Result<(), ChannelEventError> {
        let mut channel_client = ChannelClient::new(
            self.grpc_pool.channel(),
            self.grpc_pool.auth_clone(),
        );
        
        // Stream events for all channels
        let mut stream = channel_client
            .stream_channel_events(None)
            .await?;
        
        tracing::info!("Channel event stream started");
        
        while let Some(result) = stream.next().await {
            match result {
                Ok(event) => {
                    if let Some(event_data) = event.event {
                        self.handle_stream_event(
                            event_data,
                            &event.channel_id,
                            event.timestamp,
                        ).await;
                    }
                }
                Err(e) => {
                    tracing::error!("Channel event stream error: {}", e);
                    return Err(ChannelEventError::Stream(e.to_string()));
                }
            }
        }
        
        Ok(())
    }
    
    /// Handle a channel event from the stream.
    async fn handle_stream_event(
        &self,
        event: channel_event::Event,
        channel_id: &str,
        _timestamp: u64,
    ) {
        use channel_event::Event;
        use crate::domain::channel::{MemberRole, NotificationPreference};
        
        let channel_id = match channel_id.parse::<uuid::Uuid>() {
            Ok(uuid) => ChannelId::from(uuid),
            Err(e) => {
                tracing::error!("Invalid channel ID {}: {}", channel_id, e);
                return;
            }
        };
        
        match event {
            Event::MemberAdded(member) => {
                let user_id = match member.user_id.parse::<uuid::Uuid>() {
                    Ok(uuid) => UserId::from(uuid),
                    Err(e) => {
                        tracing::error!("Invalid user ID {}: {}", member.user_id, e);
                        return;
                    }
                };
                
                let membership = Membership {
                    channel_id,
                    user_id,
                    role: MemberRole::Member,
                    notification_preference: NotificationPreference::All,
                    joined_at: Utc::now(),
                    last_read_message_id: None,
                    is_muted: false,
                    muted_until: None,
                };
                
                if let Ok((message, display_name)) = self.handle_member_joined(&channel_id, &user_id, &membership) {
                    // Emit domain event for UI consumption
                    let event = DomainEvent::ChannelMemberJoined {
                        channel_id,
                        user_id,
                        display_name: display_name.clone(),
                        message,
                    };
                    
                    if let Err(e) = self.event_bus.domain_sender().send(event) {
                        tracing::warn!("Failed to publish ChannelMemberJoined event: {}", e);
                    }
                    
                    tracing::info!(
                        "Channel member joined: channel={}, user={}, display_name={}",
                        channel_id,
                        user_id,
                        display_name
                    );
                }
            }
            Event::MemberRemovedId(user_id_str) => {
                let user_id = match user_id_str.parse::<uuid::Uuid>() {
                    Ok(uuid) => UserId::from(uuid),
                    Err(e) => {
                        tracing::error!("Invalid user ID {}: {}", user_id_str, e);
                        return;
                    }
                };
                
                if let Ok((message, display_name)) = self.handle_member_removed(&channel_id, &user_id) {
                    // Emit domain event for UI consumption
                    let event = DomainEvent::ChannelMemberLeft {
                        channel_id,
                        user_id,
                        display_name: display_name.clone(),
                        message,
                    };
                    
                    if let Err(e) = self.event_bus.domain_sender().send(event) {
                        tracing::warn!("Failed to publish ChannelMemberLeft event: {}", e);
                    }
                    
                    tracing::info!(
                        "Channel member left: channel={}, user={}, display_name={}",
                        channel_id,
                        user_id,
                        display_name
                    );
                }
            }
            _ => {
                tracing::debug!("Unhandled channel event type");
            }
        }
    }

    /// Handle a member joining a channel.
    ///
    /// Creates a system message and syncs membership to local storage.
    /// Returns (message, display_name) for UI updates.
    pub fn handle_member_joined(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
        membership: &Membership,
    ) -> Result<(Message, String), ChannelEventError> {
        // Get display name
        let display_name = self.get_display_name(user_id);

        // Create system message
        let message = self.create_system_message(
            channel_id,
            SystemMessage::UserJoined(*user_id),
        );

        // Persist message
        self.message_repo.insert(&message)?;

        // Sync membership (insert or ignore if already exists)
        self.channel_repo.insert_or_ignore_membership(membership)?;

        Ok((message, display_name))
    }

    /// Handle a member leaving/being removed from a channel.
    ///
    /// Creates a system message and removes membership from local storage.
    /// Returns (message, display_name) for UI updates.
    pub fn handle_member_removed(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
    ) -> Result<(Message, String), ChannelEventError> {
        let display_name = self.get_display_name(user_id);

        let message = self.create_system_message(
            channel_id,
            SystemMessage::UserLeft(*user_id),
        );

        self.message_repo.insert(&message)?;

        // Remove membership from local storage
        self.channel_repo.delete_membership(channel_id, user_id)?;

        Ok((message, display_name))
    }

    /// Get display name for a user, falling back to user ID string.
    fn get_display_name(&self, user_id: &UserId) -> String {
        match self.user_repo.get(user_id) {
            Ok(Some(user)) => user.username,
            Ok(None) => user_id.to_string(),
            Err(e) => {
                tracing::warn!(
                    user_id = %user_id,
                    error = %e,
                    "Failed to get user for display name, falling back to ID"
                );
                user_id.to_string()
            }
        }
    }

    /// Create a system message for the given channel.
    fn create_system_message(&self, channel_id: &ChannelId, content: SystemMessage) -> Message {
        let now = Utc::now();
        Message {
            id: MessageId::new(),
            channel_id: *channel_id,
            sender_id: None, // System messages have no sender
            content: MessageContent::System(content),
            thread_id: None,
            reply_to: None,
            attachments: Vec::new(),
            is_pinned: false,
            is_edited: false,
            is_deleted: false,
            sent_at: 0, // System messages don't use sender timestamp
            created_at: now,
            updated_at: now,
            edited_at: None,
            reaction_counts: Vec::new(),
            current_user_reactions: Vec::new(),
            mentioned_integrations: Vec::new(),
            targets_ai_bot: false,
            delivery_status: String::new(),
        }
    }
}
