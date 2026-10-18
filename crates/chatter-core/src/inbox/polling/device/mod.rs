//! Device inbox message processing.
//!
//! Handles processing messages from the device inbox including:
//! - Sender key encrypted channel messages
//! - Sealed sender messages
//! - Control content (typing, reactions, pins, edits, deletes)

use std::sync::Arc;

use parking_lot::RwLock;
use prost::Message as ProstMessage;
use sha2::Digest;
use tokio::sync::broadcast;

use crate::domain::UserId;
use crate::domain::ids::{AttachmentId, ChannelId, MessageId};
use crate::domain::message::Message;
use crate::error::{CryptoError, ServiceError};
use crate::inbox::events::InboxEvent;
use crate::inbox::handlers::DomainEvent;
use crate::inbox::state::InboxStats;
use crate::mailbox::group::GroupCoordinator;
use crate::network::validation::{MessageValidationPipeline, PipelineResult};
use crate::proto::chat_v1::{DeviceMessage, EncryptedData, SealedSenderMessage};
use crate::proto::content_v1::{Content, DataMessage, content};
use crate::proto::mailbox_v1::EnvelopeType;
use crate::services::gateway::{MessageGateway, RouteResult, SealedSenderResult};
use crate::storage::repositories::ephemeral_repo::EphemeralRepository;
use crate::storage::{ChannelRepository, MessageRepository};

mod control;
mod ecies;
mod failure;
mod identified;
mod route;
mod sealed;
mod sender_key;

pub use control::dispatch_control_content;
pub use failure::{delivery_issue_message, identity_changed_message};
pub use route::process_device_message;

pub(super) use failure::desync_marker_target;
pub(crate) use failure::{
    EciesInitFailure, blocked_sender_decision, device_failure_decision, ecies_init_decision,
    sender_key_failure_decision, sender_key_missing_decision,
};

use control::{dispatch_secret_chat_bootstrap, persist_exact_saved_delete_before_ack};
use ecies::process_ecies_init_message;
use identified::process_identified_sender_message;
use sealed::process_sealed_sender_message;
use sender_key::process_sender_key_message;
