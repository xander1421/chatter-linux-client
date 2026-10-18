//! Inbox event processor - handles protocol-level InboxEvent concerns.
//!
//! Handles the protocol side of every InboxEvent. Anything the UI must see
//! leaves here as a `DomainEvent` on the event bus — nothing reads a return
//! value, so "the UI will also process it" is not a thing this layer can say.

use std::sync::Arc;

use crate::domain::UserId;
use crate::events::EventBus;
use crate::inbox::events::InboxEvent;
use crate::inbox::handlers::DomainEvent;
use crate::services::anon_dm::AnonDmService;
use crate::services::anon_group::AnonGroupService;
use crate::services::attachment::AttachmentService;
use crate::services::conversation::ConversationHandle;
use crate::services::dm_bootstrap_service::DmBootstrapService;
use crate::services::message::MessageService;
use crate::services::signal::SignalService;
use crate::services::user::UserService;
use crate::state::ActiveConversation;
use crate::util::time::TimestampNanos;

/// Processes inbox events and handles protocol-level concerns.
pub struct InboxEventProcessor {
    message_service: Arc<MessageService>,
    dm_bootstrap_service: Arc<DmBootstrapService>,
    anon_dm_service: Arc<AnonDmService>,
    anon_group_service: Arc<AnonGroupService>,
    signal_service: Arc<SignalService>,
    user_service: Arc<UserService>,
    attachment_service: Arc<AttachmentService>,
    event_bus: EventBus,
    /// ChatterClient for ConversationHandle construction.
    client: Arc<crate::client::ChatterClient>,
}

mod anonymous_dm;
mod anonymous_group;
mod messages;
mod presence;
mod protocol;
pub(crate) mod sync;

impl InboxEventProcessor {
    /// Create from a CoreServices (convenience constructor).
    pub fn from_client(client: &Arc<crate::client::ChatterClient>) -> Self {
        // Durable reaction intents already in this profile must not wait for
        // network redelivery merely because the target is already stored.
        crate::inbox::reaction_handoff::replay(
            &client.messages().message_repo,
            Some(&client.event_bus().domain_sender()),
        );
        Self {
            message_service: client.messages().clone(),
            dm_bootstrap_service: client.dm_bootstrap().clone(),
            anon_dm_service: client.anon_dms().clone(),
            anon_group_service: client.anon_groups().clone(),
            signal_service: client.signal().clone(),
            user_service: client.users().clone(),
            attachment_service: client.attachments().clone(),
            event_bus: client.event_bus().clone(),
            client: client.clone(),
        }
    }

    /// Process an inbox event. The UI hears about it only through the
    /// `DomainEvent`s emitted here.
    pub async fn process(&self, event: &InboxEvent) {
        match event {
            InboxEvent::DmDecryptionFailed { .. }
            | InboxEvent::DmDecryptionErrorNeedsReport { .. }
            | InboxEvent::GroupResendRequested { .. }
            | InboxEvent::DmResendRequested { .. }
            | InboxEvent::DmMailboxExpired { .. }
            | InboxEvent::DmPollCompleted { .. }
            | InboxEvent::MessageReceived { .. }
            | InboxEvent::MessageProcessed { .. }
            | InboxEvent::MessageFailed { .. }
            | InboxEvent::PollCompleted { .. }
            | InboxEvent::PollFailed { .. }
            | InboxEvent::AcknowledgeFailed { .. }
            | InboxEvent::GroupMailboxPollCompleted { .. }
            | InboxEvent::GroupMailboxAnnouncement { .. }
            | InboxEvent::CallKeyAnswerReceived { .. }
            | InboxEvent::CallKeyOfferReceived { .. }
            | InboxEvent::GroupCallSignalReceived { .. }
            | InboxEvent::SenderKeyNeeded { .. }
            | InboxEvent::RespondWithSkdm { .. } => self.process_protocol(event).await,
            InboxEvent::AnonGroupInvite { .. }
            | InboxEvent::AnonGroupJoinAck { .. }
            | InboxEvent::AnonGroupMemberList { .. }
            | InboxEvent::AnonGroupLeave { .. }
            | InboxEvent::AnonGroupKick { .. }
            | InboxEvent::AnonGroupUpdate { .. }
            | InboxEvent::AnonGroupMessage { .. } => self.process_anonymous_group(event).await,
            InboxEvent::AnonDmRequest { .. }
            | InboxEvent::AnonDmAccept { .. }
            | InboxEvent::AnonDmMessage { .. } => self.process_anonymous_dm(event).await,
            InboxEvent::DmReceived { .. }
            | InboxEvent::ChannelMessageReceived { .. }
            | InboxEvent::CallHangup { .. }
            | InboxEvent::DmBootstrapCompleted { .. }
            | InboxEvent::DmReaction { .. }
            | InboxEvent::ChannelReaction { .. }
            | InboxEvent::DmEdit { .. }
            | InboxEvent::ChannelEdit { .. }
            | InboxEvent::DmDeleteForEveryone { .. }
            | InboxEvent::ChannelDeleteForEveryone { .. }
            | InboxEvent::DmPin { .. }
            | InboxEvent::ChannelCallEvent { .. }
            | InboxEvent::ChannelPin { .. } => self.process_messages(event).await,
            InboxEvent::SavedMessageSync { .. }
            | InboxEvent::SavedSnapshotApplied { .. }
            | InboxEvent::ReadSync { .. }
            | InboxEvent::ProfileKeySync { .. }
            | InboxEvent::ConfigurationSync { .. }
            | InboxEvent::ChannelDeleteForMe { .. }
            | InboxEvent::ChannelReceipt { .. }
            | InboxEvent::ChannelReceiptTargetsSynced { .. } => self.process_sync(event).await,
            InboxEvent::DmTyping { .. }
            | InboxEvent::ChannelTyping { .. }
            | InboxEvent::DmReceipt { .. }
            | InboxEvent::FriendRequestsUpdated { .. }
            | InboxEvent::FriendAccepted { .. }
            | InboxEvent::ProfileKeyReceived { .. } => self.process_presence(event).await,
            // All other events have no protocol work.
            _ => {}
        }
    }
}
