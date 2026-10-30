//! DM mailbox polling.
//!
//! Handles polling and processing messages from DM (direct message) mailboxes
//! using two-way anonymous messaging.

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::crypto::envelope_crypto::decrypt_envelope;
use crate::domain::UserId;
use crate::domain::ids::{AttachmentId, ChannelId, MessageId};
use crate::domain::message::{ConversationType, Message};
use crate::error::MailboxError;
use crate::inbox::ack_policy::{self, AckDecision};
use crate::inbox::events::InboxEvent;
use crate::inbox::handlers::DomainEvent;
use crate::mailbox::dm::{DmConversationManager, TwoWayAnonymity};
use crate::network::grpc::AnonymousMailboxClient;
use crate::network::validation::MessageDeduplicator;
use crate::services::attachment::AttachmentService;
use crate::services::gateway::{DmContent, MessageGateway};
use crate::storage::repositories::settings_repo::keys;
use crate::storage::repositories::user_repo::UserRepository;
use crate::storage::{AttachmentRepository, MessageRepository, SettingsRepository};

mod bootstrap;
mod dispatch;
mod policy;
mod poll;

use bootstrap::{process_bootstrap_key_exchange, try_parse_bootstrap_response};
pub(crate) use dispatch::dispatch_dm_content;
pub(crate) use policy::dm_failure_decision;
use policy::{addressed_to_another_device, local_recipient_address};
pub use poll::poll_dm_mailboxes;

#[cfg(test)]
mod tests;
