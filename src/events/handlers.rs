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
//!                                                 | - StreamingState
//!                                                 | - PinnedState
//!                                                 | - etc.
//!                                                 +--------------+
//! ```
//!
//! # Usage
//!
//! ```ignore
//! // Create state stores
//! let message_state = MessageState::new();
//! let streaming_state = StreamingState::new();
//! // ... etc
//!
//! // Create event handlers with all state stores
//! let handlers = EventHandlers::new(
//!     message_state,
//!     chat_state,
//!     pinned_state,
//!     user_state,
//!     presence_state,
//!     streaming_state,
//!     offline_state,
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
use crate::state::{
    ChatState, MessageState, OfflineState, PinnedState, PresenceState, StreamingState, UserState,
};

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
    /// Channel/thread navigation state.
    #[allow(dead_code)]
    chat_state: ChatState,
    /// Pinned messages navigation state.
    pinned_state: PinnedState,
    /// User profile and relationship cache.
    #[allow(dead_code)]
    user_state: UserState,
    /// User presence status cache.
    #[allow(dead_code)]
    presence_state: PresenceState,
    /// Real-time streaming data (typing, reactions).
    streaming_state: StreamingState,
    /// Connection and offline queue status.
    #[allow(dead_code)]
    offline_state: OfflineState,
    /// Group mailbox coordinator for NDSS '21 per-group mailboxes.
    group_coordinator: Option<Arc<GroupCoordinator>>,
}

impl EventHandlers {
    /// Create a new EventHandlers with all state stores.
    ///
    /// # Arguments
    ///
    /// * `message_state` - Message cache and draft state
    /// * `chat_state` - Channel/thread navigation state
    /// * `pinned_state` - Pinned messages navigation state
    /// * `user_state` - User profile and relationship cache
    /// * `presence_state` - User presence status cache
    /// * `streaming_state` - Real-time streaming data (typing, reactions)
    /// * `offline_state` - Connection and offline queue status
    #[must_use]
    pub fn new(
        message_state: MessageState,
        chat_state: ChatState,
        pinned_state: PinnedState,
        user_state: UserState,
        presence_state: PresenceState,
        streaming_state: StreamingState,
        offline_state: OfflineState,
    ) -> Self {
        Self {
            message_state,
            chat_state,
            pinned_state,
            user_state,
            presence_state,
            streaming_state,
            offline_state,
            group_coordinator: None,
        }
    }

    /// Set the group coordinator for handling NDSS '21 mailbox events.
    ///
    /// Called after construction when GroupCoordinator is available.
    pub fn with_group_coordinator(mut self, coordinator: Arc<GroupCoordinator>) -> Self {
        self.group_coordinator = Some(coordinator);
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
                let channel_id = message.channel_id;
                trace!(?channel_id, message_id = ?message.id, "Adding message to state");
                self.message_state.append_message(channel_id, message);
            }

            DomainEvent::MessageEdited {
                message_id,
                new_content,
                channel_id,
            } => {
                trace!(?channel_id, ?message_id, "Updating message content in state");
                self.message_state
                    .update_message(&channel_id, &message_id, |msg| {
                        msg.content = new_content.clone();
                        msg.is_edited = true;
                    });
            }

            DomainEvent::MessageDeleted {
                message_id,
                author_id,
                channel_id,
            } => {
                tracing::info!(
                    ?message_id,
                    ?author_id,
                    ?channel_id,
                    "Inbox event: MessageDeleted received"
                );
                // For DM deletes, channel_id may be None - derive from author_id
                // (DM channel is the conversation with that author)
                if let Some(ch_id) = channel_id {
                    tracing::debug!(?ch_id, ?message_id, "Removing message from state");
                    self.message_state.remove_message(&ch_id, &message_id);
                } else {
                    // DM delete - we need to find the message by ID across all channels
                    // or derive DM channel from author. For now, try to find and remove.
                    tracing::debug!(
                        ?author_id,
                        ?message_id,
                        "DM delete: searching for message to remove"
                    );
                    // Try to find the channel containing this message
                    if let Some(ch_id) = self.message_state.find_channel_for_message(&message_id) {
                        self.message_state.remove_message(&ch_id, &message_id);
                        tracing::info!(?ch_id, ?message_id, "Removed deleted message from state");
                    } else {
                        tracing::warn!(
                            ?author_id,
                            ?message_id,
                            "Could not find channel for deleted message"
                        );
                    }
                }
            }

            // ================================================================
            // Typing Events
            // ================================================================
            DomainEvent::TypingStarted {
                user_id,
                channel_id,
            } => {
                trace!(?channel_id, ?user_id, "Adding typing user to state");
                self.add_typing_user(channel_id, user_id);
            }

            DomainEvent::TypingStopped {
                user_id,
                channel_id,
            } => {
                trace!(?channel_id, ?user_id, "Removing typing user from state");
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
                // Note: UI-level handling for anon DM read receipts is done
                // in bin_ui/features/modals/anon_dm_request.rs
            }

            DomainEvent::MessagesDelivered {
                recipient_id,
                message_timestamps,
                receipt_timestamp,
            } => {
                // TODO: Services layer should update per-message delivery receipts
                trace!(
                    ?recipient_id,
                    count = message_timestamps.len(),
                    receipt_timestamp,
                    "Delivery receipt received (not yet tracked in state)"
                );
            }

            DomainEvent::ViewedReceipt {
                message_timestamp,
                from_user_id,
                receipt_timestamp,
            } => {
                // TODO: Services layer should track view-once message views
                trace!(
                    message_timestamp,
                    ?from_user_id,
                    receipt_timestamp,
                    "Viewed receipt received (not yet tracked in state)"
                );
            }

            // ================================================================
            // Reaction Events
            // ================================================================
            DomainEvent::ReactionAdded {
                message_id,
                reactor_id,
                emoji,
                ..
            } => {
                tracing::info!(
                    ?message_id,
                    ?reactor_id,
                    %emoji,
                    "Inbox event: ReactionAdded received"
                );
                // Determine if current user reacted (would need local user ID context)
                // For now, assume not current user unless services layer provides that info
                self.streaming_state
                    .update_reaction(&message_id, emoji.clone(), 1, None);
                tracing::debug!(?message_id, %emoji, "Updated reaction count in streaming state (+1)");
            }

            DomainEvent::ReactionRemoved {
                message_id,
                reactor_id,
                emoji,
                ..
            } => {
                tracing::info!(
                    ?message_id,
                    ?reactor_id,
                    %emoji,
                    "Inbox event: ReactionRemoved received"
                );
                self.streaming_state
                    .update_reaction(&message_id, emoji.clone(), -1, None);
                tracing::debug!(?message_id, %emoji, "Updated reaction count in streaming state (-1)");
            }

            // ================================================================
            // Pin Events
            // ================================================================
            DomainEvent::MessagePinned {
                message_id,
                channel_id,
                pinner_id,
            } => {
                tracing::info!(
                    ?message_id,
                    ?channel_id,
                    ?pinner_id,
                    "Inbox event: MessagePinned received"
                );
                // Update message state to mark as pinned
                self.message_state
                    .update_message(&channel_id, &message_id, |msg| {
                        msg.is_pinned = true;
                    });
                // Add to pinned state
                self.pinned_state.add_pinned_message(message_id);
                tracing::debug!(?channel_id, ?message_id, "Marked message as pinned in state");
            }

            DomainEvent::MessageUnpinned {
                message_id,
                channel_id,
                unpinner_id,
            } => {
                tracing::info!(
                    ?message_id,
                    ?channel_id,
                    ?unpinner_id,
                    "Inbox event: MessageUnpinned received"
                );
                // Update message state to mark as unpinned
                self.message_state
                    .update_message(&channel_id, &message_id, |msg| {
                        msg.is_pinned = false;
                    });
                // Remove from pinned state
                self.pinned_state.remove_pinned_message(&message_id);
                tracing::debug!(?channel_id, ?message_id, "Marked message as unpinned in state");
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
                // TODO: Services layer should download and update message with attachment
                // The actual attachment download would be handled by a dedicated service
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
                // Sender key is processed in SenderKeyHandler before this event is emitted
                trace!(?sender_id, ?channel_id, "Sender key received (already processed by handler)");
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
                // Remove from local message cache (this syncs the deletion across devices)
                self.message_state
                    .remove_message(&conversation_id, &target_message_id);
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

            DomainEvent::AnonGroupMessageReceived { group_id, sender_id, .. } => {
                // Anon group message is handled by UI thread via event bus subscription.
                trace!(?group_id, ?sender_id, "AnonGroupMessageReceived: handled by UI thread");
            }

            DomainEvent::AnonGroupBootstrapComplete { group_id } => {
                // Anon group bootstrap complete is handled by UI thread via event bus subscription.
                trace!(?group_id, "AnonGroupBootstrapComplete: handled by UI thread");
            }

            DomainEvent::AnonGroupMemberListReceived { group_id, ref members } => {
                // Member list update - handled by UI thread.
                trace!(?group_id, member_count = members.len(), "AnonGroupMemberListReceived: handled by UI thread");
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

            DomainEvent::AnonDmMessageReceived { conversation_id, sender_id, .. } => {
                // Anon DM message is handled by UI thread via event bus subscription.
                trace!(?conversation_id, ?sender_id, "AnonDmMessageReceived: handled by UI thread");
            }

            DomainEvent::AnonDmInitReceived { conversation_id, intro_message, .. } => {
                // NDSS'21 ECIES init message - handled by anon DM service to create conversation.
                trace!(?conversation_id, intro_len = intro_message.len(), "AnonDmInitReceived: handled by anon DM service");
            }

            DomainEvent::AnonDmMessageDeleted { conversation_id, message_timestamp } => {
                // Anon DM message deletion - handled by UI thread.
                trace!(?conversation_id, ?message_timestamp, "AnonDmMessageDeleted: handled by UI thread");
            }

            DomainEvent::AnonDmMessageEdited { conversation_id, message_timestamp, .. } => {
                // Anon DM message edit - handled by UI thread.
                trace!(?conversation_id, ?message_timestamp, "AnonDmMessageEdited: handled by UI thread");
            }

            DomainEvent::AnonGroupMessageEdited { group_id, message_timestamp, .. } => {
                // Anon group message edit - handled by UI thread.
                trace!(?group_id, ?message_timestamp, "AnonGroupMessageEdited: handled by UI thread");
            }
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
        let mut typers = self.streaming_state.get_typing_users(&channel_id);
        if !typers.contains(&user_id) {
            typers.push(user_id);
            self.streaming_state.set_typing_users(channel_id, typers);
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
        let mut typers = self.streaming_state.get_typing_users(&channel_id);
        if let Some(pos) = typers.iter().position(|id| *id == user_id) {
            typers.remove(pos);
            self.streaming_state.set_typing_users(channel_id, typers);
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
    use crate::domain::message::{Message, MessageContent};

    fn create_test_stores() -> (
        MessageState,
        ChatState,
        PinnedState,
        UserState,
        PresenceState,
        StreamingState,
        OfflineState,
    ) {
        (
            MessageState::new(),
            ChatState::new(),
            PinnedState::new(),
            UserState::new(),
            PresenceState::new(),
            StreamingState::new(),
            OfflineState::new(),
        )
    }

    fn create_handlers() -> EventHandlers {
        let (msg, chat, pinned, user, presence, streaming, offline) = create_test_stores();
        // For tests, we don't have a GroupCoordinator, so it stays None
        EventHandlers::new(msg, chat, pinned, user, presence, streaming, offline)
    }

    fn create_test_message(channel_id: ChannelId) -> Message {
        Message::new_text(channel_id, Some(UserId::new()), "Test message".to_string(), 0)
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
        });

        let typers = handlers.streaming_state.get_typing_users(&channel_id);
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
        });
        assert_eq!(
            handlers.streaming_state.get_typing_users(&channel_id).len(),
            1
        );

        // Stop typing
        handlers.handle_domain_event(DomainEvent::TypingStopped {
            user_id,
            channel_id,
        });
        assert!(handlers
            .streaming_state
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
        });
        handlers.handle_domain_event(DomainEvent::TypingStarted {
            user_id: user2,
            channel_id,
        });

        let typers = handlers.streaming_state.get_typing_users(&channel_id);
        assert_eq!(typers.len(), 2);
        assert!(typers.contains(&user1));
        assert!(typers.contains(&user2));

        // Remove one
        handlers.handle_domain_event(DomainEvent::TypingStopped {
            user_id: user1,
            channel_id,
        });

        let typers = handlers.streaming_state.get_typing_users(&channel_id);
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
        });
        handlers.handle_domain_event(DomainEvent::TypingStarted {
            user_id,
            channel_id,
        });

        let typers = handlers.streaming_state.get_typing_users(&channel_id);
        assert_eq!(typers.len(), 1);
    }

    // ========================================================================
    // Reaction Event Tests
    // ========================================================================

    #[test]
    fn test_handle_reaction_added() {
        let handlers = create_handlers();
        let message_id = MessageId::new();
        let author_id = UserId::new();
        let reactor_id = UserId::new();

        handlers.handle_domain_event(DomainEvent::ReactionAdded {
            message_id,
            author_id,
            reactor_id,
            emoji: "👍".to_string(),
            timestamp: 1234567890,
        });

        let reactions = handlers.streaming_state.get_reactions(&message_id);
        assert!(reactions.is_some());
        let reactions = reactions.unwrap();
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].emoji, "👍");
        assert_eq!(reactions[0].count, 1);
    }

    #[test]
    fn test_handle_reaction_removed() {
        let handlers = create_handlers();
        let message_id = MessageId::new();
        let author_id = UserId::new();
        let reactor_id = UserId::new();

        // Add reaction
        handlers.handle_domain_event(DomainEvent::ReactionAdded {
            message_id,
            author_id,
            reactor_id,
            emoji: "👍".to_string(),
            timestamp: 1234567890,
        });

        // Remove reaction
        handlers.handle_domain_event(DomainEvent::ReactionRemoved {
            message_id,
            author_id,
            reactor_id,
            emoji: "👍".to_string(),
            timestamp: 1234567891,
        });

        // Should be removed entirely when count hits 0
        let reactions = handlers.streaming_state.get_reactions(&message_id);
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
        });

        // Check message state
        let messages = handlers.message_state.get_messages(&channel_id);
        assert!(messages[0].is_pinned);

        // Check pinned state
        assert_eq!(handlers.pinned_state.total_count(), 1);
        assert_eq!(handlers.pinned_state.current_message_id(), Some(message_id));
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
        });

        // Unpin it
        handlers.handle_domain_event(DomainEvent::MessageUnpinned {
            message_id,
            unpinner_id: pinner_id,
            channel_id,
        });

        // Check message state
        let messages = handlers.message_state.get_messages(&channel_id);
        assert!(!messages[0].is_pinned);

        // Check pinned state
        assert_eq!(handlers.pinned_state.total_count(), 0);
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
