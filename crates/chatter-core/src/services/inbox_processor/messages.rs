use super::*;

impl InboxEventProcessor {
    pub(super) async fn process_messages(&self, event: &InboxEvent) {
        match event {
            // =================================================================
            InboxEvent::DmReceived {
                peer_id,
                timestamp,
                message_id,
                attachments,
                ..
            } => {
                // Create attachment records BEFORE UI handler reads from DB
                if !attachments.is_empty() {
                    let pointers: Vec<_> = attachments
                        .iter()
                        .filter(|p| !p.cdn_key.is_empty())
                        .cloned()
                        .collect();
                    if !pointers.is_empty()
                        && let Err(error) = self.attachment_service.create_from_pointers(&pointers)
                    {
                        tracing::warn!(%error, "Failed to store attachment records");
                    }
                }

                // Protocol: send delivery receipt
                tracing::info!(
                    peer_id = %peer_id,
                    timestamp,
                    "DELIVERY_RECEIPT: Triggering delivery receipt for received DM"
                );
                // Same conversation as a sealed-sender DM; the receipt's
                // transport is picked by mode in `send_friend_dm_content`.
                let handle = ConversationHandle::new(
                    ActiveConversation::ServerDm { peer_id: *peer_id },
                    self.client.clone(),
                );
                let ts = *timestamp;
                tokio::spawn(async move {
                    if let Err(e) = handle.send_delivery_receipt(&[ts]).await {
                        tracing::warn!("Failed to send DM delivery receipt: {}", e);
                    }
                });

                // Emit DomainEvent so domain_handler can display the message
                if let Ok(mid) = message_id.parse::<crate::domain::ids::MessageId>() {
                    if let Ok(Some(message)) = self.message_service.get_message_local(&mid) {
                        self.event_bus
                            .emit_domain(DomainEvent::MessageReceived { message });
                    }
                }
            }

            InboxEvent::ChannelMessageReceived {
                sender_id,
                message_id,
                timestamp,
                attachments,
                ..
            } => {
                // Create attachment records BEFORE UI handler reads from DB
                // This prevents the "empty bubble" race condition
                if !attachments.is_empty() {
                    let pointers: Vec<_> = attachments
                        .iter()
                        .filter(|p| !p.cdn_key.is_empty())
                        .cloned()
                        .collect();
                    if !pointers.is_empty()
                        && let Err(error) = self.attachment_service.create_from_pointers(&pointers)
                    {
                        tracing::warn!(%error, "Failed to store attachment records");
                    }
                }

                // Protocol: pre-fetch sender profile
                if self
                    .user_service
                    .get_user_local(sender_id)
                    .ok()
                    .flatten()
                    .is_none()
                {
                    tracing::debug!(
                        sender_id = %sender_id,
                        "Fetching sender profile for channel message"
                    );
                    // The fetch can finish after a join notice was rendered.
                    // Emit the normal profile event so visible rows re-resolve.
                    self.spawn_peer_profile_refresh(*sender_id);
                }

                // Read the message store_data_message already persisted. Its
                // conversation_type tells us how to address the delivery
                // receipt: a channel-free DM (ServerDm, keyed by the sender)
                // must receipt back to the peer via the sealed per-device
                // path, NOT a non-existent channel.
                let stored = message_id
                    .parse::<crate::domain::ids::MessageId>()
                    .ok()
                    .and_then(|mid| self.message_service.get_message_local(&mid).ok().flatten());

                // A self-synced OUTGOING message (SyncSent, F8 fix) is attributed
                // to US: `sender_id` is our own id. You never send a delivery
                // receipt for your own message, so skip the receipt entirely for
                // it — otherwise our linked device would receipt back to our own
                // devices.
                let is_own_synced = self
                    .message_service
                    .current_user_id()
                    .map(|me| me == *sender_id)
                    .unwrap_or(false);

                if !is_own_synced
                    && matches!(
                        stored.as_ref().map(|m| &m.conversation_type),
                        Some(crate::domain::message::ConversationType::ServerDm)
                    )
                {
                    // Channel receipts are owned by the shared post-commit
                    // listener; this event also carries channel-free friend DMs.
                    let handle = ConversationHandle::new(
                        ActiveConversation::ServerDm {
                            peer_id: *sender_id,
                        },
                        self.client.clone(),
                    );
                    let ts = *timestamp;
                    tokio::spawn(async move {
                        if let Err(e) = handle.send_delivery_receipt(&[ts]).await {
                            tracing::warn!("Failed to send delivery receipt: {}", e);
                        }
                    });
                }

                // Emit DomainEvent so domain_handler can display the message.
                if let Some(message) = stored {
                    self.event_bus
                        .emit_domain(DomainEvent::MessageReceived { message });
                }
            }

            InboxEvent::CallHangup {
                peer_id,
                call_id,
                reason,
                timestamp: _,
            } => {
                tracing::info!(
                    peer_id = %peer_id,
                    call_id = %call_id,
                    reason = %reason,
                    "Call hangup received - emitting domain event for cleanup"
                );
                #[cfg(feature = "media")]
                {
                    let own_elsewhere = self.user_service.current_user_id().ok() == Some(*peer_id)
                        && matches!(
                            reason,
                            crate::services::gateway::CallHangupReason::AcceptedElsewhere
                                | crate::services::gateway::CallHangupReason::DeclinedElsewhere
                        );
                    let Some(worker) = self.client.call_service().get() else {
                        return;
                    };
                    match worker
                        .authenticate_signal_hangup(call_id, *peer_id, own_elsewhere)
                        .await
                    {
                        super::super::call_worker::SignalHangupDisposition::Forward => {}
                        super::super::call_worker::SignalHangupDisposition::GroupRingWithdrawn(
                            revision,
                        ) => {
                            if self.client.auth_state().get().session_revision == revision {
                                self.client
                                    .call_signaling()
                                    .drop_ringing_group_call_invite(call_id);
                            }
                            return;
                        }
                        super::super::call_worker::SignalHangupDisposition::Reject => {
                            tracing::warn!(%call_id, %peer_id, "Ignoring hangup without a matching peer or pending group caller");
                            return;
                        }
                    }
                }
                self.event_bus.emit_domain(DomainEvent::CallHangupReceived {
                    call_id: call_id.clone(),
                    peer_id: *peer_id,
                    reason: *reason,
                });
            }

            InboxEvent::DmBootstrapCompleted { peer_id } => {
                tracing::info!(
                    peer_id = %peer_id,
                    "BOOTSTRAP[4/4] DmBootstrapCompleted event received - will flush pending messages"
                );
                // The UI hears DmBootstrapCompleted (or MessageSendFailed)
                // from the service once the queued messages have been sent.
                self.dm_bootstrap_service
                    .handle_bootstrap_completed(*peer_id, self.event_bus.clone());
            }

            // =================================================================
            // Ephemeral events — emit DomainEvent for DB persistence, UI also handles
            // =================================================================
            InboxEvent::DmReaction {
                conversation_peer_id,
                peer_id,
                emoji,
                remove,
                target_sent_timestamp,
                target_author_aci,
                timestamp,
            } => {
                let (scope, sender_id) = match self.message_service.dm_reaction_admission(
                    *conversation_peer_id,
                    *peer_id,
                    target_author_aci,
                ) {
                    Ok(admitted) => admitted,
                    Err(error) => {
                        tracing::warn!(%error,"Rejected reaction destination metadata");
                        return;
                    }
                };
                // Persist first; the unified-reactions table has UNIQUE
                // (conversation kind/id, author, time, reactor, emoji).
                // Only emit the UI delta if the reaction was actually new —
                // otherwise the snapshot + delta both apply and the bubble
                // shows count=2 for a single reaction. The target must be in
                // this friend DM (`resolve_inbound_target`).
                match self.message_service.record_inbound_reaction(
                    &scope,
                    peer_id,
                    &sender_id,
                    *target_sent_timestamp,
                    emoji,
                    *remove,
                    *timestamp,
                ) {
                    Ok(true) => {
                        let event = if *remove {
                            DomainEvent::ReactionRemoved {
                                scope: scope.clone(),
                                sender_id,
                                sent_at_ns: *target_sent_timestamp,
                                reactor_id: *peer_id,
                                emoji: emoji.clone(),
                                timestamp: *target_sent_timestamp,
                            }
                        } else {
                            DomainEvent::ReactionAdded {
                                scope: scope.clone(),
                                sender_id,
                                sent_at_ns: *target_sent_timestamp,
                                reactor_id: *peer_id,
                                emoji: emoji.clone(),
                                timestamp: *target_sent_timestamp,
                            }
                        };
                        self.event_bus.emit_domain(event);
                    }
                    Ok(false) => {
                        tracing::debug!(emoji = %emoji, "DM reaction held, dropped or already recorded — skipping UI delta");
                    }
                    Err(e) => tracing::warn!(error = %e, "Failed to persist DM reaction"),
                }
            }

            InboxEvent::ChannelReaction {
                channel_id,
                sender_id,
                emoji,
                remove,
                target_sent_timestamp,
                target_author_aci,
                timestamp,
            } => {
                let Ok(target_sender_id) = target_author_aci.parse() else {
                    tracing::warn!("Invalid reaction target author");
                    return;
                };
                let scope = self.message_service.channel_reaction_scope(
                    *channel_id,
                    &target_sender_id,
                    *target_sent_timestamp,
                );
                let scope = match scope {
                    Ok(scope) => scope,
                    Err(error) => {
                        tracing::warn!(%error, "Reaction scope unavailable");
                        return;
                    }
                };
                match self.message_service.record_inbound_reaction(
                    &scope,
                    sender_id,
                    &target_sender_id,
                    *target_sent_timestamp,
                    emoji,
                    *remove,
                    *timestamp,
                ) {
                    Ok(true) => {
                        let event = if *remove {
                            DomainEvent::ReactionRemoved {
                                scope: scope.clone(),
                                sender_id: target_sender_id,
                                sent_at_ns: *target_sent_timestamp,
                                reactor_id: *sender_id,
                                emoji: emoji.clone(),
                                timestamp: *target_sent_timestamp,
                            }
                        } else {
                            DomainEvent::ReactionAdded {
                                scope: scope.clone(),
                                sender_id: target_sender_id,
                                sent_at_ns: *target_sent_timestamp,
                                reactor_id: *sender_id,
                                emoji: emoji.clone(),
                                timestamp: *target_sent_timestamp,
                            }
                        };
                        self.event_bus.emit_domain(event);
                    }
                    Ok(false) => {
                        tracing::debug!(emoji = %emoji, "Channel reaction held, dropped or already recorded — skipping UI delta");
                    }
                    Err(e) => tracing::warn!(error = %e, "Failed to persist channel reaction"),
                }
            }

            InboxEvent::DmEdit {
                peer_id,
                target_sent_timestamp,
                new_body,
                edit_timestamp,
                server_timestamp,
            } => {
                match self.message_service.apply_dm_edit(
                    peer_id,
                    *target_sent_timestamp,
                    new_body,
                    *edit_timestamp,
                    *server_timestamp,
                ) {
                    Ok(Some(msg)) => {
                        self.event_bus.emit_domain(DomainEvent::MessageEdited {
                            message_id: msg.id,
                            new_content: msg.content,
                            channel_id: msg.conversation_id,
                            conversation_type: msg.conversation_type,
                            author_id: *peer_id,
                            target_sent_timestamp: *target_sent_timestamp,
                        });
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!(%peer_id, error = %e, "Failed to apply a DM edit"),
                }
            }

            InboxEvent::ChannelEdit {
                channel_id,
                target_sent_timestamp,
                new_body,
                sender_id,
                edit_timestamp,
                server_timestamp,
            } => {
                match self.message_service.apply_channel_edit(
                    channel_id,
                    sender_id,
                    *target_sent_timestamp,
                    new_body,
                    *edit_timestamp,
                    *server_timestamp,
                ) {
                    Ok(Some(msg)) => {
                        self.event_bus.emit_domain(DomainEvent::MessageEdited {
                            message_id: msg.id,
                            new_content: msg.content,
                            channel_id: *channel_id,
                            conversation_type: msg.conversation_type,
                            author_id: *sender_id,
                            target_sent_timestamp: *target_sent_timestamp,
                        });
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(%channel_id, error = %e, "Failed to apply a channel edit")
                    }
                }
            }

            InboxEvent::DmDeleteForEveryone {
                target_author_aci,
                destination_user_id,
                target_call_id,
                peer_id,
                target_sent_timestamp,
            } => {
                match self
                    .message_service
                    .apply_scoped_call_or_timestamp_delete_received(
                        peer_id,
                        target_author_aci.as_deref(),
                        destination_user_id.as_deref(),
                        target_call_id.as_deref(),
                        *target_sent_timestamp,
                    )
                    .await
                {
                    Ok(Some(msg)) => {
                        self.event_bus.emit_domain(DomainEvent::MessageDeleted {
                            message_id: msg.id,
                            author_id: msg.sender_id.unwrap_or(*peer_id),
                            channel_id: Some(msg.conversation_id),
                            target_sent_timestamp: msg.sent_at,
                        });
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!(%peer_id, error = %e, "Failed to apply a DM delete"),
                }
            }

            InboxEvent::ChannelDeleteForEveryone {
                channel_id,
                sender_id,
                target_sent_timestamp,
                ..
            } => {
                match self.message_service.apply_channel_delete(
                    channel_id,
                    sender_id,
                    *target_sent_timestamp,
                ) {
                    Ok(Some(msg)) => {
                        self.event_bus.emit_domain(DomainEvent::MessageDeleted {
                            message_id: msg.id,
                            author_id: *sender_id,
                            channel_id: Some(*channel_id),
                            target_sent_timestamp: *target_sent_timestamp,
                        });
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(%channel_id, error = %e, "Failed to apply a channel delete")
                    }
                }
            }

            InboxEvent::DmPin {
                peer_id,
                target_sent_timestamp,
                unpin,
                target_author_aci,
                timestamp,
            } => {
                let target = target_author_aci.parse().ok().and_then(|author| {
                    self.message_service
                        .resolve_inbound_pin(
                            &crate::domain::ChannelId::from(*peer_id),
                            peer_id,
                            &author,
                            *target_sent_timestamp,
                            *unpin,
                            *timestamp,
                        )
                        .ok()
                        .flatten()
                });
                if let Some(msg) = target {
                    if *unpin {
                        self.event_bus.emit_domain(DomainEvent::MessageUnpinned {
                            message_id: msg.id,
                            unpinner_id: *peer_id,
                            channel_id: msg.conversation_id,
                            conversation_type: msg.conversation_type,
                            target_sent_timestamp: *target_sent_timestamp,
                        });
                    } else {
                        self.event_bus.emit_domain(DomainEvent::MessagePinned {
                            message_id: msg.id,
                            pinner_id: *peer_id,
                            channel_id: msg.conversation_id,
                            conversation_type: msg.conversation_type,
                            target_sent_timestamp: *target_sent_timestamp,
                        });
                    }
                }
            }

            InboxEvent::ChannelCallEvent {
                channel_id,
                sender_id,
                call_id,
                phase,
                duration_secs,
                timestamp,
            } => {
                // Files the row and tells the UI; a non-member's is dropped there.
                if let Err(e) = self.message_service.file_channel_call_event(
                    channel_id,
                    sender_id,
                    call_id,
                    *phase,
                    *duration_secs,
                    *timestamp,
                ) {
                    tracing::warn!(error = %e, channel_id = %channel_id, "Channel call event not filed");
                }
            }

            InboxEvent::ChannelPin {
                channel_id,
                sender_id,
                target_sent_timestamp,
                unpin,
                target_author_aci,
                timestamp,
            } => {
                let target = target_author_aci.parse().ok().and_then(|author| {
                    self.message_service
                        .resolve_inbound_pin(
                            channel_id,
                            sender_id,
                            &author,
                            *target_sent_timestamp,
                            *unpin,
                            *timestamp,
                        )
                        .ok()
                        .flatten()
                });
                if let Some(msg) = target {
                    if *unpin {
                        self.event_bus.emit_domain(DomainEvent::MessageUnpinned {
                            message_id: msg.id,
                            unpinner_id: *sender_id,
                            channel_id: *channel_id,
                            conversation_type: msg.conversation_type,
                            target_sent_timestamp: *target_sent_timestamp,
                        });
                    } else {
                        self.event_bus.emit_domain(DomainEvent::MessagePinned {
                            message_id: msg.id,
                            pinner_id: *sender_id,
                            channel_id: *channel_id,
                            conversation_type: msg.conversation_type,
                            target_sent_timestamp: *target_sent_timestamp,
                        });
                    }
                }
            }

            _ => unreachable!("event routed to wrong inbox concern"),
        }
    }
}
