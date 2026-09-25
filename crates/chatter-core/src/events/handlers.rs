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
    signal_service: Option<Arc<crate::services::signal_service::SignalService>>,
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
    pub fn with_message_service(mut self, service: Arc<crate::services::message::MessageService>) -> Self {
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
    pub fn with_anon_group_service(mut self, service: Arc<crate::services::anon_group::AnonGroupService>) -> Self {
        self.anon_group_service = Some(service);
        self
    }

    /// Set the anon DM service for conversation creation from ECIES init messages.
    pub fn with_anon_dm_service(mut self, service: Arc<crate::services::anon_dm::AnonDmService>) -> Self {
        self.anon_dm_service = Some(service);
        self
    }

    /// Set the signal service for sender key rotation on member departure.
    pub fn with_signal_service(mut self, service: Arc<crate::services::signal_service::SignalService>) -> Self {
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
        let handlers = self.clone();
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

    /// Handle a single domain event by dispatching to the appropriate state store.
    ///
    /// This method maps each DomainEvent variant to the corresponding state
    /// store update method. It is synchronous to allow easy testing.
    pub fn handle_domain_event(&self, event: DomainEvent) {
        match event {
            // ================================================================
            // Message Events
            // ================================================================
            DomainEvent::MessageReceived { message } => {
                let channel_id = message.conversation_id;
                trace!(?channel_id, message_id = ?message.id, "Adding message to state");
                self.message_state.append_message(channel_id, message);
            }

            DomainEvent::MessageEdited {
                message_id,
                new_content,
                channel_id,
                ..  // conversation_type, target_sent_timestamp
            } => {
                trace!(?channel_id, ?message_id, "Updating message content in state + DB");
                self.message_state
                    .update_message(&channel_id, &message_id, |msg| {
                        msg.content = new_content.clone();
                        msg.is_edited = true;
                    });
                // Persist to DB so is_edited survives reload
                if let Some(ref svc) = self.message_service {
                    if let Err(e) = svc.update_message_body(&message_id, &new_content) {
                        tracing::warn!(?message_id, error = %e, "Failed to persist edit to DB");
                    }
                }
            }

            DomainEvent::MessageDeleted {
                message_id,
                author_id,
                channel_id,
                ..
            } => {
                tracing::info!(
                    ?message_id,
                    ?author_id,
                    ?channel_id,
                    "Inbox event: MessageDeleted received"
                );
                // Remove from in-memory state
                if let Some(ch_id) = channel_id {
                    self.message_state.remove_message(&ch_id, &message_id);
                } else if let Some(ch_id) = self.message_state.find_channel_for_message(&message_id) {
                    self.message_state.remove_message(&ch_id, &message_id);
                }
                // Persist deletion to DB
                if let Some(ref svc) = self.message_service {
                    if let Err(e) = svc.delete_message_local(&message_id) {
                        tracing::warn!(?message_id, error = %e, "Failed to persist delete to DB");
                    }
                }
            }

            // ================================================================
            // Typing Events
            // ================================================================
            DomainEvent::TypingStarted {
                user_id,
                channel_id,
                timestamp,
            } => {
                if self.chat_state.accept_typing(channel_id, user_id, timestamp, true) {
                    trace!(?channel_id, ?user_id, "Adding typing user to state");
                    self.add_typing_user(channel_id, user_id);
                } else {
                    trace!(?channel_id, ?user_id, "Ignoring STARTED older than its STOPPED");
                }
            }

            DomainEvent::TypingStopped {
                user_id,
                channel_id,
                timestamp,
            } => {
                trace!(?channel_id, ?user_id, "Removing typing user from state");
                self.chat_state.accept_typing(channel_id, user_id, timestamp, false);
                self.remove_typing_user(channel_id, user_id);
            }

            // ================================================================
            // Receipt Events
            // ================================================================
            DomainEvent::MessagesRead {
                reader_id,
                message_timestamps,
                receipt_timestamp,
                channel_id,
            } => {
                // Log the read receipt event with channel context
                trace!(
                    ?reader_id,
                    ?channel_id,
                    count = message_timestamps.len(),
                    receipt_timestamp,
                    "Read receipt received"
                );
                // Persist read status to DB
                self.message_state
                    .update_delivery_status_by_timestamps(&message_timestamps, "read");
                if let Some(ref svc) = self.message_service {
                    for ts in &message_timestamps {
                        // Receipt may lack conversation context; use lookup-based update
                        // to avoid unscoped WHERE sent_at = ? across conversations.
                        if let Err(e) = svc.update_delivery_status_by_timestamp_with_lookup(*ts, "read") {
                            tracing::trace!(timestamp = ts, error = %e, "Failed to persist read status");
                        }
                    }
                }
            }

            DomainEvent::MessagesDelivered {
                recipient_id,
                message_timestamps,
                receipt_timestamp,
            } => {
                trace!(
                    ?recipient_id,
                    count = message_timestamps.len(),
                    receipt_timestamp,
                    "Delivery receipt received"
                );
                self.message_state
                    .update_delivery_status_by_timestamps(&message_timestamps, "delivered");
                // Persist delivery status to DB — receipts lack conversation context,
                // so use lookup-based update (SELECT by timestamp, UPDATE by ID).
                if let Some(ref svc) = self.message_service {
                    for ts in &message_timestamps {
                        if let Err(e) = svc.update_delivery_status_by_timestamp_with_lookup(*ts, "delivered") {
                            tracing::trace!(timestamp = ts, error = %e, "Failed to persist delivery status");
                        }
                    }
                }
            }

            DomainEvent::ViewedReceipt {
                message_timestamp,
                from_user_id,
                receipt_timestamp,
            } => {
                trace!(
                    message_timestamp,
                    ?from_user_id,
                    receipt_timestamp,
                    "Viewed receipt received"
                );
                self.message_state
                    .update_delivery_status_by_timestamps(&[message_timestamp], "read");
                if let Some(ref svc) = self.message_service {
                    // ViewedReceipt lacks conversation context; use lookup-based update.
                    let _ = svc.update_delivery_status_by_timestamp_with_lookup(message_timestamp, "read");
                }
            }

            // ================================================================
            // Reaction Events
            // ================================================================
            DomainEvent::ReactionAdded {
                sender_id,
                sent_at_ns,
                reactor_id,
                emoji,
                ..
            } => {
                let cache_key = crate::domain::MessageId::from_sender_timestamp(&sender_id, sent_at_ns);
                tracing::info!(
                    ?sender_id,
                    sent_at_ns,
                    ?reactor_id,
                    %emoji,
                    "Inbox event: ReactionAdded received"
                );
                // Storage persistence (and the dedup gate) lives in
                // `inbox_processor` (regular DM / channel) and `base_anon`
                // (anon DM / group). Those layers only emit
                // `DomainEvent::ReactionAdded` when the unified-reactions
                // row was newly inserted, so this listener can update the
                // in-memory cache unconditionally without double-counting.
                self.chat_state
                    .update_reaction(&cache_key, emoji.clone(), 1, None);
            }

            DomainEvent::ReactionRemoved {
                sender_id,
                sent_at_ns,
                reactor_id,
                emoji,
                ..
            } => {
                let cache_key = crate::domain::MessageId::from_sender_timestamp(&sender_id, sent_at_ns);
                tracing::info!(
                    ?sender_id,
                    sent_at_ns,
                    ?reactor_id,
                    %emoji,
                    "Inbox event: ReactionRemoved received"
                );
                // See ReactionAdded: dedup happens upstream in inbox_processor
                // / base_anon, so the in-memory cache update can be unconditional.
                self.chat_state
                    .update_reaction(&cache_key, emoji.clone(), -1, None);
            }

            // ================================================================
            // Pin Events
            // ================================================================
            DomainEvent::MessagePinned {
                message_id,
                channel_id,
                pinner_id,
                target_sent_timestamp: _,
            } => {
                tracing::info!(
                    ?message_id,
                    ?channel_id,
                    ?pinner_id,
                    "Inbox event: MessagePinned received"
                );
                
                self.message_state
                    .update_message(&channel_id, &message_id, |msg| {
                        msg.is_pinned = true;
                    });
                self.chat_state.add_pinned_message(message_id);
                if let Some(ref svc) = self.message_service {
                    if let Err(e) = svc.set_message_pinned(&message_id, true) {
                        tracing::warn!(?message_id, error = %e, "Failed to persist pin to DB");
                    }
                }
            }

            DomainEvent::MessageUnpinned {
                message_id,
                channel_id,
                unpinner_id,
                target_sent_timestamp: _,
            } => {
                tracing::info!(
                    ?message_id,
                    ?channel_id,
                    ?unpinner_id,
                    "Inbox event: MessageUnpinned received"
                );
                
                self.message_state
                    .update_message(&channel_id, &message_id, |msg| {
                        msg.is_pinned = false;
                    });
                if let Some(ref svc) = self.message_service {
                    if let Err(e) = svc.set_message_pinned(&message_id, false) {
                        tracing::warn!(?message_id, error = %e, "Failed to persist unpin to DB");
                    }
                }
                
                // Remove from pinned state
                self.chat_state.remove_pinned_message(&message_id);
                
                tracing::debug!(?channel_id, ?message_id, "Marked message as unpinned in state and DB");
            }

            // ================================================================
            // Attachment Events
            // ================================================================
            DomainEvent::AttachmentReady {
                attachment,
                message_id,
                channel_id,
            } => {
                trace!(?channel_id, ?message_id, attachment_id = ?attachment.id, "Attachment ready");
                // Attachment records are created by InboxEventProcessor::create_from_pointers()
                // before the UI handler runs. Download is triggered by UI's trigger_channel_attachment_download().
            }

            // ================================================================
            // Key Exchange Events
            // ================================================================
            DomainEvent::KeyExchangeReceived {
                sender_id,
                conversation_id,
                ..
            } => {
                // Key exchange is handled by crypto layer, not state
                trace!(?sender_id, ?conversation_id, "Key exchange received (handled by crypto layer)");
            }

            DomainEvent::SenderKeyReceived {
                sender_id,
                channel_id,
                ..
            } => {
                // Sender key is processed in the polling layer before this event is emitted.
                trace!(?sender_id, ?channel_id, "Sender key received (already processed)");
            }

            DomainEvent::GroupMailboxReceived {
                sender_id,
                channel_id,
                mailbox_public_key,
                timestamp,
            } => {
                // Process NDSS '21 mailbox announcement bundled with SKDM
                if let Some(ref coordinator) = self.group_coordinator {
                    if let Err(e) = coordinator.process_mailbox_key(
                        sender_id,
                        &channel_id,
                        mailbox_public_key,
                        timestamp,
                    ) {
                        warn!(?sender_id, ?channel_id, error = %e, "Failed to process group mailbox from SKDM");
                    } else {
                        trace!(?sender_id, ?channel_id, "Processed group mailbox from SKDM (NDSS '21)");
                    }
                } else {
                    trace!(?sender_id, ?channel_id, "GroupMailboxReceived event ignored (no coordinator)");
                }
            }

            // ================================================================
            // Sync Events
            // ================================================================
            DomainEvent::DeleteForMeSync {
                conversation_id,
                target_message_id,
                ..
            } => {
                trace!(?conversation_id, ?target_message_id, "DeleteForMeSync: removing message locally");
                self.message_state
                    .remove_message(&conversation_id, &target_message_id);
                // Persist deletion to DB
                if let Some(ref svc) = self.message_service {
                    if let Err(e) = svc.delete_message_local(&target_message_id) {
                        tracing::warn!(?target_message_id, error = %e, "Failed to persist delete-for-me to DB");
                    }
                }
            }

            DomainEvent::SavedMessageSynced { message } => {
                let channel_id = message.conversation_id;
                trace!(?channel_id, message_id = ?message.id, "SavedMessageSynced: adding to state");
                self.message_state.append_message(channel_id, message);
            }

            DomainEvent::DeviceLinked {
                device_id,
                device_name,
                approved_by_self,
                ..
            } => {
                // UI thread surfaces a toast / refreshes Devices modal via
                // event bus subscription. State stores have nothing to update
                // here — the device list lives on the server and is fetched
                // on demand by the modal.
                trace!(
                    device_id,
                    device_name = ?device_name,
                    approved_by_self,
                    "DeviceLinked: handled by UI thread"
                );
            }

            // ================================================================
            // Call Events
            // ================================================================
            DomainEvent::CallHangupReceived { call_id, peer_id, .. } => {
                // Call hangup is handled by the UI thread in calls.rs via event bus subscription.
                // No state store updates needed here.
                trace!(?call_id, ?peer_id, "CallHangupReceived: handled by UI thread");
            }

            DomainEvent::CallKeyAnswerReceived { call_id, peer_id, .. } => {
                // Call key answer is handled by the UI thread in calls.rs via event bus subscription.
                // No state store updates needed here.
                trace!(?call_id, ?peer_id, "CallKeyAnswerReceived: handled by UI thread");
            }

            DomainEvent::GroupCallKeyReceived { call_id, sender_id, key_epoch, .. } => {
                // Group call key is handled by the UI thread in calls.rs via event bus subscription.
                // No state store updates needed here.
                trace!(?call_id, ?sender_id, key_epoch, "GroupCallKeyReceived: handled by UI thread");
            }

            // ================================================================
            // Anonymous Group Events
            // ================================================================
            DomainEvent::AnonGroupInviteReceived { group_id, group_name, inviter_id, .. } => {
                // Anon group invite is handled by UI thread via event bus subscription.
                trace!(?group_id, ?group_name, ?inviter_id, "AnonGroupInviteReceived: handled by UI thread");
            }

            DomainEvent::AnonGroupJoined { group_id } => {
                // Anon group joined is handled by UI thread via event bus subscription.
                trace!(?group_id, "AnonGroupJoined: handled by UI thread");
            }

            DomainEvent::AnonGroupMemberJoined { group_id, user_id, .. } => {
                // Anon group member joined is handled by UI thread via event bus subscription.
                trace!(?group_id, ?user_id, "AnonGroupMemberJoined: handled by UI thread");
            }

            DomainEvent::AnonGroupMemberLeft { group_id, user_id } => {
                // Anon group member left is handled by UI thread via event bus subscription.
                trace!(?group_id, ?user_id, "AnonGroupMemberLeft: handled by UI thread");
            }

            DomainEvent::AnonGroupMemberKicked { group_id, user_id, kicked_by } => {
                // Anon group member kicked is handled by UI thread via event bus subscription.
                trace!(?group_id, ?user_id, ?kicked_by, "AnonGroupMemberKicked: handled by UI thread");
            }


            DomainEvent::AnonGroupBootstrapComplete { group_id } => {
                // Anon group bootstrap complete is handled by UI thread via event bus subscription.
                trace!(?group_id, "AnonGroupBootstrapComplete: handled by UI thread");
            }

            DomainEvent::AnonGroupMemberListReceived { group_id, ref members } => {
                // Profile sharing is NOT triggered here — Signal Protocol model:
                // profiles are shared once at join time (invite + JoinAck), and the
                // owner's member list broadcast already includes all profile data.
                // Re-sharing on every member list update is unnecessary and wasteful.
                trace!(?group_id, member_count = members.len(), "AnonGroupMemberListReceived: UI handles display refresh");
            }

            // ================================================================
            // Anonymous DM Events
            // ================================================================
            DomainEvent::AnonDmRequestReceived { conversation_id, sender_id, .. } => {
                // Anon DM request is handled by UI thread via event bus subscription.
                trace!(?conversation_id, ?sender_id, "AnonDmRequestReceived: handled by UI thread");
            }

            DomainEvent::AnonDmRequestAccepted { conversation_id, peer_id, .. } => {
                // Anon DM acceptance is handled by UI thread via event bus subscription.
                trace!(?conversation_id, ?peer_id, "AnonDmRequestAccepted: handled by UI thread");
            }


            DomainEvent::AnonDmInitReceived {
                conversation_id,
                sender_id,
                sender_display_name,
                sender_avatar,
                peer_mailbox_pk,
                peer_identity_key,
                peer_delivery_token,
                ref intro_message,
                ..
            } => {
                // NDSS'21 ECIES init — create conversation in DB (was previously in UI)
                if let Some(ref svc) = self.anon_dm_service {
                    let dm = crate::domain::anon_dm::AnonDm::from_request(
                        conversation_id, sender_id, peer_mailbox_pk,
                    ).with_display_name(&sender_display_name)
                     .with_avatar_opt(sender_avatar);

                    if let Err(e) = svc.create_incoming_conversation(
                        &dm, &peer_identity_key, &peer_delivery_token,
                    ) {
                        warn!(
                            conversation_id = %conversation_id,
                            error = %e,
                            "Failed to create anon DM conversation from ECIES init"
                        );
                    } else {
                        debug!(
                            conversation_id = %conversation_id,
                            sender_id = %sender_id,
                            "Created anon DM conversation from ECIES init"
                        );
                    }
                } else {
                    trace!(?conversation_id, intro_len = intro_message.len(), "AnonDmInitReceived: no anon_dm_service configured");
                }
            }


            DomainEvent::AnonCallOfferReceived {
                conversation_id, sender_id, call_room_id, room_name,
                call_type, caller_display_name, call_encryption_key, ..
            } => {
                // Store pending call state so it's ready when UI accepts/declines
                if let Some(ref svc) = self.anon_dm_service {
                    let mut call_room_id_array = [0u8; 16];
                    if call_room_id.len() >= 16 {
                        call_room_id_array.copy_from_slice(&call_room_id[..16]);
                    } else {
                        call_room_id_array[..call_room_id.len()].copy_from_slice(&call_room_id);
                    }
                    let pending = crate::services::anon_dm::PendingAnonCall {
                        call_room_id: call_room_id_array,
                        room_name: room_name.clone(),
                        conversation_id,
                        token: crate::mailbox::CallRoomToken {
                            id: String::new(), public_key: vec![], private_key: vec![],
                            signature: vec![], randomizer: None,
                        },
                        is_video_call: call_type == 2,
                        is_outgoing: false,
                        display_name: caller_display_name,
                        encryption_key: call_encryption_key,
                        sender_id: Some(sender_id),
                        started_at: None,
                    };
                    svc.store_pending_anon_call(room_name.clone(), pending);
                    debug!(?conversation_id, ?room_name, "Stored pending anon call offer");
                }
            }

            DomainEvent::AnonCallAnswerReceived { conversation_id, accepted, .. } => {
                // Anonymous call answer - handled by UI thread for call state.
                trace!(?conversation_id, ?accepted, "AnonCallAnswerReceived: handled by UI thread");
            }

            DomainEvent::AnonCallHangupReceived { conversation_id, reason, .. } => {
                // Anonymous call hangup - handled by UI thread for ending call.
                trace!(?conversation_id, ?reason, "AnonCallHangupReceived: handled by UI thread");
            }

            DomainEvent::AnonGroupCallOfferReceived {
                group_id, sender_id, call_room_id, room_name,
                call_type, caller_display_name, call_encryption_key, ..
            } => {
                // Store pending call state (reuse anon DM pending call infra)
                if let Some(ref svc) = self.anon_dm_service {
                    let mut call_room_id_array = [0u8; 16];
                    if call_room_id.len() >= 16 {
                        call_room_id_array.copy_from_slice(&call_room_id[..16]);
                    } else {
                        call_room_id_array[..call_room_id.len()].copy_from_slice(&call_room_id);
                    }
                    let conversation_id = crate::domain::anon_dm::AnonDmId::try_from_slice(
                        group_id.as_bytes()
                    ).expect("AnonGroupId is 16 bytes");
                    let pending = crate::services::anon_dm::PendingAnonCall {
                        call_room_id: call_room_id_array,
                        room_name: room_name.clone(),
                        conversation_id,
                        token: crate::mailbox::CallRoomToken {
                            id: String::new(), public_key: vec![], private_key: vec![],
                            signature: vec![], randomizer: None,
                        },
                        is_video_call: call_type == 2,
                        is_outgoing: false,
                        display_name: caller_display_name,
                        encryption_key: call_encryption_key,
                        sender_id: Some(sender_id),
                        started_at: None,
                    };
                    svc.store_pending_anon_call(room_name.clone(), pending);
                    debug!(?group_id, ?room_name, "Stored pending anon group call offer");
                }
            }

            DomainEvent::AnonGroupCallAnswerReceived { group_id, accepted, .. } => {
                trace!(?group_id, ?accepted, "AnonGroupCallAnswerReceived: handled by UI thread");
            }

            DomainEvent::AnonGroupCallHangupReceived { group_id, reason, .. } => {
                trace!(?group_id, ?reason, "AnonGroupCallHangupReceived: handled by UI thread");
            }

            // ================================================================
            // UI-Specific Events (handled by UI thread)
            // ================================================================
            DomainEvent::CallKeyOfferReceived { call_id, peer_id, .. } => {
                // Incoming call offer - handled by UI thread for call modal.
                trace!(?call_id, ?peer_id, "CallKeyOfferReceived: handled by UI thread");
            }

            DomainEvent::DmBootstrapCompleted { peer_id, sent_timestamps } => {
                // The queued bubbles went out: "sending" → "sent". The UI
                // thread also re-enables the composer.
                trace!(?peer_id, count = sent_timestamps.len(), "DmBootstrapCompleted");
                self.message_state
                    .update_delivery_status_by_timestamps(&sent_timestamps, "sent");
                if let Some(ref svc) = self.message_service {
                    if let Err(e) = svc.set_friend_dm_delivery_status(&peer_id.into(), &sent_timestamps, "sent") {
                        tracing::warn!(?peer_id, error = %e, "Failed to persist sent status");
                    }
                }
            }

            DomainEvent::FriendRequestsUpdated { ref received, ref sent } => {
                // Friend requests update - handled by UI thread for friend list.
                trace!(received_count = received.len(), sent_count = sent.len(), "FriendRequestsUpdated: handled by UI thread");
            }

            DomainEvent::FriendAccepted { friend_id } => {
                // Friend accepted - handled by UI thread to add DM to sidebar.
                trace!(?friend_id, "FriendAccepted: handled by UI thread");
            }

            DomainEvent::AnonGroupUpdated { group_id, updated_by, .. } => {
                // Anon group update - handled by UI thread for group settings.
                trace!(?group_id, ?updated_by, "AnonGroupUpdated: handled by UI thread");
            }

            // ================================================================
            // Local Send Events
            // ================================================================
            DomainEvent::MessageSendFailed { channel_id, error, message_timestamps } => {
                // Recorded as failed so the bubble says so after a reload too.
                trace!(?channel_id, ?error, "Message send failed");
                self.message_state
                    .update_delivery_status_by_timestamps(&message_timestamps, "failed");
                if let Some(ref svc) = self.message_service {
                    if let Err(e) = svc.set_friend_dm_delivery_status(&channel_id, &message_timestamps, "failed") {
                        tracing::warn!(?channel_id, error = %e, "Failed to persist failed status");
                    }
                }
            }

            // ================================================================
            // Channel Member Events
            // ================================================================
            DomainEvent::ChannelMemberJoined { channel_id, user_id, display_name, message } => {
                // Add system message to chat
                trace!(?channel_id, ?user_id, %display_name, "Member joined channel");
                self.message_state.append_message(channel_id, message);
            }

            DomainEvent::ChannelMemberLeft { channel_id, user_id, display_name, message } => {
                // Add system message to chat
                trace!(?channel_id, ?user_id, %display_name, "Member left channel");
                self.message_state.append_message(channel_id, message);

                // Mark channel for sender key rotation — the departed member had
                // copies of our sender key, so we MUST rotate before sending any
                // more messages. This matches Signal's behavior: rotate on ANY
                // member departure (voluntary or forced) for forward secrecy.
                if let Some(ref signal_svc) = self.signal_service {
                    if let Err(e) = signal_svc.mark_channel_for_rotation(&channel_id) {
                        tracing::error!(
                            channel_id = %channel_id,
                            departed_user = %user_id,
                            error = %e,
                            "Failed to mark channel for sender key rotation after member departure"
                        );
                    } else {
                        tracing::info!(
                            channel_id = %channel_id,
                            departed_user = %user_id,
                            "Marked channel for sender key rotation after member departure"
                        );
                    }
                }
            }

            // Channel metadata (name/icon) and member-role changes are persisted
            // to the channel repo by ChannelEventService before this event fires;
            // the UI reads channel name / member list from the repo, so there is
            // no in-memory state to mutate here.
            DomainEvent::ChannelUpdated { channel_id, name, .. } => {
                trace!(?channel_id, %name, "Channel metadata updated");
            }
            DomainEvent::ChannelMemberRoleChanged { channel_id, user_id, role } => {
                trace!(?channel_id, ?user_id, ?role, "Channel member role changed");
            }

            // Channel lifecycle (INFRA-29). ChannelEventService has already
            // written the authoritative repo state; only in-memory caches are
            // touched here.
            DomainEvent::ChannelDeleted { channel_id } => {
                tracing::info!(%channel_id, "Channel deleted; dropping cached messages");
                self.message_state.clear_channel(&channel_id);
            }
            DomainEvent::ChannelCreated { channel_id, ref name } => {
                trace!(?channel_id, %name, "Channel created");
            }
            DomainEvent::ChannelArchivedChanged { channel_id, archived } => {
                trace!(?channel_id, archived, "Channel archive state changed");
            }
            DomainEvent::ChannelModerationAction {
                channel_id, user_id, ref action, is_self, ..
            } => {
                if is_self && matches!(action.as_str(), "KICK" | "BAN") {
                    // We are out of this channel: stop serving its cached
                    // messages from memory.
                    tracing::info!(%channel_id, %action, "Removed from channel by moderation");
                    self.message_state.clear_channel(&channel_id);
                } else {
                    trace!(?channel_id, ?user_id, %action, "Channel moderation action");
                }
            }
            DomainEvent::ChannelInviteReceived { channel_id, inviter_id, .. } => {
                trace!(?channel_id, ?inviter_id, "Channel invitation received");
            }
            DomainEvent::ChannelInviteResponded { channel_id, inviter_id, accepted } => {
                trace!(?channel_id, ?inviter_id, accepted, "Channel invitation answered");
            }
            DomainEvent::ChannelOperationFailed { channel_id, ref operation_id, ref code, ref message } => {
                // Saga compensation: the server rolled the operation back and
                // ChannelEventService already re-read the channel. Log loudly —
                // this used to be discarded entirely, leaving optimistic state
                // wrong forever.
                tracing::warn!(
                    %channel_id, %operation_id, %code, %message,
                    "Channel operation failed server-side (rolled back)"
                );
            }

            // ================================================================
            // User Presence Events
            // ================================================================
            DomainEvent::UserPresenceChanged { user_id, presence, timestamp } => {
                // Presence updates are handled by the UI directly
                trace!(?user_id, ?presence, ?timestamp, "User presence changed");
                // TODO: Consider adding a presence state store if needed for caching
            }
            
            DomainEvent::AnonDmProfileUpdated { 
                conversation_id, 
                user_id, 
                display_name, 
                avatar,
                avatar_url, 
                bio, 
                timestamp 
            } => {
                // Profile updates for anonymous DMs
                trace!(
                    ?conversation_id, 
                    ?user_id, 
                    ?display_name,
                    has_avatar = avatar.is_some(),
                    ?avatar_url,
                    ?bio,
                    ?timestamp, 
                    "Anonymous DM profile updated"
                );
                
                // The profile update is already stored by the inbox handler
                // This event is just for notifying other components
            }

            DomainEvent::UserProfileUpdated {
                user_id,
                display_name,
                status_message,
                timestamp,
                ..
            } => {
                // Already persisted by the user events stream handler; UI subscribes
                // directly to refresh display name + avatar caches.
                trace!(
                    ?user_id,
                    ?display_name,
                    ?status_message,
                    ?timestamp,
                    "Server-managed user profile updated"
                );
            }

            // ================================================================
            // Jarvis AI Events
            // ================================================================
            DomainEvent::JarvisResponse { .. } => {
                #[cfg(feature = "jarvis")]
                {
                    // Jarvis events are handled by the UI directly
                }
                #[cfg(not(feature = "jarvis"))]
                {
                    tracing::warn!(target: "chatter::jarvis", "received JarvisResponse but jarvis feature disabled");
                }
            }
            DomainEvent::JarvisStats { .. } => {
                #[cfg(feature = "jarvis")]
                {
                    // Jarvis events are handled by the UI directly
                }
                #[cfg(not(feature = "jarvis"))]
                {
                    tracing::warn!(target: "chatter::jarvis", "received JarvisStats but jarvis feature disabled");
                }
            }
            DomainEvent::JarvisCleared { .. } => {
                #[cfg(feature = "jarvis")]
                {
                    // Jarvis events are handled by the UI directly
                }
                #[cfg(not(feature = "jarvis"))]
                {
                    tracing::warn!(target: "chatter::jarvis", "received JarvisCleared but jarvis feature disabled");
                }
            }

            // ================================================================
            // Sync Events
            // ================================================================
            DomainEvent::SyncComplete { .. } => {
                // Sync completion is handled by the UI directly
            }

            // ================================================================
            // Push Notifications
            // ================================================================
            DomainEvent::PushWakeup { .. } => {
                // Push wake-ups are consumed by the client (triggers sync) and
                // by the UI directly (e.g., to show a reconnect spinner).
                // No state-store update is required here.
            }

            // ================================================================
            // Signal-protocol sync events
            // ================================================================
            // Handled by the Signal service / inbox handlers that own the
            // ratchet + prekey stores. Nothing to mirror into UI state.
            DomainEvent::DmSessionHealed { .. }
            // The change is already parked in `pending_identity_changes`, which
            // is where the prompt reads it from; there is no state store to
            // mutate. The event exists so an OPEN conversation can raise the
            // prompt without waiting for a reload.
            | DomainEvent::PeerIdentityChanged { .. }
            | DomainEvent::PeerDevicesChanged { .. } => {}

            // IncomingCall surfaces as a desktop toast via
            // spawn_notification_listener; no state-store mutation here
            // (call UI subscribes to CallOrchestratorEvent directly for
            // in-app modal state).
            DomainEvent::IncomingCall { .. } => {}
        }
    }

    // ========================================================================
    // Helper Methods
    // ========================================================================

    /// Add a typing user to a channel's typing list.
    ///
    /// Helper that fetches current typing users, adds the new user if not present,
    /// and updates the streaming state.
    fn add_typing_user(
        &self,
        channel_id: crate::domain::ids::ChannelId,
        user_id: crate::domain::ids::UserId,
    ) {
        let mut typers = self.chat_state.get_typing_users(&channel_id);
        if !typers.contains(&user_id) {
            typers.push(user_id);
            self.chat_state.set_typing_users(channel_id, typers);
        }
    }

    /// Remove a typing user from a channel's typing list.
    ///
    /// Helper that fetches current typing users, removes the user if present,
    /// and updates the streaming state.
    fn remove_typing_user(
        &self,
        channel_id: crate::domain::ids::ChannelId,
        user_id: crate::domain::ids::UserId,
    ) {
        let mut typers = self.chat_state.get_typing_users(&channel_id);
        if let Some(pos) = typers.iter().position(|id| *id == user_id) {
            typers.remove(pos);
            self.chat_state.set_typing_users(channel_id, typers);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::{ChannelId, MessageId, UserId};
    use crate::domain::message::{ConversationType, Message, MessageContent};

    fn create_handlers() -> EventHandlers {
        EventHandlers::new(MessageState::new(), ChatState::new())
    }

    fn create_test_message(channel_id: ChannelId) -> Message {
        Message::new_text(channel_id, ConversationType::Channel, Some(UserId::new()), "Test message".to_string(), 0)
    }

    // ========================================================================
    // Message Event Tests
    // ========================================================================

    #[test]
    fn test_handle_message_received() {
        let handlers = create_handlers();
        let channel_id = ChannelId::new();
        let message = create_test_message(channel_id);
        let message_id = message.id;

        handlers.handle_domain_event(DomainEvent::MessageReceived {
            message: message.clone(),
        });

        let messages = handlers.message_state.get_messages(&channel_id);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, message_id);
    }

    #[test]
    fn test_handle_message_edited() {
        let handlers = create_handlers();
        let channel_id = ChannelId::new();
        let message = create_test_message(channel_id);
        let message_id = message.id;

        // First add the message
        handlers.handle_domain_event(DomainEvent::MessageReceived { message });

        // Then edit it
        let new_content = MessageContent::Text("Edited content".to_string());
        handlers.handle_domain_event(DomainEvent::MessageEdited {
            message_id,
            new_content: new_content.clone(),
            channel_id,
            conversation_type: ConversationType::Channel,
            author_id: UserId::new(),
            target_sent_timestamp: 0,
        });

        let messages = handlers.message_state.get_messages(&channel_id);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].is_edited);
        assert_eq!(messages[0].content, new_content);
    }

    #[test]
    fn test_handle_message_deleted() {
        let handlers = create_handlers();
        let channel_id = ChannelId::new();
        let message = create_test_message(channel_id);
        let message_id = message.id;
        let author_id = UserId::new();

        // First add the message
        handlers.handle_domain_event(DomainEvent::MessageReceived { message });
        assert_eq!(handlers.message_state.get_messages(&channel_id).len(), 1);

        // Then delete it
        handlers.handle_domain_event(DomainEvent::MessageDeleted {
            message_id,
            author_id,
            channel_id: Some(channel_id),
            target_sent_timestamp: 1704067200_000_000_000,
        });

        assert!(handlers.message_state.get_messages(&channel_id).is_empty());
    }

    // ========================================================================
    // Typing Event Tests
    // ========================================================================

    #[test]
    fn test_handle_typing_started() {
        let handlers = create_handlers();
        let channel_id = ChannelId::new();
        let user_id = UserId::new();

        handlers.handle_domain_event(DomainEvent::TypingStarted {
            user_id,
            channel_id,
            timestamp: 0,
        });

        let typers = handlers.chat_state.get_typing_users(&channel_id);
        assert_eq!(typers.len(), 1);
        assert!(typers.contains(&user_id));
    }

    #[test]
    fn test_handle_typing_stopped() {
        let handlers = create_handlers();
        let channel_id = ChannelId::new();
        let user_id = UserId::new();

        // Start typing
        handlers.handle_domain_event(DomainEvent::TypingStarted {
            user_id,
            channel_id,
            timestamp: 0,
        });
        assert_eq!(
            handlers.chat_state.get_typing_users(&channel_id).len(),
            1
        );

        // Stop typing
        handlers.handle_domain_event(DomainEvent::TypingStopped {
            user_id,
            channel_id,
            timestamp: 0,
        });
        assert!(handlers
            .chat_state
            .get_typing_users(&channel_id)
            .is_empty());
    }

    #[test]
    fn test_handle_typing_multiple_users() {
        let handlers = create_handlers();
        let channel_id = ChannelId::new();
        let user1 = UserId::new();
        let user2 = UserId::new();

        handlers.handle_domain_event(DomainEvent::TypingStarted {
            user_id: user1,
            channel_id,
            timestamp: 0,
        });
        handlers.handle_domain_event(DomainEvent::TypingStarted {
            user_id: user2,
            channel_id,
            timestamp: 0,
        });

        let typers = handlers.chat_state.get_typing_users(&channel_id);
        assert_eq!(typers.len(), 2);
        assert!(typers.contains(&user1));
        assert!(typers.contains(&user2));

        // Remove one
        handlers.handle_domain_event(DomainEvent::TypingStopped {
            user_id: user1,
            channel_id,
            timestamp: 0,
        });

        let typers = handlers.chat_state.get_typing_users(&channel_id);
        assert_eq!(typers.len(), 1);
        assert!(!typers.contains(&user1));
        assert!(typers.contains(&user2));
    }

    #[test]
    fn test_handle_typing_duplicate_start() {
        let handlers = create_handlers();
        let channel_id = ChannelId::new();
        let user_id = UserId::new();

        // Start typing twice (should not duplicate)
        handlers.handle_domain_event(DomainEvent::TypingStarted {
            user_id,
            channel_id,
            timestamp: 0,
        });
        handlers.handle_domain_event(DomainEvent::TypingStarted {
            user_id,
            channel_id,
            timestamp: 0,
        });

        let typers = handlers.chat_state.get_typing_users(&channel_id);
        assert_eq!(typers.len(), 1);
    }

    // ========================================================================
    // Reaction Event Tests
    // ========================================================================

    #[test]
    fn test_handle_reaction_added() {
        let handlers = create_handlers();
        let sender_id = UserId::new();
        let sent_at_ns: u64 = 1704067200_000_000_000;
        let reactor_id = UserId::new();

        handlers.handle_domain_event(DomainEvent::ReactionAdded {
            sender_id,
            sent_at_ns,
            reactor_id,
            emoji: "👍".to_string(),
            timestamp: 1234567890,
        });

        let cache_key = MessageId::from_sender_timestamp(&sender_id, sent_at_ns);
        let reactions = handlers.chat_state.get_reactions(&cache_key);
        assert!(reactions.is_some());
        let reactions = reactions.unwrap();
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].emoji, "👍");
        assert_eq!(reactions[0].count, 1);
    }

    #[test]
    fn test_handle_reaction_removed() {
        let handlers = create_handlers();
        let sender_id = UserId::new();
        let sent_at_ns: u64 = 1704067200_000_000_000;
        let reactor_id = UserId::new();

        // Add reaction
        handlers.handle_domain_event(DomainEvent::ReactionAdded {
            sender_id,
            sent_at_ns,
            reactor_id,
            emoji: "👍".to_string(),
            timestamp: 1234567890,
        });

        // Remove reaction
        handlers.handle_domain_event(DomainEvent::ReactionRemoved {
            sender_id,
            sent_at_ns,
            reactor_id,
            emoji: "👍".to_string(),
            timestamp: 1234567891,
        });

        // Should be removed entirely when count hits 0
        let cache_key = MessageId::from_sender_timestamp(&sender_id, sent_at_ns);
        let reactions = handlers.chat_state.get_reactions(&cache_key);
        assert!(reactions.is_none());
    }

    // ========================================================================
    // Pin Event Tests
    // ========================================================================

    #[test]
    fn test_handle_message_pinned() {
        let handlers = create_handlers();
        let channel_id = ChannelId::new();
        let message = create_test_message(channel_id);
        let message_id = message.id;
        let pinner_id = UserId::new();

        // Add message first
        handlers.handle_domain_event(DomainEvent::MessageReceived { message });

        // Pin it
        handlers.handle_domain_event(DomainEvent::MessagePinned {
            message_id,
            pinner_id,
            channel_id,
            target_sent_timestamp: 0,
        });

        // Check message state
        let messages = handlers.message_state.get_messages(&channel_id);
        assert!(messages[0].is_pinned);

        // Check pinned state
        assert_eq!(handlers.chat_state.pinned_total_count(), 1);
        assert_eq!(handlers.chat_state.pinned_current_message_id(), Some(message_id));
    }

    #[test]
    fn test_handle_message_unpinned() {
        let handlers = create_handlers();
        let channel_id = ChannelId::new();
        let message = create_test_message(channel_id);
        let message_id = message.id;
        let pinner_id = UserId::new();

        // Add and pin message
        handlers.handle_domain_event(DomainEvent::MessageReceived { message });
        handlers.handle_domain_event(DomainEvent::MessagePinned {
            message_id,
            pinner_id,
            channel_id,
            target_sent_timestamp: 0,
        });

        // Unpin it
        handlers.handle_domain_event(DomainEvent::MessageUnpinned {
            message_id,
            unpinner_id: pinner_id,
            channel_id,
            target_sent_timestamp: 0,
        });

        // Check message state
        let messages = handlers.message_state.get_messages(&channel_id);
        assert!(!messages[0].is_pinned);

        // Check pinned state
        assert_eq!(handlers.chat_state.pinned_total_count(), 0);
    }

    // ========================================================================
    // Sync Event Tests
    // ========================================================================

    #[test]
    fn test_handle_delete_for_me_sync() {
        let handlers = create_handlers();
        let channel_id = ChannelId::new();
        let message = create_test_message(channel_id);
        let message_id = message.id;
        let author_id = UserId::new();

        // Add message
        handlers.handle_domain_event(DomainEvent::MessageReceived { message });
        assert_eq!(handlers.message_state.get_messages(&channel_id).len(), 1);

        // Sync delete from another device
        handlers.handle_domain_event(DomainEvent::DeleteForMeSync {
            conversation_id: channel_id,
            target_author_id: author_id,
            target_message_id: message_id,
            timestamp: 1234567890,
        });

        // Message should be removed
        assert!(handlers.message_state.get_messages(&channel_id).is_empty());
    }

    // ========================================================================
    // Spawn Tests
    // ========================================================================

    #[tokio::test]
    async fn test_spawn_receives_events() {
        let handlers = create_handlers();
        let bus = EventBus::new();

        let _handle = handlers.spawn(bus.clone());

        let channel_id = ChannelId::new();
        let message = create_test_message(channel_id);

        // Give the spawned task time to subscribe
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Emit an event
        bus.emit_domain(DomainEvent::MessageReceived {
            message: message.clone(),
        });

        // Give time for event to be processed
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Check that the message was added to state
        let messages = handlers.message_state.get_messages(&channel_id);
        assert_eq!(messages.len(), 1);
    }

    #[tokio::test]
    async fn test_spawn_handles_bus_close() {
        let handlers = create_handlers();
        let bus = EventBus::new();

        let handle = handlers.spawn(bus.clone());

        // Drop the bus
        drop(bus);

        // The handler should complete without panic
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), handle).await;
        assert!(result.is_ok());
    }

    // ========================================================================
    // Debug Impl Test
    // ========================================================================

    #[test]
    fn test_debug_impl() {
        let handlers = create_handlers();
        let debug_str = format!("{:?}", handlers);
        assert!(debug_str.contains("EventHandlers"));
        assert!(debug_str.contains("message_state"));
    }
}
