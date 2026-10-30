//! Inbound path for server-delivered channel messages.
//!
//! One place decrypts, persists and announces a `chat_v1::Message` that the
//! server handed us for a server-managed channel. Two callers share it:
//!
//! - the live event stream (`services::channel_stream`), and
//! - the durable catch-up fetch (`sync::message_sync`, `GetMessages`).
//!
//! They must agree, or a message reads correctly when it arrives live and as
//! the literal string "[Encrypted message]" when it arrives through a backfill
//! — which is exactly what happened before this module existed: the sync path
//! called `proto_to_domain_message_with_sender` directly, and that maps
//! `Content::Encrypted` to a placeholder.

use std::sync::Arc;

use prost::Message as _;

use crate::domain::ids::{ChannelId, UserId};
use crate::domain::message::Message;
use crate::events::EventBus;
use crate::inbox::DomainEvent;
use crate::proto::chat_v1;
use crate::proto::content_v1::{self, GroupEncrypted};
use crate::services::attachment::AttachmentService;

use super::conversion::{conversation_type_for_channel, proto_to_domain_message_with_sender};
use super::{MessageError, MessageService};

mod edit;
mod persist;
mod prepare;
mod resync;

use edit::is_newer_edit;
pub(crate) use persist::Persisted;
pub(crate) use prepare::DecryptFailureGate;
#[cfg(test)]
use prepare::{RESEND_GIVE_UP_ATTEMPTS, record_decrypt_failure};
#[cfg(test)]
use resync::ensure_resync_ingested;

/// Outcome of ingesting one server-delivered channel message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ingested {
    /// Stored (or updated) and announced on the domain bus.
    Stored,
    /// Our own message, already rendered optimistically at send time.
    OwnMessage,
    /// Parked waiting for the sender's key distribution message.
    DeferredPendingSenderKey,
    /// Could not be used at all (bad proto, undecryptable, conversion failed).
    Dropped,
    /// Given up for good (a peer's gets a delivery-issue marker). Terminal:
    /// it must not hold a sync checkpoint or a stream resync.
    DeliveryIssue,
    /// Storage or recoverable decrypt failure; stream must not acknowledge it.
    RetryableFailure,
}

/// Shared decrypt → persist → announce path for channel messages.
pub struct ChannelInbound {
    messages: Arc<MessageService>,
    attachments: Arc<AttachmentService>,
    users: Arc<crate::services::user::UserService>,
}

impl ChannelInbound {
    /// Shared blocked-sender decision for live, catch-up, resync and key retry.
    /// Send-time intervals make catch-up agree with live receipt after an unblock.
    pub(crate) fn drop_blocked_sender(
        messages: &MessageService,
        proto_msg: &chat_v1::Message,
        channel_id: ChannelId,
    ) -> Result<bool, crate::error::StorageError> {
        if proto_msg.channel_id.parse::<ChannelId>().ok() != Some(channel_id) {
            return Ok(false);
        }
        let Ok(sender) = proto_msg.author_id.parse::<UserId>() else {
            return Ok(false);
        };
        let me = messages
            .settings_repo
            .current_user_id()
            .map_err(|error| crate::error::StorageError::InvalidData(error.to_string()))?;
        if sender == me {
            return Ok(false);
        }
        let anon = crate::storage::repositories::anon_dm_repo::AnonDmRepository::new(
            messages.database.clone(),
        );
        let blocked = messages.user_repo.is_blocked(&me, &sender)?
            || messages.user_repo.was_blocked_at(&me, &sender, proto_msg.timestamp)?
            || anon.is_user_blocked(&sender)?;
        Ok(blocked)
    }

    pub fn new(
        messages: Arc<MessageService>,
        attachments: Arc<AttachmentService>,
        users: Arc<crate::services::user::UserService>,
    ) -> Self {
        Self {
            messages,
            attachments,
            users,
        }
    }

    fn event_bus(&self) -> &EventBus {
        &self.messages.event_bus
    }

    /// Decrypt, persist and announce one channel message.
    ///
    /// `live` skips this device's optimistic sends. Every newly inserted row
    /// announces once, including catch-up: otherwise a buffered live duplicate
    /// is discarded after catch-up and an open chat never learns of the row.
    pub async fn ingest(
        &self,
        proto_msg: &chat_v1::Message,
        channel_id: ChannelId,
        live: bool,
    ) -> Ingested {
        // THIS device's own messages are skipped only on the LIVE path, where
        // they are already on screen from the optimistic send. On a backfill they must
        // still be stored, or a reinstall loses this user's own half of the
        // conversation. (An own message in an ENCRYPTED channel still drops
        // out below — we cannot decrypt our own sender-key ciphertext — which
        // is what the message-backup path exists for.)
        let mut message = match self.prepare(proto_msg, channel_id, live).await {
            Ok(m) => m,
            Err(outcome) => return outcome,
        };

        let Some(persisted) = self.persist(&mut message, channel_id) else {
            return Ingested::RetryableFailure;
        };

        if persisted == Persisted::New {
            let _ = self
                .event_bus()
                .domain_sender()
                .send(DomainEvent::MessageReceived { message });
        }

        Ingested::Stored
    }
}

#[cfg(test)]
mod replay_failure_tests;
#[cfg(test)]
mod resync_acknowledgement_tests;
#[cfg(test)]
mod tests;
