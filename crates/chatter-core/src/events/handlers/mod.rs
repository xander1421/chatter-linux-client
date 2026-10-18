//! Event handlers for domain event to state synchronization.
//!
//! Bridges domain events from inbox handlers to reactive state stores,
//! enabling the UI to stay in sync with incoming data.
//!
//! # Architecture
//!
//! ```text
//! +------------------+     +----------------+     +---------------+
//! | Inbox Handlers   | --> | EventBus       | --> | EventHandlers |
//! | (emit DomainEvent)|    | (broadcast)    |     | (dispatch)    |
//! +------------------+     +----------------+     +---------------+
//!                                                        |
//!                                                        v
//!                                                 +--------------+
//!                                                 | State Stores |
//!                                                 | - MessageState
//!                                                 | - ChatState  (pinned + streaming substates)
//!                                                 | - etc.
//!                                                 +--------------+
//! ```
//!
//! # Usage
//!
//! ```ignore
//! // Create state stores
//! let message_state = MessageState::new();
//! let chat_state = ChatState::new();
//! // ... etc
//!
//! // Create event handlers with all state stores
//! let handlers = EventHandlers::new(
//!     message_state,
//!     chat_state,
//! );
//!
//! // Spawn the event loop
//! let handle = handlers.spawn(event_bus.clone());
//!
//! // The handler loop runs until the bus is dropped
//! ```

use std::sync::Arc;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tracing::{debug, trace, warn};

use crate::events::bus::EventBus;
use crate::inbox::handlers::DomainEvent;
use crate::mailbox::group::GroupCoordinator;
use crate::state::{ChatState, MessageState};

/// Event handlers that bridge domain events to state stores.
///
/// Subscribes to the EventBus domain channel and dispatches events to
/// appropriate state store update methods.
///
/// # Thread Safety
///
/// EventHandlers holds Clone-able references to all state stores.
/// The spawn() method creates a background tokio task that processes events.
/// Multiple EventHandlers instances can be created (e.g., for testing),
/// but typically only one should be spawned per application.
#[derive(Clone)]
pub struct EventHandlers {
    /// Weak publication channel: emitting derived events must not keep the bus alive.
    domain_tx: Option<broadcast::WeakSender<DomainEvent>>,
    /// Message cache and draft state.
    message_state: MessageState,
    /// Chat-side state: active conversation + pinned-bar + streaming ephemera.
    chat_state: ChatState,
    /// Group mailbox coordinator for NDSS '21 per-group mailboxes.
    group_coordinator: Option<Arc<GroupCoordinator>>,
    /// Message service for DB persistence of edits/deletes.
    message_service: Option<Arc<crate::services::message::MessageService>>,
    /// Anon group service for profile sharing after member list updates.
    anon_group_service: Option<Arc<crate::services::anon_group::AnonGroupService>>,
    /// Anon DM service for conversation creation from ECIES init messages.
    anon_dm_service: Option<Arc<crate::services::anon_dm::AnonDmService>>,
    /// Signal service for sender key rotation on member departure.
    signal_service: Option<Arc<crate::services::signal::SignalService>>,
}

impl EventHandlers {
    /// Create a new EventHandlers with the state stores it actually reads.
    ///
    /// # Arguments
    ///
    /// * `message_state` - Message cache and draft state
    /// * `chat_state`    - Active conversation + pinned-bar + streaming ephemera
    #[must_use]
    pub fn new(message_state: MessageState, chat_state: ChatState) -> Self {
        Self {
            domain_tx: None,
            message_state,
            chat_state,
            group_coordinator: None,
            message_service: None,
            anon_group_service: None,
            anon_dm_service: None,
            signal_service: None,
        }
    }

    /// Set the message service for DB persistence.
    pub fn with_message_service(
        mut self,
        service: Arc<crate::services::message::MessageService>,
    ) -> Self {
        self.message_service = Some(service);
        self
    }

    /// Set the group coordinator for handling NDSS '21 mailbox events.
    ///
    /// Called after construction when GroupCoordinator is available.
    pub fn with_group_coordinator(mut self, coordinator: Arc<GroupCoordinator>) -> Self {
        self.group_coordinator = Some(coordinator);
        self
    }

    /// Set the anon group service for profile sharing after member list updates.
    pub fn with_anon_group_service(
        mut self,
        service: Arc<crate::services::anon_group::AnonGroupService>,
    ) -> Self {
        self.anon_group_service = Some(service);
        self
    }

    /// Set the anon DM service for conversation creation from ECIES init messages.
    pub fn with_anon_dm_service(
        mut self,
        service: Arc<crate::services::anon_dm::AnonDmService>,
    ) -> Self {
        self.anon_dm_service = Some(service);
        self
    }

    /// Set the signal service for sender key rotation on member departure.
    pub fn with_signal_service(
        mut self,
        service: Arc<crate::services::signal::SignalService>,
    ) -> Self {
        self.signal_service = Some(service);
        self
    }

    /// Spawn the event handler loop as a background task.
    ///
    /// Creates an async loop that:
    /// 1. Subscribes to the EventBus domain channel
    /// 2. Receives domain events
    /// 3. Dispatches each event to `handle_domain_event()`
    ///
    /// The loop runs until the EventBus is dropped or all senders are closed.
    ///
    /// # Returns
    ///
    /// A JoinHandle that can be used to await or abort the background task.
    pub fn spawn(&self, event_bus: EventBus) -> JoinHandle<()> {
        let mut handlers = self.clone();
        handlers.domain_tx = Some(event_bus.domain_sender().downgrade());
        let mut rx = event_bus.subscribe_domain();

        tokio::spawn(async move {
            debug!("EventHandlers: starting domain event loop");

            loop {
                match rx.recv().await {
                    Ok(event) => {
                        trace!(?event, "EventHandlers: received domain event");
                        handlers.handle_domain_event(event);
                    }
                    Err(broadcast::error::RecvError::Lagged(count)) => {
                        warn!(
                            count,
                            "EventHandlers: lagged behind, dropped {} events", count
                        );
                        // Continue processing - we'll catch up
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        debug!("EventHandlers: domain channel closed, shutting down");
                        break;
                    }
                }
            }
        })
    }
}

mod anonymous;
mod channels;
mod key_exchange;
mod messages;
mod notifications;
#[cfg(test)]
mod tests;

impl EventHandlers {
    /// Handle a single domain event by dispatching to the appropriate state store.
    /// This method maps each DomainEvent variant to the corresponding state
    /// store update method. It is synchronous to allow easy testing.
    pub fn handle_domain_event(&self, event: DomainEvent) {
        match event {
            event @ DomainEvent::MessageReceived { .. } => self.handle_messages_event(event),
            event @ DomainEvent::MessageEdited { .. } => self.handle_messages_event(event),
            event @ DomainEvent::MessageDeleted { .. } => self.handle_messages_event(event),
            event @ DomainEvent::TypingStarted { .. } => self.handle_messages_event(event),
            event @ DomainEvent::TypingStopped { .. } => self.handle_messages_event(event),
            event @ DomainEvent::MessageDeliveryChanged { .. } => self.handle_messages_event(event),
            event @ DomainEvent::MessagesRead { .. } => self.handle_messages_event(event),
            event @ DomainEvent::MessagesDelivered { .. } => self.handle_messages_event(event),
            event @ DomainEvent::ViewedReceipt { .. } => self.handle_messages_event(event),
            event @ DomainEvent::ReactionAdded { .. } => self.handle_messages_event(event),
            event @ DomainEvent::ReactionRemoved { .. } => self.handle_messages_event(event),
            event @ DomainEvent::ReactionSnapshotChanged { .. } => {
                self.handle_messages_event(event)
            }
            event @ DomainEvent::MessagePinned { .. } => self.handle_messages_event(event),
            event @ DomainEvent::MessageUnpinned { .. } => self.handle_messages_event(event),
            event @ DomainEvent::AttachmentReady { .. } => self.handle_messages_event(event),
            event @ DomainEvent::DeleteForMeSync { .. } => self.handle_messages_event(event),
            event @ DomainEvent::SavedMessageSynced { .. } => self.handle_messages_event(event),
            event @ DomainEvent::DmBootstrapCompleted { .. } => self.handle_messages_event(event),
            event @ DomainEvent::MessageSendFailed { .. } => self.handle_messages_event(event),
            event @ DomainEvent::KeyExchangeReceived { .. } => self.handle_channels_event(event),
            event @ DomainEvent::SenderKeyReceived { .. } => self.handle_channels_event(event),
            event @ DomainEvent::GroupMailboxReceived { .. } => self.handle_channels_event(event),
            event @ DomainEvent::ChannelMemberJoined { .. } => self.handle_channels_event(event),
            event @ DomainEvent::ChannelMemberLeft { .. } => self.handle_channels_event(event),
            event @ DomainEvent::ChannelUpdated { .. } => self.handle_channels_event(event),
            event @ DomainEvent::ChannelMemberUpdated { .. } => self.handle_channels_event(event),
            event @ DomainEvent::ChannelCreated { .. } => self.handle_channels_event(event),
            event @ DomainEvent::ChannelArchivedChanged { .. } => self.handle_channels_event(event),
            event @ DomainEvent::ChannelModerationAction { .. } => {
                self.handle_channels_event(event)
            }
            event @ DomainEvent::ChannelInviteReceived { .. } => self.handle_channels_event(event),
            event @ DomainEvent::ChannelInviteResponded { .. } => self.handle_channels_event(event),
            event @ DomainEvent::ChannelOperationFailed { .. } => self.handle_channels_event(event),
            event @ DomainEvent::ChannelMembershipNotice { .. } => {
                self.handle_channels_event(event)
            }
            event @ DomainEvent::DmSessionHealed { .. } => self.handle_key_exchange_event(event),
            event @ DomainEvent::PeerIdentityChanged { .. } => self.handle_channels_event(event),
            event @ DomainEvent::PeerDevicesChanged { .. } => self.handle_channels_event(event),
            event @ DomainEvent::AnonGroupInviteReceived { .. } => {
                self.handle_anonymous_event(event)
            }
            event @ DomainEvent::AnonGroupJoined { .. } => self.handle_anonymous_event(event),
            event @ DomainEvent::AnonGroupMemberJoined { .. } => self.handle_anonymous_event(event),
            event @ DomainEvent::AnonGroupMemberLeft { .. } => self.handle_anonymous_event(event),
            event @ DomainEvent::AnonGroupMemberKicked { .. } => self.handle_anonymous_event(event),
            event @ DomainEvent::AnonGroupBootstrapComplete { .. } => {
                self.handle_anonymous_event(event)
            }
            event @ DomainEvent::AnonGroupMemberListReceived { .. } => {
                self.handle_anonymous_event(event)
            }
            event @ DomainEvent::AnonDmRequestReceived { .. } => self.handle_anonymous_event(event),
            event @ DomainEvent::AnonDmRequestAccepted { .. } => self.handle_anonymous_event(event),
            event @ DomainEvent::AnonDmEnded { .. } => self.handle_anonymous_event(event),
            event @ DomainEvent::AnonDmDeclined { .. } => self.handle_anonymous_event(event),
            event @ DomainEvent::AnonDmInitReceived { .. } => self.handle_anonymous_event(event),
            event @ DomainEvent::AnonDmProfileUpdated { .. } => self.handle_anonymous_event(event),
            event @ DomainEvent::DeviceLinked { .. } => self.handle_notifications_event(event),
            event @ DomainEvent::CallHangupReceived { .. } => {
                self.handle_notifications_event(event)
            }
            event @ DomainEvent::CallKeyAnswerReceived { .. } => {
                self.handle_notifications_event(event)
            }
            event @ DomainEvent::GroupCallSignalReceived { .. } => {
                self.handle_notifications_event(event)
            }
            event @ DomainEvent::CallKeyOfferReceived { .. } => {
                self.handle_notifications_event(event)
            }
            event @ DomainEvent::FriendRequestsUpdated { .. } => {
                self.handle_notifications_event(event)
            }
            event @ DomainEvent::FriendAccepted { .. } => self.handle_notifications_event(event),
            event @ DomainEvent::AnonGroupUpdated { .. } => self.handle_notifications_event(event),
            event @ DomainEvent::UserPresenceChanged { .. } => {
                self.handle_notifications_event(event)
            }
            event @ DomainEvent::UserProfileUpdated { .. } => {
                self.handle_notifications_event(event)
            }
            event @ DomainEvent::SyncComplete { .. } => self.handle_notifications_event(event),
            event @ DomainEvent::MissedCallsChanged => self.handle_notifications_event(event),
            event @ DomainEvent::ConversationsRead { .. } => self.handle_notifications_event(event),
            event @ DomainEvent::PushWakeup { .. } => self.handle_notifications_event(event),
        }
    }
}

impl std::fmt::Debug for EventHandlers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventHandlers")
            .field("message_state", &self.message_state)
            .finish_non_exhaustive()
    }
}
