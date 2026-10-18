//! DM bootstrap operations.
//!
//! Handles initial DM conversation setup using the Sesame algorithm
//! from the NDSS '21 paper. The lower UUID always initiates the bootstrap.

use std::sync::Arc;

use prost::Message as ProstMessage;

use crate::domain::ids::{AttachmentId, ChannelId, UserId};
use crate::mailbox::dm::{DmBootstrap, release_bootstrap_lock, try_acquire_bootstrap_lock};
use crate::network::grpc::AnonymousMailboxClient;
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::proto::chat_v1::{SealedDelivery, SealedDeviceMessage, SealedSenderMessage};
use crate::services::current_user::CurrentUserProvider;
use crate::util::time::TimestampNanos;

use super::{MessageError, MessageService};

mod delivery;
mod devices;
mod entry;
mod prewarm;

use delivery::bootstrap_request;

#[cfg(test)]
mod tests;
