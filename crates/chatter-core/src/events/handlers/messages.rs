use super::EventHandlers;
use crate::inbox::handlers::DomainEvent;
use tracing::{trace, warn};

impl EventHandlers {
    pub(super) fn handle_messages_event(&self, event: DomainEvent) {
        match event {
            // ================================================================
            // Message Events
            // ================================================================
            DomainEvent::MessageReceived { message } => {
                let channel_id = message.conversation_id;
                trace!(?channel_id, message_id = ?message.id, "Adding message to state");
                // A pin that arrived before its message is applied at store
                // time (`message_modifier_repo`), so no MessagePinned follows.
                if message.is_pinned {
                    self.chat_state.add_pinned_message(message.id);
                }
                self.message_state.append_message(channel_id, message);
            }

            DomainEvent::MessageEdited {
                message_id,
                new_content,
                channel_id,
                conversation_type,
                author_id,
                ..  // target_sent_timestamp
            } => {
                trace!(?channel_id, ?message_id, "Updating message content in state + DB");
                // A Saved edit is durable before its event is sent
                // (`ConversationHandle::edit_saved_message`), so the stored
                // row is the truth: a queued older event must neither show
                // nor write its body over a newer edit. The row read back by
                // id must be that Saved row — same kind, conversation and
                // author — or the event is dropped.
                let new_content = if conversation_type
                    == crate::domain::message::ConversationType::SavedMessages
                {
                    let Some(stored) = self
                        .message_service
                        .as_ref()
                        .and_then(|svc| svc.message_repo.get(&message_id).ok().flatten())
                        .filter(|stored| {
                            stored.conversation_type == conversation_type
                                && stored.conversation_id == channel_id
                                && stored.sender_id == Some(author_id)
                                && !stored.is_deleted
                        })
                    else {
                        return;
                    };
                    stored.content
                } else {
                    // Persist to DB so is_edited survives reload
                    if let Some(ref svc) = self.message_service {
                        if let Err(e) = svc.update_message_body(&message_id, &new_content) {
                            tracing::warn!(?message_id, error = %e, "Failed to persist edit to DB");
                        }
                    }
                    new_content
                };
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
                ..
            } => {
                tracing::info!(
                    ?message_id,
                    ?author_id,
                    ?channel_id,
                    "Inbox event: MessageDeleted received"
                );
                // Remove from in-memory state. Storage is already final:
                // every emitter deletes the row before announcing it
                // (`MessageService::apply_delete`, `channel_stream`, the anon
                // backends), so a UI rebuild on this event cannot race it.
                if let Some(ch_id) = channel_id {
                    self.message_state.remove_message(&ch_id, &message_id);
                } else if let Some(ch_id) = self.message_state.find_channel_for_message(&message_id) {
                    self.message_state.remove_message(&ch_id, &message_id);
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
            DomainEvent::MessageDeliveryChanged { message_id, channel_id, status } => {
                self.message_state.update_message(&channel_id, &message_id, |message| {
                    message.delivery_status = crate::services::message::advance_delivery_status(&message.delivery_status, &status).to_owned();
                });
            }
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
            DomainEvent::ReactionAdded {scope,sender_id,sent_at_ns,..}
            | DomainEvent::ReactionRemoved {scope,sender_id,sent_at_ns,..}
            | DomainEvent::ReactionSnapshotChanged {scope,sender_id,sent_at_ns} => {
                if let Some(service)=&self.message_service {
                    // Events identify persisted targets. Re-read instead of
                    // manufacturing aggregate counts from actor-specific deltas.
                    match service.message_repo.get_by_reaction_scope(&scope,&sender_id,sent_at_ns) {
                        Ok(Some(message))=> {
                            let presentation=crate::domain::reaction::reaction_presentation(&message.reaction_counts,&message.current_user_reactions);
                            self.chat_state.replace_reactions(&scope.cache_key(&sender_id,message.sent_at),presentation.into_iter().map(|r|crate::state::chat_state::ReactionSummary {emoji:r.emoji,count:r.count,user_reacted:r.own}).collect());
                        }
                        Ok(None)=>self.chat_state.replace_reactions(&scope.cache_key(&sender_id,sent_at_ns),Vec::new()),
                        Err(_)=>warn!("Persisted reaction cache refresh awaits storage recovery"),
                    }
                }
            }

            // ================================================================
            // Pin Events
            // ================================================================
            DomainEvent::MessagePinned {
                message_id,
                channel_id,
                pinner_id,
                conversation_type,
                ..
            } => {
                tracing::info!(
                    ?message_id,
                    ?channel_id,
                    ?pinner_id,
                    "Inbox event: MessagePinned received"
                );
                self.apply_pin_event(message_id, channel_id, conversation_type, pinner_id, true);
            }

            DomainEvent::MessageUnpinned {
                message_id,
                channel_id,
                unpinner_id,
                conversation_type,
                ..
            } => {
                tracing::info!(
                    ?message_id,
                    ?channel_id,
                    ?unpinner_id,
                    "Inbox event: MessageUnpinned received"
                );
                self.apply_pin_event(message_id, channel_id, conversation_type, unpinner_id, false);
            }

            // ================================================================
            // Attachment Events
            // ================================================================
            DomainEvent::AttachmentReady {
                attachment: _,
                message_id,
                channel_id,
            } => {
                trace!(?channel_id, ?message_id, "Attachment ready");
                // Attachment records are created by InboxEventProcessor::create_from_pointers()
                // before the UI handler runs. Download is triggered by UI's trigger_channel_attachment_download().
            }

            // ================================================================
            // Sync Events
            // ================================================================
            DomainEvent::DeleteForMeSync {
                conversation_id,
                target_message_id,
                saved_deletion_persisted,
                ..
            } => {
                trace!(?conversation_id, ?target_message_id, "DeleteForMeSync: removing message locally");
                self.message_state
                    .remove_message(&conversation_id, &target_message_id);
                // Persist deletion to DB
                if !saved_deletion_persisted && let Some(ref svc) = self.message_service {
                    if let Err(e) = svc.delete_message_local(&target_message_id) {
                        tracing::warn!(?target_message_id, error = %e, "Failed to persist delete-for-me to DB");
                    }
                }
            }

            DomainEvent::MessageSnapshotChanged { message } => {
                let message = if let Some(service) = &self.message_service {
                    let Some(stored) = service.message_repo.get(&message.id).ok().flatten()
                        .filter(|stored| stored.conversation_type == message.conversation_type
                            && stored.conversation_id == message.conversation_id
                            && stored.sender_id == message.sender_id
                            && stored.sent_at == message.sent_at && !stored.is_deleted)
                    else { return };
                    stored
                } else {
                    message
                };
                let channel_id = message.conversation_id;
                trace!(?channel_id, message_id = ?message.id, "MessageSnapshotChanged: updating state");
                if self.message_state.get_messages(&channel_id).iter().any(|m| m.id == message.id) {
                    self.message_state.update_message(&channel_id, &message.id, |stored| {
                        *stored = message.clone();
                    });
                } else {
                    self.message_state.append_message(channel_id, message);
                }
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
            _ => unreachable!("event dispatched to wrong handler category"),
        }
    }

    /// A pin event into state and, for every kind but Saved, the DB.
    ///
    /// A Saved pin is durable before its event is sent
    /// (`ConversationHandle::pin_saved_message`), so the stored row is the
    /// truth: it is not written again, and the flag state takes is the row's
    /// current `is_pinned`, not the event's, so a queued older pin cannot
    /// replay over a later unpin, edit or delete. The row read back by id
    /// must be that Saved row — same kind and conversation, our own
    /// authorship, undeleted — or the event is dropped, and the pinned bar
    /// is only moved while that Saved channel is the open conversation
    /// (`ActiveConversation::Channel`; a DM or anonymous conversation
    /// sharing the id is not it). Other kinds keep their path: the event's
    /// flag is persisted and applied.
    fn apply_pin_event(
        &self,
        message_id: crate::domain::ids::MessageId,
        channel_id: crate::domain::ids::ChannelId,
        conversation_type: crate::domain::message::ConversationType,
        actor: crate::domain::ids::UserId,
        pinned: bool,
    ) {
        let pinned = if conversation_type == crate::domain::message::ConversationType::SavedMessages
        {
            let Some(stored) = self
                .message_service
                .as_ref()
                .and_then(|svc| svc.message_repo.get(&message_id).ok().flatten())
                .filter(|stored| {
                    stored.conversation_type == conversation_type
                        && stored.conversation_id == channel_id
                        && stored.sender_id == Some(actor)
                        && !stored.is_deleted
                })
            else {
                return;
            };
            let open = matches!(
                self.chat_state.active_conversation(),
                crate::state::ActiveConversation::Channel(id) if id == channel_id
            );
            if !open {
                self.message_state
                    .update_message(&channel_id, &message_id, |msg| {
                        msg.is_pinned = stored.is_pinned;
                    });
                return;
            }
            stored.is_pinned
        } else {
            if let Some(ref svc) = self.message_service {
                if let Err(e) = svc.set_message_pinned(&message_id, pinned) {
                    tracing::warn!(?message_id, pinned, error = %e, "Failed to persist pin to DB");
                }
            }
            pinned
        };
        self.message_state
            .update_message(&channel_id, &message_id, |msg| {
                msg.is_pinned = pinned;
            });
        if pinned {
            self.chat_state.add_pinned_message(message_id);
        } else {
            self.chat_state.remove_pinned_message(&message_id);
        }
        tracing::debug!(?channel_id, ?message_id, pinned, "Applied pin state");
    }

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
