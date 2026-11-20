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
use crate::services::dm_bootstrap_service::DmBootstrapService;
use crate::services::message::MessageService;
use crate::services::signal::SignalService;
use crate::services::user::UserService;
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
    auth_state: crate::state::AuthState,
    #[cfg(feature = "media")]
    call_service: Arc<std::sync::OnceLock<crate::services::CallWorkerHandle>>,
    /// The façade that owns this processor, held strongly for the processor's
    /// whole life (as the `ChatterClient` it replaces was).
    facade: Arc<dyn InboxFacade>,
}

/// The façade operations the processor needs and cannot reach through a
/// service: they go through `ChatterClient` (implemented in `client/`).
pub(crate) trait InboxFacade: Send + Sync {
    /// Owe a delivery receipt for a received friend DM (`ServerDm`): accepted
    /// senders only, batched and sent after a random delay
    /// (`client::delivery_receipt_batch`).
    fn send_dm_delivery_receipt(
        self: Arc<Self>,
        peer_id: UserId,
        timestamp: u64,
    ) -> futures::future::BoxFuture<'static, Result<(), String>>;
    /// Bring up call services before an incoming ring is presented.
    #[cfg(feature = "media")]
    fn ensure_call_service(&self);
    /// Drop a ringing (not yet accepted) group-call invite.
    #[cfg(feature = "media")]
    fn drop_ringing_group_call_invite(&self, call_id: &str);
}

mod anonymous_dm;
mod anonymous_group;
mod messages;
mod presence;
mod protocol;
pub(crate) mod sync;

impl InboxEventProcessor {
    /// Create from the services it uses and the façade that owns it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        message_service: Arc<MessageService>,
        dm_bootstrap_service: Arc<DmBootstrapService>,
        anon_dm_service: Arc<AnonDmService>,
        anon_group_service: Arc<AnonGroupService>,
        signal_service: Arc<SignalService>,
        user_service: Arc<UserService>,
        attachment_service: Arc<AttachmentService>,
        event_bus: EventBus,
        auth_state: crate::state::AuthState,
        #[cfg(feature = "media")] call_service: Arc<
            std::sync::OnceLock<crate::services::CallWorkerHandle>,
        >,
        facade: Arc<dyn InboxFacade>,
    ) -> Self {
        // Durable reaction intents already in this profile must not wait for
        // network redelivery merely because the target is already stored.
        crate::inbox::reaction_handoff::replay(
            &message_service.message_repo,
            Some(&event_bus.domain_sender()),
        );
        Self {
            message_service,
            dm_bootstrap_service,
            anon_dm_service,
            anon_group_service,
            signal_service,
            user_service,
            attachment_service,
            event_bus,
            auth_state,
            #[cfg(feature = "media")]
            call_service,
            facade,
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
            | InboxEvent::SavedDeletionApplied { .. }
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
