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

impl InboxEventProcessor {
    fn publish_saved_snapshot(
        &self,
        own: UserId,
        outcome: crate::services::message::SavedSyncOutcome,
    ) {
        use crate::services::message::SavedSyncOutcome;
        match outcome {
            SavedSyncOutcome::Created(message) => {
                self.event_bus
                    .emit_domain(DomainEvent::SavedMessageSynced { message });
            }
            SavedSyncOutcome::Updated {
                message,
                body_changed,
                pin_changed,
                quote_hydrated,
            } => {
                if quote_hydrated {
                    self.event_bus.emit_domain(DomainEvent::SavedMessageSynced {
                        message: message.clone(),
                    });
                }
                if body_changed {
                    self.event_bus.emit_domain(DomainEvent::MessageEdited {
                        message_id: message.id,
                        new_content: message.content.clone(),
                        channel_id: message.conversation_id,
                        conversation_type: message.conversation_type.clone(),
                        author_id: own,
                        target_sent_timestamp: message.sent_at,
                    });
                }
                match pin_changed {
                    Some(true) => {
                        self.event_bus.emit_domain(DomainEvent::MessagePinned {
                            message_id: message.id,
                            pinner_id: own,
                            channel_id: message.conversation_id,
                            conversation_type: message.conversation_type,
                            target_sent_timestamp: message.sent_at,
                        });
                    }
                    Some(false) => {
                        self.event_bus.emit_domain(DomainEvent::MessageUnpinned {
                            message_id: message.id,
                            unpinner_id: own,
                            channel_id: message.conversation_id,
                            conversation_type: message.conversation_type,
                            target_sent_timestamp: message.sent_at,
                        });
                    }
                    None => {}
                }
            }
            SavedSyncOutcome::Unchanged => {}
        }
    }

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

            // The group-mailbox transport is retired for server-managed
            // channels; announcements from older peers are ignored.
            InboxEvent::GroupMailboxAnnouncement { .. } => {}

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
                channel_id,
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
                        channel_id: channel_id.clone(),
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

            // =================================================================
            // Anonymous Group Events
            // =================================================================
            InboxEvent::AnonGroupInvite {
                peer_id,
                group_id,
                group_name,
                inviter_mailbox_pk,
                timestamp: _,
                profile_key,
                encrypted_display_name,
                encrypted_avatar,
            } => {
                self.handle_anon_group_invite(
                    *peer_id,
                    group_id,
                    group_name,
                    inviter_mailbox_pk,
                    profile_key,
                    encrypted_display_name,
                    encrypted_avatar,
                );
                // Return false - UI needs to add sidebar entry
            }

            InboxEvent::AnonGroupJoinAck {
                peer_id,
                group_id,
                joiner_mailbox_pk,
                timestamp,
                profile_key,
                encrypted_display_name,
                encrypted_avatar,
            } => {
                self.handle_anon_group_join_ack(
                    *peer_id,
                    group_id,
                    joiner_mailbox_pk,
                    profile_key,
                    encrypted_display_name,
                    encrypted_avatar,
                    *timestamp,
                );
            }

            InboxEvent::AnonGroupMemberList {
                peer_id,
                group_id,
                members,
                timestamp,
            } => {
                tracing::info!(
                    peer_id = %peer_id,
                    group_id = ?group_id,
                    member_count = members.len(),
                    timestamp = timestamp,
                    "Received anonymous group member list"
                );

                let anon_group_id =
                    match crate::domain::anon_group::AnonGroupId::try_from_slice(group_id) {
                        Some(id) => id,
                        None => {
                            tracing::warn!("Invalid group_id length for AnonGroupMemberList");
                            return;
                        }
                    };

                let anon_service = self.anon_group_service.clone();
                let members_clone = members.clone();
                let sender_id = *peer_id;
                let timestamp = *timestamp;
                let _ = tokio::task::spawn_blocking(move || {
                    if let Err(e) =
                        anon_service.handle_member_list_ordered(anon_group_id, sender_id, members_clone, timestamp)
                    {
                        tracing::error!(
                            group_id = ?anon_group_id,
                            error = %e,
                            "Failed to handle member list"
                        );
                    }
                });
            }

            InboxEvent::AnonGroupLeave {
                peer_id,
                group_id,
                timestamp,
            } => {
                tracing::info!(
                    peer_id = %peer_id,
                    group_id = ?group_id,
                    timestamp = timestamp,
                    "Received anonymous group leave notification"
                );

                let anon_group_id =
                    match crate::domain::anon_group::AnonGroupId::try_from_slice(group_id) {
                        Some(id) => id,
                        None => {
                            tracing::warn!("Invalid group_id length for AnonGroupLeave");
                            return;
                        }
                    };

                let anon_service = self.anon_group_service.clone();
                let peer_id_clone = *peer_id;
                let timestamp = *timestamp;
                let _ = tokio::task::spawn_blocking(move || {
                    if let Err(e) = anon_service.handle_leave_ordered(anon_group_id, peer_id_clone, timestamp) {
                        tracing::error!(
                            group_id = ?anon_group_id,
                            peer_id = %peer_id_clone,
                            error = %e,
                            "Failed to handle leave"
                        );
                    }
                });
            }

            InboxEvent::AnonGroupKick {
                peer_id,
                group_id,
                kicked_user_id,
                admin_signature,
                timestamp,
            } => {
                tracing::info!(
                    peer_id = %peer_id,
                    group_id = ?group_id,
                    kicked_user_id = ?kicked_user_id,
                    timestamp = timestamp,
                    "Received anonymous group kick notification"
                );

                let anon_group_id =
                    match crate::domain::anon_group::AnonGroupId::try_from_slice(group_id) {
                        Some(id) => id,
                        None => {
                            tracing::warn!("Invalid group_id length for AnonGroupKick");
                            return;
                        }
                    };

                let kicked_user = match UserId::try_from_slice(kicked_user_id) {
                    Some(id) => id,
                    None => {
                        tracing::warn!("Invalid kicked_user_id for AnonGroupKick");
                        return;
                    }
                };

                let anon_service = self.anon_group_service.clone();
                let admin_signature_clone = admin_signature.clone();
                let peer_id_clone = *peer_id;
                let timestamp = *timestamp;
                let _ = tokio::task::spawn_blocking(move || {
                    if let Err(e) = anon_service.handle_kick(
                        anon_group_id,
                        kicked_user,
                        peer_id_clone,
                        &admin_signature_clone,
                        timestamp,
                    ) {
                        tracing::error!(
                            group_id = ?anon_group_id,
                            error = %e,
                            "Failed to handle kick"
                        );
                    }
                });
            }

            InboxEvent::AnonGroupUpdate {
                peer_id,
                group_id,
                update,
                timestamp,
            } => {
                let anon_group_id =
                    match crate::domain::anon_group::AnonGroupId::try_from_slice(group_id) {
                        Some(id) => id,
                        None => {
                            tracing::warn!("Invalid group_id length for AnonGroupUpdate");
                            return;
                        }
                    };

                tracing::info!(
                    peer_id = %peer_id,
                    group_id = %anon_group_id,
                    timestamp = timestamp,
                    "Received anonymous group update"
                );

                let anon_service = self.anon_group_service.clone();
                let update_clone = update.clone();
                let peer_id_clone = *peer_id;
                let _ = tokio::task::spawn_blocking(move || {
                    if let Err(e) =
                        anon_service.handle_update(anon_group_id, peer_id_clone, &update_clone)
                    {
                        tracing::error!(
                            group_id = %anon_group_id,
                            error = %e,
                            "Failed to handle update"
                        );
                    }
                });
            }

            InboxEvent::AnonGroupMessage {
                peer_id,
                group_id,
                inner_content,
            } => {
                self.handle_anon_group_message(*peer_id, group_id, inner_content);
            }

            // =================================================================
            // Anonymous DM Events
            // =================================================================
            InboxEvent::AnonDmRequest {
                peer_id,
                conversation_id,
                initiator_mailbox_pk,
                intro_message,
                profile_key,
                encrypted_display_name,
                encrypted_avatar,
                timestamp: _,
            } => {
                self.handle_anon_dm_request(
                    *peer_id,
                    conversation_id,
                    initiator_mailbox_pk,
                    intro_message,
                    profile_key,
                    encrypted_display_name,
                    encrypted_avatar,
                );
                // Return false - UI needs to add sidebar entry
            }

            InboxEvent::AnonDmAccept {
                peer_id,
                conversation_id,
                responder_mailbox_pk,
                profile_key,
                encrypted_display_name,
                encrypted_avatar,
                timestamp: _,
            } => {
                self.handle_anon_dm_accept(
                    *peer_id,
                    conversation_id,
                    responder_mailbox_pk,
                    profile_key,
                    encrypted_display_name,
                    encrypted_avatar,
                );
                // Return false - UI needs to update sidebar entry
            }

            InboxEvent::AnonDmMessage {
                peer_id,
                conversation_id,
                inner_content,
            } => {
                self.handle_anon_dm_message(*peer_id, conversation_id, inner_content);
            }

            // =================================================================
            // Mixed events (protocol part here, DomainEvent for the UI)
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
                    if !pointers.is_empty() {
                        self.attachment_service.create_from_pointers(&pointers);
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
                channel_id,
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
                    if !pointers.is_empty() {
                        self.attachment_service.create_from_pointers(&pointers);
                    }
                }

                // Protocol: pre-fetch sender profile + send delivery receipt
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
                    let user_svc = self.user_service.clone();
                    let sid = *sender_id;
                    tokio::spawn(async move {
                        match user_svc.get_user_profile(&sid).await {
                            Ok(profile) => {
                                tracing::debug!(
                                    sender_id = %sid,
                                    display_name = %profile.display_name,
                                    "Fetched and cached sender profile"
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    sender_id = %sid,
                                    error = %e,
                                    "Failed to fetch sender profile, will show UUID"
                                );
                            }
                        }
                    });
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

                if !is_own_synced {
                    let is_dm = matches!(
                        stored.as_ref().map(|m| &m.conversation_type),
                        Some(crate::domain::message::ConversationType::ServerDm)
                    );
                    let receipt_conversation = if is_dm {
                        ActiveConversation::ServerDm {
                            peer_id: *sender_id,
                        }
                    } else {
                        ActiveConversation::Channel(*channel_id)
                    };

                    let handle = ConversationHandle::new(receipt_conversation, self.client.clone());
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
                        super::call_worker::SignalHangupDisposition::Forward => {},
                        super::call_worker::SignalHangupDisposition::GroupRingWithdrawn(revision) => {
                            if self.client.auth_state().get().session_revision == revision {
                                self.client.call_signaling().drop_ringing_group_call_invite(call_id);
                            }
                            return;
                        },
                        super::call_worker::SignalHangupDisposition::Reject => {
                            tracing::warn!(%call_id, %peer_id, "Ignoring hangup without a matching peer or pending group caller");
                            return;
                        },
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

            InboxEvent::SavedMessageSync { sender_id, sync } => {
                // A peer must never write into our Saved Messages.
                let own = self.message_service.current_user_id().ok();
                if own.as_ref() != Some(sender_id) {
                    tracing::warn!(sender_id = %sender_id, "SavedMessageSync from another user dropped");
                    return;
                }
                let Some(own) = own else { return };
                // The snapshot is applied to storage first; the events then
                // carry the stored row, the same ones a local edit or pin
                // emits, so state and UI take them on the one path.
                match self.message_service.apply_saved_message_sync(sync) {
                    Ok(outcome) => self.publish_saved_snapshot(own, outcome),
                    Err(e) => {
                        tracing::warn!(message_id = %sync.message_id, error = %e, "Failed to apply synced saved message")
                    }
                }
            }

            InboxEvent::SavedSnapshotApplied { sender_id, outcome } => {
                self.publish_saved_snapshot(*sender_id, outcome.clone());
            }

            InboxEvent::ReadSync { sender_id, sync } => {
                // A peer must never mark our messages read: only our own
                // devices send a ReadSync.
                if self.message_service.current_user_id().ok().as_ref() != Some(sender_id) {
                    tracing::warn!(sender_id = %sender_id, "ReadSync from another user dropped");
                    return;
                }
                match self.message_service.apply_read_sync(sync) {
                    Ok(conversation_ids) if !conversation_ids.is_empty() => {
                        self.event_bus.emit_domain(DomainEvent::ConversationsRead {
                            conversation_ids,
                            from_other_device: true,
                        });
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "ReadSync: marking messages read failed"),
                }
            }

            InboxEvent::ProfileKeySync { sender_id, sync } => {
                // A peer must never set our profile key: only our own
                // devices send a ProfileKeySync.
                if self.message_service.current_user_id().ok().as_ref() != Some(sender_id) {
                    tracing::warn!(sender_id = %sender_id, "ProfileKeySync from another user dropped");
                    return;
                }
                match crate::services::profile_visibility::apply_sync(
                    &self.message_service.settings_repo,
                    sync,
                ) {
                    Ok(changed) => tracing::info!(key_changed = changed, "ProfileKeySync applied"),
                    Err(e) => tracing::warn!(error = %e, "ProfileKeySync: storing the key failed"),
                }
            }

            InboxEvent::ConfigurationSync { sender_id, sync } => {
                // `apply` drops one that is not from our own user: a peer
                // must never change our privacy settings.
                match crate::services::configuration_sync::apply(
                    &self.message_service.settings_repo,
                    sender_id,
                    sync,
                ) {
                    Ok(true) => tracing::info!("ConfigurationSync applied"),
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "ConfigurationSync: storing the settings failed")
                    }
                }
            }

            InboxEvent::ChannelDeleteForMe {
                sender_id,
                conversation_id,
                target_author_aci,
                target_sent_timestamp,
                saved_message_id,
                saved_messages_scope,
            } => {
                let result = if *saved_messages_scope || !saved_message_id.is_empty() {
                    persist_saved_delete_for_me_sync(
                        &self.message_service.message_repo,
                        self.message_service.current_user_id().ok(),
                        *sender_id,
                        conversation_id,
                        target_author_aci,
                        *target_sent_timestamp,
                        saved_message_id,
                        *saved_messages_scope,
                    )
                    .map(|event| {
                        self.event_bus.emit_domain(event);
                    })
                } else {
                    admit_delete_for_me_sync(
                        &self.message_service.message_repo,
                        &self.event_bus,
                        self.message_service.current_user_id().ok(),
                        *sender_id,
                        conversation_id,
                        target_author_aci,
                        *target_sent_timestamp,
                    )
                };
                if let Err(error) = result {
                    tracing::warn!(%error, "DeleteForMeSync rejected");
                }
            }

            InboxEvent::ChannelReceipt {
                channel_id,
                sender_id,
                sender_device_id,
                receipt_type,
                timestamps,
            } => {
                if let Err(error) = self.message_service.apply_peer_receipt(
                    *channel_id,
                    self.message_service.conversation_type(channel_id),
                    *sender_id,
                    crate::domain::ids::DeviceId(*sender_device_id),
                    *receipt_type,
                    timestamps,
                ) {
                    tracing::warn!(%error, "Failed to persist channel receipt");
                }
            }

            // =================================================================
            // Typing indicators — bridge inbox events to TypingStarted/Stopped
            // domain events (consumed by events/handlers.rs → chat state → UI).
            // action: 1 = Started, else Stopped. DM typing is keyed by the
            // peer's derived channel_id (matches the stored ServerDm rows).
            // =================================================================
            // Our own typing, fanned to our other devices along with the
            // members': it is not "someone is typing" here.
            InboxEvent::DmTyping {
                peer_id: user_id, ..
            }
            | InboxEvent::ChannelTyping {
                sender_id: user_id, ..
            } if self.message_service.current_user_id().ok().as_ref() == Some(user_id) => {}

            InboxEvent::DmTyping {
                peer_id,
                action,
                timestamp,
            } => {
                let channel_id = crate::domain::ids::ChannelId::from(*peer_id);
                let (user_id, timestamp) = (*peer_id, *timestamp);
                self.event_bus.emit_domain(if *action == 1 {
                    DomainEvent::TypingStarted {
                        user_id,
                        channel_id,
                        timestamp,
                    }
                } else {
                    DomainEvent::TypingStopped {
                        user_id,
                        channel_id,
                        timestamp,
                    }
                });
            }

            InboxEvent::ChannelTyping {
                channel_id,
                sender_id,
                action,
                timestamp,
            } => {
                let (user_id, channel_id, timestamp) = (*sender_id, *channel_id, *timestamp);
                self.event_bus.emit_domain(if *action == 1 {
                    DomainEvent::TypingStarted {
                        user_id,
                        channel_id,
                        timestamp,
                    }
                } else {
                    DomainEvent::TypingStopped {
                        user_id,
                        channel_id,
                        timestamp,
                    }
                });
            }

            InboxEvent::DmReceipt {
                peer_id,
                sender_device_id,
                receipt_type,
                timestamps,
            } => {
                if let Err(error) = self.message_service.apply_peer_receipt(
                    crate::domain::ids::ChannelId::from(*peer_id),
                    crate::domain::message::ConversationType::ServerDm,
                    *peer_id,
                    crate::domain::ids::DeviceId(*sender_device_id),
                    *receipt_type,
                    timestamps,
                ) {
                    tracing::warn!(%error, "Failed to persist DM receipt");
                }
            }

            // =================================================================
            // Friend-request fan-out — translate the inbox-layer notification
            // into a UI-visible DomainEvent. Without this bridge the user
            // events stream fires `InboxEvent::FriendRequestsUpdated` but the
            // UI (which only listens to `DomainEvent`) never refreshes the
            // pending-requests list.
            // =================================================================
            InboxEvent::FriendRequestsUpdated { received, sent } => {
                self.event_bus
                    .emit_domain(DomainEvent::FriendRequestsUpdated {
                        received: received.clone(),
                        sent: sent.clone(),
                    });
            }

            InboxEvent::FriendAccepted { friend_user_id } => {
                match friend_user_id.parse::<UserId>() {
                    Ok(friend_id) => {
                        // DMs are channel-free (Signal-style): there is no
                        // server DM channel to fetch. The sidebar is built
                        // from the friends list (Task 3.6), so the DM only
                        // appears once the friendship row exists. The accepter
                        // wrote it inline; we (the original requester) learn of
                        // acceptance here, so we must persist it ourselves.
                        if let Err(e) = self.user_service.record_accepted_friend_local(&friend_id) {
                            tracing::warn!(
                                peer_id = %friend_id,
                                error = %e,
                                "FriendAccepted: failed to persist friendship locally — DM will not appear"
                            );
                        }
                        self.event_bus
                            .emit_domain(DomainEvent::FriendAccepted { friend_id });

                        // Establish the DM Signal session in the background now
                        // that we're friends, so the first call offer / message
                        // doesn't pay the first-contact Sesame bootstrap on the
                        // call path. No-op on the higher-UUID side (Sesame).
                        let ms = self.message_service.clone();
                        tokio::spawn(async move {
                            if let Err(e) = ms.prewarm_dm_session(&friend_id).await {
                                tracing::warn!(
                                    peer_id = %friend_id,
                                    error = %e,
                                    "FriendAccepted: prewarm DM session failed"
                                );
                            }
                        });

                        // Signal-style handshake: send OUR profile key now so
                        // the friend learns it without waiting for a first
                        // real message. The accepter sends their half from
                        // the UI's accept-request handler; together both
                        // sides learn each other's key with zero messages.
                        let ms = self.message_service.clone();
                        tokio::spawn(async move {
                            if let Err(e) = ms.send_profile_key_handshake(&friend_id).await {
                                tracing::warn!(
                                    peer_id = %friend_id,
                                    error = %e,
                                    "FriendAccepted: profile-key handshake not delivered; retries exhausted or no longer friends"
                                );
                            }
                        });
                    }
                    Err(_) => {
                        tracing::warn!(
                            friend_user_id,
                            "FriendAccepted: failed to parse friend user ID — UI will not refresh"
                        );
                    }
                }
            }

            // Pure profile-key handshake (see `store_data_message` /
            // `send_profile_key_handshake`): the gateway already persisted
            // the sender's profile key and created no message row. Fetch +
            // decrypt their profile now — `get_user_profile` upserts it
            // locally — and translate it into the same DomainEvent the
            // server-managed profile stream emits, so the existing UI
            // handler (sidebar / header / friends list refresh) just works.
            InboxEvent::ProfileKeyReceived { sender_id } => {
                let sender_id = *sender_id;
                let user_service = self.user_service.clone();
                let event_bus = self.event_bus.clone();
                tokio::spawn(async move {
                    match user_service.get_user_profile(&sender_id).await {
                        Ok(profile) => {
                            event_bus.emit_domain(DomainEvent::UserProfileUpdated {
                                user_id: sender_id,
                                display_name: profile.display_name,
                                status_message: profile
                                    .status_message
                                    .as_ref()
                                    .map(|s| s.text.clone()),
                                avatar_bytes: profile.avatar_bytes,
                                timestamp: chrono::Utc::now().timestamp_millis() as u64,
                            });
                        }
                        Err(e) => {
                            tracing::warn!(
                                peer_id = %sender_id,
                                error = %e,
                                "ProfileKeyReceived: failed to fetch/decrypt peer profile"
                            );
                        }
                    }
                });
            }

            // =================================================================
            // Everything else has no protocol work. Nothing but this loop
            // reads InboxEvents, so an event that needs the UI must be given
            // an arm above that emits a DomainEvent.
            // =================================================================
            _ => {}
        }
    }

    // =====================================================================
    // Helper methods
    // =====================================================================

    fn handle_anon_group_invite(
        &self,
        peer_id: UserId,
        group_id: &[u8],
        group_name: &str,
        inviter_mailbox_pk: &[u8],
        profile_key: &Option<Vec<u8>>,
        encrypted_display_name: &Option<Vec<u8>>,
        encrypted_avatar: &Option<Vec<u8>>,
    ) {
        tracing::info!(
            peer_id = %peer_id,
            group_id = ?group_id,
            group_name = %group_name,
            has_profile = profile_key.is_some(),
            "Received anonymous group invite"
        );

        let anon_group_id = match crate::domain::anon_group::AnonGroupId::try_from_slice(group_id) {
            Some(id) => id,
            None => {
                tracing::warn!(
                    group_id = ?group_id,
                    "Invalid group_id length for AnonGroupInvite (expected 16 bytes)"
                );
                return;
            }
        };

        let mailbox_pk: [u8; 32] = match inviter_mailbox_pk.try_into() {
            Ok(pk) => pk,
            Err(_) => {
                tracing::warn!(
                    mailbox_pk_len = inviter_mailbox_pk.len(),
                    "Invalid mailbox_pk length for AnonGroupInvite (expected 32 bytes)"
                );
                return;
            }
        };

        // Parse and decrypt profile data
        let profile_key_arr: Option<[u8; 32]> = profile_key.as_ref().and_then(|pk| {
            if pk.len() == 32 {
                let mut key = [0u8; 32];
                key.copy_from_slice(pk);
                Some(key)
            } else {
                None
            }
        });

        use crate::crypto::profile_crypto::{decrypt_avatar, decrypt_display_name};

        let inviter_display_name: Option<String> = match (&profile_key_arr, encrypted_display_name)
        {
            (Some(pk), Some(encrypted)) if !encrypted.is_empty() => {
                decrypt_display_name(pk, encrypted)
                    .ok()
                    .filter(|n| !n.is_empty())
            }
            _ => None,
        };

        let inviter_avatar: Option<Vec<u8>> = match (&profile_key_arr, encrypted_avatar) {
            (Some(pk), Some(encrypted)) if !encrypted.is_empty() => {
                decrypt_avatar(pk, encrypted).ok()
            }
            _ => None,
        };

        tracing::debug!(
            peer_id = %peer_id,
            inviter_display_name = ?inviter_display_name,
            "Decrypted inviter profile for group invite"
        );

        // Emit domain event for UI to show invite dialog
        self.event_bus
            .emit_domain(DomainEvent::AnonGroupInviteReceived {
                group_id: anon_group_id,
                group_name: group_name.to_string(),
                inviter_id: peer_id,
                inviter_mailbox_pk: mailbox_pk,
                profile_key: profile_key_arr,
                inviter_display_name,
                inviter_avatar,
            });
    }

    fn handle_anon_group_join_ack(
        &self,
        peer_id: UserId,
        group_id: &[u8],
        joiner_mailbox_pk: &[u8],
        profile_key: &Option<Vec<u8>>,
        encrypted_display_name: &Option<Vec<u8>>,
        encrypted_avatar: &Option<Vec<u8>>,
        timestamp: u64,
    ) {
        tracing::info!(
            peer_id = %peer_id,
            group_id = ?group_id,
            has_profile = profile_key.is_some(),
            "Received anonymous group join acknowledgment"
        );

        let anon_group_id = match crate::domain::anon_group::AnonGroupId::try_from_slice(group_id) {
            Some(id) => id,
            None => {
                tracing::warn!("Invalid group_id length for AnonGroupJoinAck");
                return;
            }
        };

        let mailbox_pk: [u8; 32] = match joiner_mailbox_pk.try_into() {
            Ok(pk) => pk,
            Err(_) => {
                tracing::warn!("Invalid mailbox_pk length for AnonGroupJoinAck");
                return;
            }
        };

        // Parse and decrypt profile data
        let profile_key_arr: Option<[u8; 32]> = profile_key.as_ref().and_then(|pk| {
            if pk.len() == 32 {
                let mut key = [0u8; 32];
                key.copy_from_slice(pk);
                Some(key)
            } else {
                None
            }
        });

        use crate::crypto::profile_crypto::{decrypt_avatar, decrypt_display_name};

        let display_name: Option<String> = match (&profile_key_arr, encrypted_display_name) {
            (Some(pk), Some(encrypted)) if !encrypted.is_empty() => {
                decrypt_display_name(pk, encrypted)
                    .ok()
                    .filter(|n| !n.is_empty())
            }
            _ => None,
        };

        let avatar: Option<Vec<u8>> = match (&profile_key_arr, encrypted_avatar) {
            (Some(pk), Some(encrypted)) if !encrypted.is_empty() => {
                decrypt_avatar(pk, encrypted).ok()
            }
            _ => None,
        };

        tracing::debug!(
            peer_id = %peer_id,
            display_name = ?display_name,
            "Decrypted profile for join ack"
        );

        let anon_service = self.anon_group_service.clone();
        let peer_id_clone = peer_id;
        let event_bus = self.event_bus.clone();

        tokio::spawn(async move {
            match anon_service
                .handle_join_ack_ordered(
                    anon_group_id,
                    peer_id_clone,
                    mailbox_pk,
                    profile_key_arr,
                    display_name,
                    avatar,
                    timestamp,
                )
                .await
            {
                // The joiner is in our roster now: their posts that beat
                // this ack in are delivered.
                Ok(()) => {
                    for event in anon_service.release_held_posts(&anon_group_id) {
                        event_bus.emit_domain(event);
                    }
                }
                Err(e) => tracing::error!(
                    group_id = ?anon_group_id,
                    peer_id = %peer_id_clone,
                    error = %e,
                    "Failed to handle join ack"
                ),
            }
        });
    }

    fn handle_anon_group_message(
        &self,
        peer_id: UserId,
        group_id: &[u8],
        inner_content: &crate::proto::content_v1::Content,
    ) {
        let anon_group_id = match crate::domain::anon_group::AnonGroupId::try_from_slice(group_id) {
            Some(id) => id,
            None => {
                tracing::warn!("Invalid group_id length for AnonGroupMessage");
                return;
            }
        };

        tracing::debug!(
            peer_id = %peer_id,
            group_id = %anon_group_id,
            "Received anonymous group message"
        );

        // Delegate to service layer (mirrors handle_anon_dm_message pattern).
        let anon_group_service = self.anon_group_service.clone();
        let event_bus = self.event_bus.clone();
        let inner_content_ref = inner_content.clone();

        let _ = tokio::task::spawn_blocking(move || {
            match anon_group_service.process_incoming_content(
                &anon_group_id,
                &peer_id,
                &inner_content_ref,
            ) {
                Ok(events) => {
                    for event in events {
                        event_bus.emit_domain(event);
                    }
                }
                Err(e) => {
                    tracing::error!(
                        group_id = %anon_group_id,
                        error = %e,
                        "[ANON_GROUP] Failed to process incoming message"
                    );
                }
            }
        });
    }

    fn handle_anon_dm_request(
        &self,
        peer_id: UserId,
        conversation_id: &[u8],
        initiator_mailbox_pk: &[u8],
        intro_message: &Option<String>,
        profile_key: &Option<Vec<u8>>,
        encrypted_display_name: &Option<Vec<u8>>,
        encrypted_avatar: &Option<Vec<u8>>,
    ) {
        tracing::info!(
            peer_id = %peer_id,
            conversation_id_len = conversation_id.len(),
            has_intro = intro_message.is_some(),
            has_encrypted_name = encrypted_display_name.is_some(),
            has_encrypted_avatar = encrypted_avatar.is_some(),
            "Received anonymous DM request"
        );

        let anon_dm_id = match crate::domain::anon_dm::AnonDmId::try_from_slice(conversation_id) {
            Some(id) => id,
            None => {
                tracing::warn!("Invalid conversation_id length for AnonDmRequest");
                return;
            }
        };

        let mailbox_pk: [u8; 32] = match initiator_mailbox_pk.try_into() {
            Ok(pk) => pk,
            Err(_) => {
                tracing::warn!(
                    mailbox_pk_len = initiator_mailbox_pk.len(),
                    "Invalid mailbox_pk length for AnonDmRequest (expected 32 bytes)"
                );
                return;
            }
        };

        let anon_dm_service = self.anon_dm_service.clone();
        let event_bus = self.event_bus.clone();
        let intro_msg = intro_message.clone();
        let profile_key = profile_key.clone();
        let encrypted_display_name = encrypted_display_name.clone();
        let encrypted_avatar = encrypted_avatar.clone();

        tokio::spawn(async move {
            let intro_msg_for_event = intro_msg.clone();
            let result = tokio::task::spawn_blocking(move || {
                anon_dm_service.handle_request_encrypted(
                    anon_dm_id,
                    peer_id,
                    mailbox_pk,
                    intro_msg.as_deref(),
                    profile_key,
                    encrypted_display_name,
                    encrypted_avatar,
                )
            })
            .await;

            match result {
                Ok(Ok((_dm, profile))) => {
                    // Emit request received event
                    event_bus.emit_domain(DomainEvent::AnonDmRequestReceived {
                        conversation_id: anon_dm_id,
                        sender_id: peer_id,
                        sender_mailbox_pk: mailbox_pk,
                        intro_message: intro_msg_for_event,
                        profile_key: profile.profile_key,
                        sender_display_name: Some(profile.display_name.clone()),
                        sender_avatar: profile.avatar.clone(),
                    });

                    // Also emit profile updated event to ensure UI updates avatars immediately
                    event_bus.emit_domain(DomainEvent::AnonDmProfileUpdated {
                        conversation_id: anon_dm_id,
                        user_id: peer_id,
                        display_name: Some(profile.display_name),
                        avatar: profile.avatar,
                        avatar_url: None,
                        bio: None,
                        timestamp: TimestampNanos::now().as_u64(),
                    });
                }
                Ok(Err(e)) => {
                    tracing::debug!(peer_id = %peer_id, error = %e, "Failed to handle AnonDmRequest");
                }
                Err(e) => {
                    tracing::error!(error = %e, "spawn_blocking panicked in AnonDmRequest handler");
                }
            }
        });
    }

    fn handle_anon_dm_accept(
        &self,
        peer_id: UserId,
        conversation_id: &[u8],
        responder_mailbox_pk: &[u8],
        profile_key: &Option<Vec<u8>>,
        encrypted_display_name: &Option<Vec<u8>>,
        encrypted_avatar: &Option<Vec<u8>>,
    ) {
        tracing::info!(
            peer_id = %peer_id,
            conversation_id_len = conversation_id.len(),
            has_encrypted_display_name = encrypted_display_name.is_some(),
            has_encrypted_avatar = encrypted_avatar.as_ref().map(|v| !v.is_empty()).unwrap_or(false),
            "Received anonymous DM acceptance"
        );

        let anon_dm_id = match crate::domain::anon_dm::AnonDmId::try_from_slice(conversation_id) {
            Some(id) => id,
            None => {
                tracing::warn!("Invalid conversation_id length for AnonDmAccept");
                return;
            }
        };

        let mailbox_pk: [u8; 32] = match responder_mailbox_pk.try_into() {
            Ok(pk) => pk,
            Err(_) => {
                tracing::warn!(
                    mailbox_pk_len = responder_mailbox_pk.len(),
                    "Invalid mailbox_pk length for AnonDmAccept (expected 32 bytes)"
                );
                return;
            }
        };

        let anon_dm_service = self.anon_dm_service.clone();
        let event_bus = self.event_bus.clone();
        let client = self.client.clone();
        let profile_key = profile_key.clone();
        let encrypted_display_name = encrypted_display_name.clone();
        let encrypted_avatar = encrypted_avatar.clone();

        tokio::spawn(async move {
            let service_for_handle = anon_dm_service.clone();
            let result = tokio::task::spawn_blocking(move || {
                service_for_handle.handle_acceptance_encrypted(
                    anon_dm_id,
                    peer_id,
                    mailbox_pk,
                    profile_key,
                    encrypted_display_name,
                    encrypted_avatar,
                )
            })
            .await;

            match result {
                Ok(Ok(profile)) => {
                    // Emit acceptance event
                    event_bus.emit_domain(DomainEvent::AnonDmRequestAccepted {
                        conversation_id: anon_dm_id,
                        peer_id,
                        peer_mailbox_pk: mailbox_pk,
                        profile_key: profile.profile_key,
                        peer_display_name: profile.display_name.clone(),
                        peer_avatar: profile.avatar.clone(),
                    });

                    // Also emit profile updated event to ensure UI updates avatars immediately
                    event_bus.emit_domain(DomainEvent::AnonDmProfileUpdated {
                        conversation_id: anon_dm_id,
                        user_id: peer_id,
                        display_name: Some(profile.display_name),
                        avatar: profile.avatar,
                        avatar_url: None,
                        bio: None,
                        timestamp: TimestampNanos::now().as_u64(),
                    });

                    // Auto-send our profile (display name + avatar) back to the
                    // peer now that the Double Ratchet is established. The peer
                    // only saw our ECIES init at request time — this is the
                    // first moment we can share richer identity securely.
                    // Moved here from the UI layer so the profile sync
                    // happens regardless of frontend.
                    let our_name = client.auth_state().get().display_name.clone();
                    let service = anon_dm_service.clone();
                    tokio::spawn(async move {
                        if let Err(e) = service
                            .send_profile_update(&anon_dm_id, our_name, None, None)
                            .await
                        {
                            tracing::warn!(
                                conversation_id = %anon_dm_id,
                                error = %e,
                                "Failed to send profile back to responder after acceptance"
                            );
                        }
                    });
                }
                Ok(Err(e)) => {
                    tracing::debug!(peer_id = %peer_id, error = %e, "Failed to handle AnonDmAccept");
                }
                Err(e) => {
                    tracing::error!(error = %e, "spawn_blocking panicked in AnonDmAccept handler");
                }
            }
        });
    }

    fn handle_anon_dm_message(
        &self,
        peer_id: UserId,
        conversation_id: &[u8],
        inner_content: &crate::proto::content_v1::Content,
    ) {
        let anon_dm_id = match crate::domain::anon_dm::AnonDmId::try_from_slice(conversation_id) {
            Some(id) => id,
            None => {
                tracing::warn!("Invalid conversation_id length for AnonDmMessage");
                return;
            }
        };

        // Send delivery receipt for DataMessages (not typing/reactions/etc.).
        // Attachment records + message storage happen inside
        // `AnonDmService::process_incoming_content` below via the shared
        // dispatcher — no need to duplicate here.
        if let Some(crate::proto::content_v1::content::Content::DataMessage(_)) =
            &inner_content.content
        {
            let dm_service = self.anon_dm_service.clone();
            let dm_id = anon_dm_id;
            let timestamp = inner_content.timestamp;
            tokio::spawn(async move {
                if let Err(e) = dm_service
                    .send_delivery_receipt(&dm_id, vec![timestamp])
                    .await
                {
                    tracing::warn!(error = %e, "[ANON_DM] Failed to send delivery receipt");
                }
            });
        }

        // Delegate to service layer for business logic
        let anon_dm_service = self.anon_dm_service.clone();
        let event_bus = self.event_bus.clone();
        let inner_content_ref = inner_content.clone();

        let _ = tokio::task::spawn_blocking(move || {
            match anon_dm_service.process_incoming_content(
                &anon_dm_id,
                &peer_id,
                &inner_content_ref,
            ) {
                Ok(events) => {
                    for event in events {
                        event_bus.emit_domain(event);
                    }
                }
                Err(e) => {
                    tracing::error!(
                        conversation_id = %anon_dm_id,
                        error = %e,
                        "[ANON_DM] Failed to process incoming message"
                    );
                }
            }
        });
    }
}

/// The additive exact identity is exclusively an authenticated own Saved control.
pub(crate) fn persist_saved_delete_for_me_sync(
    repo: &crate::storage::MessageRepository,
    own: Option<UserId>,
    sender: UserId,
    conversation_id: &str,
    author: &str,
    sent_at: u64,
    message_id: &str,
    saved_scope: bool,
) -> Result<DomainEvent, crate::error::StorageError> {
    use crate::domain::ids::{ChannelId, MessageId};
    use crate::error::StorageError;
    let invalid = || StorageError::InvalidData("Invalid exact own Saved deletion".into());
    let own = own.filter(|own| *own == sender).ok_or_else(invalid)?;
    let conversation: ChannelId = conversation_id.parse().map_err(|_| invalid())?;
    let author: UserId = author.parse().map_err(|_| invalid())?;
    let id: MessageId = message_id.parse().map_err(|_| invalid())?;
    if !saved_scope
        || conversation != ChannelId::from(own)
        || author != own
        || sent_at == 0
        || sent_at > i64::MAX as u64
    {
        return Err(invalid());
    }
    // Transaction rechecks current persisted account and any existing row's
    // identity before recording a scoped tombstone, even when no row exists.
    repo.delete_own_saved_exact(&own, &id, sent_at)?;
    Ok(DomainEvent::DeleteForMeSync {
        conversation_id: conversation,
        target_author_id: own,
        target_message_id: id,
        timestamp: TimestampNanos::now().as_u64(),
        saved_deletion_persisted: true,
    })
}

/// Legacy sync has no storage-kind field. Resolve only within its exact ID,
/// author and clock, and refuse collisions rather than guessing a kind.
fn admit_delete_for_me_sync(
    repo: &crate::storage::MessageRepository,
    bus: &EventBus,
    own: Option<UserId>,
    sender: UserId,
    conversation_id: &str,
    author: &str,
    sent_at: u64,
) -> Result<(), crate::error::StorageError> {
    use crate::domain::message::ConversationType as CT;
    use crate::domain::reaction::ReactionScope;
    use crate::error::StorageError;
    let invalid = || StorageError::InvalidData("Invalid or ambiguous delete-for-me target".into());
    let own = own.filter(|own| *own == sender).ok_or_else(invalid)?;
    if sent_at == 0 || sent_at > i64::MAX as u64 {
        return Err(invalid());
    }
    let id: crate::domain::ids::ChannelId = conversation_id.parse().map_err(|_| invalid())?;
    let author: UserId = author.parse().map_err(|_| invalid())?;
    let mut target = None;
    // Unsupported kinds participate only in collision detection. They never
    // become deletion targets through this own-device server sync protocol.
    for kind in [
        CT::Channel,
        CT::ServerDm,
        CT::SavedMessages,
        CT::AnonDm,
        CT::AnonGroup,
        CT::Mesh,
        CT::MeshDm,
    ] {
        let scope = ReactionScope::new(id, kind);
        if let Some(message) = repo.get_by_reaction_scope(&scope, &author, sent_at)? {
            if target.is_some() {
                return Err(invalid());
            }
            target = Some(message);
        }
    }
    let Some(target) = target else {
        return Ok(());
    };
    match target.conversation_type {
        CT::Channel | CT::ServerDm => {}
        CT::SavedMessages if id == crate::domain::ids::ChannelId::from(own) && author == own => {}
        _ => return Err(invalid()),
    }
    bus.emit_domain(DomainEvent::DeleteForMeSync {
        conversation_id: id,
        target_author_id: author,
        target_message_id: target.id,
        timestamp: TimestampNanos::now().as_u64(),
        saved_deletion_persisted: false,
    });
    Ok(())
}

#[cfg(test)]
#[path = "inbox_delete_for_me_tests.rs"]
mod inbox_delete_for_me_tests;
