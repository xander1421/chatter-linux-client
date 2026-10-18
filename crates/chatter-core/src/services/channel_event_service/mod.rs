//! Channel event service for handling member events and system messages.
//!
//! Centralizes system message creation for member join/leave events,
//! separating business logic from UI handlers.

use std::sync::Arc;

use chrono::Utc;

use crate::domain::channel::Membership;
use crate::domain::ids::{ChannelId, MessageId, UserId};
use crate::domain::message::{Message, MessageContent, SystemMessage};
use crate::error::NetworkError;
use crate::error::StorageError;
use crate::events::EventBus;
use crate::inbox::DomainEvent;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::streaming::heartbeat::{self, Next};
use crate::proto::chat_v1::channel_event;
use crate::services::channel::membership_log::{
    Applied, LogObserver, MembershipLog, MembershipNotice,
};
use crate::services::current_user::CurrentUserProvider;
use crate::storage::repositories::MessageRepository;
use crate::storage::repositories::channel_repo::ChannelRepository;
use crate::storage::repositories::user_repo::UserRepository;

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
    /// Needed to tell "this moderation action targets ME" from "it targets
    /// somebody else" — a ban on us has to drop our own membership.
    settings_repo: Arc<crate::storage::repositories::settings_repo::SettingsRepository>,
    /// Pending invites the UI has already been told about, so a reconnect
    /// refetch announces only the ones it missed.
    announced_invites: parking_lot::Mutex<std::collections::HashSet<ChannelId>>,
    /// Verifies and commits every relayed `ChannelChange`; tells this
    /// service (as its [`LogObserver`]) what to display.
    membership_log: Arc<MembershipLog>,
}

mod membership;
mod observer;
mod stream;

#[cfg(test)]
mod tests;

impl ChannelEventService {
    /// Create a new ChannelEventService.
    pub fn new(
        message_repo: Arc<MessageRepository>,
        channel_repo: Arc<ChannelRepository>,
        user_repo: Arc<UserRepository>,
        grpc_pool: Arc<GrpcClientPool>,
        event_bus: EventBus,
        settings_repo: Arc<crate::storage::repositories::settings_repo::SettingsRepository>,
        membership_log: Arc<MembershipLog>,
    ) -> Self {
        Self {
            message_repo,
            channel_repo,
            user_repo,
            grpc_pool,
            event_bus,
            settings_repo,
            announced_invites: Default::default(),
            membership_log,
        }
    }
}

/// Why a member dropped off the roster: the signed `RemoveMember.reason`, or
/// what a roster sync can still learn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberRemoval {
    Left,
    Kicked(UserId),
    Banned(UserId),
    /// Gone from the roster and no verified signed change says why (an
    /// unsigned server removal, or a log the sync could not read). Reads
    /// "X is no longer a member" / "You are no longer a member".
    Unexplained,
}
