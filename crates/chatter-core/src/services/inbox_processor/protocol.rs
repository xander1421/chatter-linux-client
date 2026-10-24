use super::*;

impl InboxEventProcessor {
    pub(super) async fn process_protocol(&self, event: &InboxEvent) {
        match event {
            // =================================================================
            // Pure protocol events
            // =================================================================
            InboxEvent::DmDecryptionFailed {
                peer_id,
                error,
                session_desync,
            } => {
                tracing::error!(
                    "DM decryption failed from {}: {} (session_desync={})",
                    peer_id,
                    error,
                    session_desync
                );
                if *session_desync {
                    self.dm_bootstrap_service.reset_peer_sessions(*peer_id);
                }
            }

            InboxEvent::DmDecryptionErrorNeedsReport {
                peer_id,
                peer_device_id,
                dem_serialized,
            } => {
                tracing::warn!(
                    peer_id = %peer_id,
                    peer_device_id,
                    "1:1 session desync — sending DecryptionErrorMessage report to sender"
                );
                let message_service = self.message_service.clone();
                let peer_id = *peer_id;
                let peer_device_id = *peer_device_id;
                let dem = dem_serialized.clone();
                tokio::spawn(async move {
                    if let Err(e) = message_service
                        .send_decryption_error_report(&peer_id, peer_device_id, dem)
                        .await
                    {
                        tracing::error!(
                            peer_id = %peer_id,
                            peer_device_id,
                            error = %e,
                            "Failed to deliver DecryptionErrorMessage report — peer session stays wedged until their next message triggers another report"
                        );
                    }
                });
            }

            InboxEvent::GroupResendRequested {
                peer_id,
                peer_device_id,
                timestamp_ms,
            } => {
                let message_service = self.message_service.clone();
                let (peer_id, peer_device_id, timestamp_ms) =
                    (*peer_id, *peer_device_id, *timestamp_ms);
                tokio::spawn(async move {
                    message_service
                        .resend_on_decryption_error(&peer_id, peer_device_id, timestamp_ms)
                        .await;
                });
            }

            InboxEvent::DmResendRequested {
                peer_id,
                peer_device_id,
                timestamp_ms,
            } => {
                let message_service = self.message_service.clone();
                let (peer_id, peer_device_id, timestamp_ms) =
                    (*peer_id, *peer_device_id, *timestamp_ms);
                tokio::spawn(async move {
                    message_service
                        .resend_dm_on_decryption_error(&peer_id, peer_device_id, timestamp_ms)
                        .await;
                });
            }

            InboxEvent::DmMailboxExpired { peer_id } => {
                tracing::info!(
                    peer_id = %peer_id,
                    "HEALING: DmMailboxExpired event received, triggering re-bootstrap"
                );
                self.dm_bootstrap_service.handle_mailbox_expired(*peer_id);
            }

            InboxEvent::DmPollCompleted { total_messages } => {
                if *total_messages > 0 {
                    tracing::debug!("DM poll completed: {} messages", total_messages);
                }
            }

            InboxEvent::MessageReceived { message_id, .. } => {
                tracing::trace!("Inbox message received: {}", message_id);
            }

            InboxEvent::MessageProcessed { message_id } => {
                tracing::trace!("Inbox message processed: {}", message_id);
            }

            InboxEvent::MessageFailed { message_id, error } => {
                tracing::warn!("Inbox message failed: {} - {}", message_id, error);
            }

            InboxEvent::PollCompleted { count, .. } => {
                if *count > 0 {
                    tracing::trace!("Inbox poll completed: {} messages", count);
                }
            }

            InboxEvent::PollFailed { error } => {
                tracing::warn!("Inbox poll failed: {}", error);
            }

            InboxEvent::AcknowledgeFailed { message_ids, error } => {
                tracing::warn!("Inbox ack failed for {:?}: {}", message_ids, error);
            }

            InboxEvent::GroupMailboxPollCompleted {
                total_groups,
                total_messages,
            } => {
                tracing::trace!(total_groups, total_messages, "Group mailbox poll completed");
            }

            InboxEvent::CallKeyAnswerReceived {
                peer_id,
                call_id,
                ephemeral_public_key,
                timestamp: _,
            } => {
                tracing::info!(
                    peer_id = %peer_id,
                    call_id = %call_id,
                    "Call key answer received - completing key exchange"
                );
                self.event_bus
                    .emit_domain(DomainEvent::CallKeyAnswerReceived {
                        call_id: call_id.clone(),
                        peer_id: *peer_id,
                        ephemeral_public_key: *ephemeral_public_key,
                    });
            }

            InboxEvent::CallKeyOfferReceived {
                peer_id,
                call_id,
                ephemeral_public_key,
                call_type,
                timestamp,
            } => {
                // Bridge the inbox-layer offer to the domain event the call
                // orchestrator + incoming-call UI consume. Without this arm the
                // decrypted offer is emitted onto the inbox broadcast and dropped
                // — the callee never rings. (Answer/GroupCallKey are bridged
                // below; the offer arm was the missing sibling.)
                let is_identity_verified = self
                    .signal_service
                    .is_peer_verified(&peer_id.to_string())
                    .unwrap_or(false);
                let caller_name = self.user_service.get_display_name(peer_id);
                tracing::info!(
                    peer_id = %peer_id,
                    call_id = %call_id,
                    is_identity_verified,
                    "Call offer received - presenting incoming call"
                );
                self.event_bus
                    .emit_domain(DomainEvent::CallKeyOfferReceived {
                        call_id: call_id.clone(),
                        peer_id: *peer_id,
                        ephemeral_public_key: *ephemeral_public_key,
                        call_type: call_type.clone(),
                        is_identity_verified,
                        // No existing producer emits a warning string; a mismatch/
                        // change surfaces via the safety-number flow, not here.
                        identity_warning: String::new(),
                        caller_name,
                        timestamp: *timestamp,
                    });
            }

            InboxEvent::GroupCallSignalReceived {
                sender_id,
                call_id,
                channel_id,
                signal,
                timestamp,
            } => {
                tracing::info!(
                    sender_id = %sender_id,
                    call_id = %call_id,
                    ?signal,
                    channel_id = %channel_id,
                    timestamp = timestamp,
                    "Group call signal received"
                );
                self.event_bus
                    .emit_domain(DomainEvent::GroupCallSignalReceived {
                        call_id: call_id.clone(),
                        sender_id: *sender_id,
                        channel_id: *channel_id,
                        signal: *signal,
                    });
            }

            InboxEvent::SenderKeyNeeded {
                channel_id,
                sender_id,
            } => {
                // The inbox route does not carry the sender's device; gate it
                // under device 0 so it shares one backoff per sender.
                if !self
                    .message_service
                    .should_request_sender_key(*channel_id, *sender_id, 0)
                {
                    return;
                }
                tracing::info!(
                    channel_id = %channel_id,
                    sender_id = %sender_id,
                    "Sender key needed - sending request"
                );
                let msg_svc = self.message_service.clone();
                let ch_id = *channel_id;
                let s_id = *sender_id;
                tokio::spawn(async move {
                    if let Err(e) = msg_svc.send_sender_key_request(&ch_id, &s_id).await {
                        tracing::warn!(
                            channel_id = %ch_id,
                            sender_id = %s_id,
                            error = %e,
                            "Failed to send sender key request"
                        );
                    } else {
                        tracing::info!(
                            channel_id = %ch_id,
                            sender_id = %s_id,
                            "Sent sender key request to peer"
                        );
                    }
                });
            }

            InboxEvent::RespondWithSkdm {
                channel_id,
                requester_id,
                skdm_bytes,
            } => {
                tracing::info!(
                    channel_id = %channel_id,
                    requester_id = %requester_id,
                    skdm_len = skdm_bytes.len(),
                    "SenderKeyRequest received"
                );
                let msg_svc = self.message_service.clone();
                let ch_id = *channel_id;
                let req_id = *requester_id;
                let skdm_bytes = skdm_bytes.clone();
                tokio::spawn(async move {
                    match msg_svc
                        .answer_sender_key_request(&ch_id, &req_id, &skdm_bytes)
                        .await
                    {
                        Ok(true) => tracing::info!(
                            channel_id = %ch_id,
                            requester_id = %req_id,
                            "Sent SKDM response to requester"
                        ),
                        Ok(false) => {}
                        Err(e) => tracing::warn!(
                            channel_id = %ch_id,
                            requester_id = %req_id,
                            error = %e,
                            "Failed to send SKDM response"
                        ),
                    }
                });
            }

            _ => unreachable!("event routed to wrong inbox concern"),
        }
    }
}
