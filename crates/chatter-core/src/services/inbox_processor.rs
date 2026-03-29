//! Inbox event processor - handles protocol-level InboxEvent concerns.
//!
//! Processes InboxEvents that are purely protocol logic (no UI dependency),
//! returning `true` if the event was fully handled or `false` if the UI
//! should also process it.

use std::sync::Arc;

use crate::domain::UserId;
use crate::events::EventBus;
use crate::inbox::events::InboxEvent;
use crate::inbox::handlers::DomainEvent;
use crate::services::anon_dm::AnonDmService;
use crate::services::anon_group::AnonGroupService;
use crate::services::attachment::AttachmentService;
use crate::services::channel::ChannelService;
use crate::services::conversation::ConversationHandle;
use crate::services::dm_bootstrap_service::DmBootstrapService;
use crate::services::message::MessageService;
use crate::services::signal_service::SignalService;
use crate::services::user::UserService;
use crate::state::ActiveConversation;

/// Processes inbox events and handles protocol-level concerns.
///
/// Returns `true` from `process()` if the event was fully handled (UI
/// does not need to see it), `false` if UI should also process it.
pub struct InboxEventProcessor {
    message_service: Arc<MessageService>,
    channel_service: Arc<ChannelService>,
    dm_bootstrap_service: Arc<DmBootstrapService>,
    anon_dm_service: Arc<AnonDmService>,
    anon_group_service: Arc<AnonGroupService>,
    #[allow(dead_code)] // Reserved for future identity validation in call handling
    signal_service: Arc<SignalService>,
    user_service: Arc<UserService>,
    attachment_service: Arc<AttachmentService>,
    event_bus: EventBus,
    /// ChatterClient for ConversationHandle construction.
    client: Arc<crate::client::ChatterClient>,
}

impl InboxEventProcessor {
    /// Create a new inbox event processor.
    pub fn new(
        message_service: Arc<MessageService>,
        channel_service: Arc<ChannelService>,
        dm_bootstrap_service: Arc<DmBootstrapService>,
        anon_dm_service: Arc<AnonDmService>,
        anon_group_service: Arc<AnonGroupService>,
        signal_service: Arc<SignalService>,
        user_service: Arc<UserService>,
        attachment_service: Arc<AttachmentService>,
        event_bus: EventBus,
        client: Arc<crate::client::ChatterClient>,
    ) -> Self {
        Self {
            message_service,
            channel_service,
            dm_bootstrap_service,
            anon_dm_service,
            anon_group_service,
            signal_service,
            user_service,
            attachment_service,
            event_bus,
            client,
        }
    }

    /// Create from a CoreServices (convenience constructor).
    pub fn from_client(client: &Arc<crate::client::ChatterClient>) -> Self {
        Self {
            message_service: client.messages().clone(),
            channel_service: client.channels().clone(),
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

    /// Process an inbox event.
    ///
    /// Returns `true` if the event was fully handled (UI does not need it),
    /// `false` if the UI should also process it.
    pub async fn process(&self, event: &InboxEvent) -> bool {
        match event {
            // =================================================================
            // Pure protocol events (fully handled here, return true)
            // =================================================================

            InboxEvent::DmDecryptionFailed { peer_id, error, convergence_failed } => {
                tracing::error!(
                    "DM decryption failed from {}: {} (convergence_failed={})",
                    peer_id, error, convergence_failed
                );
                if *convergence_failed {
                    self.dm_bootstrap_service.handle_convergence_failure(*peer_id);
                }
                true
            }

            InboxEvent::DmRetryRequested { peer_id, original_timestamp } => {
                tracing::info!(
                    peer_id = %peer_id,
                    original_timestamp,
                    "DM retry requested - resetting session"
                );
                self.dm_bootstrap_service.handle_convergence_failure(*peer_id);
                true
            }

            InboxEvent::DmMailboxExpired { peer_id } => {
                tracing::info!(
                    peer_id = %peer_id,
                    "HEALING: DmMailboxExpired event received, triggering re-bootstrap"
                );
                self.dm_bootstrap_service.handle_mailbox_expired(*peer_id);
                true
            }

            InboxEvent::DmPollCompleted { total_messages } => {
                if *total_messages > 0 {
                    tracing::debug!("DM poll completed: {} messages", total_messages);
                }
                true
            }

            InboxEvent::MessageReceived { message_id, .. } => {
                tracing::trace!("Inbox message received: {}", message_id);
                true
            }

            InboxEvent::MessageProcessed { message_id } => {
                tracing::trace!("Inbox message processed: {}", message_id);
                true
            }

            InboxEvent::MessageFailed { message_id, error } => {
                tracing::warn!("Inbox message failed: {} - {}", message_id, error);
                true
            }

            InboxEvent::PollCompleted { count, .. } => {
                if *count > 0 {
                    tracing::trace!("Inbox poll completed: {} messages", count);
                }
                true
            }

            InboxEvent::PollFailed { error } => {
                tracing::warn!("Inbox poll failed: {}", error);
                true
            }

            InboxEvent::AcknowledgeFailed { message_ids, error } => {
                tracing::warn!("Inbox ack failed for {:?}: {}", message_ids, error);
                true
            }

            InboxEvent::GroupMailboxPollCompleted { total_groups, total_messages } => {
                tracing::trace!(
                    total_groups,
                    total_messages,
                    "Group mailbox poll completed"
                );
                true
            }

            InboxEvent::GroupMailboxAnnouncement {
                channel_id,
                member_id,
                mailbox_pk,
                timestamp: _,
            } => {
                self.handle_group_mailbox_announcement(channel_id, member_id, mailbox_pk);
                true
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
                self.event_bus.emit_domain(DomainEvent::CallKeyAnswerReceived {
                    call_id: call_id.clone(),
                    peer_id: *peer_id,
                    ephemeral_public_key: *ephemeral_public_key,
                });
                true
            }

            InboxEvent::GroupCallKeyReceived {
                sender_id,
                call_id,
                key_material,
                key_epoch,
                channel_id,
                timestamp,
            } => {
                tracing::info!(
                    sender_id = %sender_id,
                    call_id = %call_id,
                    key_epoch = key_epoch,
                    channel_id = %channel_id,
                    timestamp = timestamp,
                    "Group call key received - updating encryption"
                );
                self.event_bus.emit_domain(DomainEvent::GroupCallKeyReceived {
                    call_id: call_id.clone(),
                    sender_id: *sender_id,
                    key_material: *key_material,
                    key_epoch: *key_epoch,
                    channel_id: *channel_id,
                });
                true
            }

            InboxEvent::SenderKeyNeeded { channel_id, sender_id } => {
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
                true
            }

            InboxEvent::RespondWithSkdm { channel_id, requester_id, skdm_bytes } => {
                tracing::info!(
                    channel_id = %channel_id,
                    requester_id = %requester_id,
                    skdm_len = skdm_bytes.len(),
                    "Responding with our SKDM"
                );
                let msg_svc = self.message_service.clone();
                let ch_id = *channel_id;
                let req_id = *requester_id;
                let skdm_bytes = skdm_bytes.clone();
                tokio::spawn(async move {
                    let skdm_result: Result<crate::proto::content_v1::SenderKeyDistribution, String> = if skdm_bytes.is_empty() {
                        msg_svc.create_sender_key_for_channel(&ch_id).await
                            .map_err(|e| e.to_string())
                    } else {
                        use prost::Message as ProstMessage;
                        crate::proto::content_v1::SenderKeyDistribution::decode(skdm_bytes.as_slice())
                            .map_err(|e| e.to_string())
                    };

                    match skdm_result {
                        Ok(skdm) => {
                            if let Err(e) = msg_svc.send_skdm_to_member(&ch_id, &req_id, &skdm).await {
                                tracing::warn!(
                                    channel_id = %ch_id,
                                    requester_id = %req_id,
                                    error = %e,
                                    "Failed to send SKDM response"
                                );
                            } else {
                                tracing::info!(
                                    channel_id = %ch_id,
                                    requester_id = %req_id,
                                    "Sent SKDM response to requester"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                channel_id = %ch_id,
                                error = ?e,
                                "Failed to create/decode SKDM"
                            );
                        }
                    }
                });
                true
            }

            // =================================================================
            // Anonymous Group Events (fully handled, return true)
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
                    *peer_id, group_id, group_name, inviter_mailbox_pk,
                    profile_key, encrypted_display_name, encrypted_avatar,
                );
                // Return false - UI needs to add sidebar entry
                false
            }

            InboxEvent::AnonGroupJoinAck {
                peer_id,
                group_id,
                joiner_mailbox_pk,
                timestamp: _,
                profile_key,
                encrypted_display_name,
                encrypted_avatar,
            } => {
                self.handle_anon_group_join_ack(
                    *peer_id, group_id, joiner_mailbox_pk,
                    profile_key, encrypted_display_name, encrypted_avatar,
                );
                true
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

                let anon_group_id = match crate::domain::anon_group::AnonGroupId::try_from_slice(group_id) {
                    Some(id) => id,
                    None => {
                        tracing::warn!("Invalid group_id length for AnonGroupMemberList");
                        return true;
                    }
                };

                let anon_service = self.anon_group_service.clone();
                let members_clone = members.clone();
                let sender_id = *peer_id;
                let _ = tokio::task::spawn_blocking(move || {
                    if let Err(e) = anon_service.handle_member_list(anon_group_id, sender_id, members_clone) {
                        tracing::error!(
                            group_id = ?anon_group_id,
                            error = %e,
                            "Failed to handle member list"
                        );
                    }
                });
                true
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

                let anon_group_id = match crate::domain::anon_group::AnonGroupId::try_from_slice(group_id) {
                    Some(id) => id,
                    None => {
                        tracing::warn!("Invalid group_id length for AnonGroupLeave");
                        return true;
                    }
                };

                let anon_service = self.anon_group_service.clone();
                let peer_id_clone = *peer_id;
                let _ = tokio::task::spawn_blocking(move || {
                    if let Err(e) = anon_service.handle_leave(anon_group_id, peer_id_clone) {
                        tracing::error!(
                            group_id = ?anon_group_id,
                            peer_id = %peer_id_clone,
                            error = %e,
                            "Failed to handle leave"
                        );
                    }
                });
                true
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

                let anon_group_id = match crate::domain::anon_group::AnonGroupId::try_from_slice(group_id) {
                    Some(id) => id,
                    None => {
                        tracing::warn!("Invalid group_id length for AnonGroupKick");
                        return true;
                    }
                };

                let kicked_user = match UserId::try_from_slice(kicked_user_id) {
                    Some(id) => id,
                    None => {
                        tracing::warn!("Invalid kicked_user_id for AnonGroupKick");
                        return true;
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
                true
            }

            InboxEvent::AnonGroupUpdate {
                peer_id,
                group_id,
                update,
                timestamp,
            } => {
                let anon_group_id = match crate::domain::anon_group::AnonGroupId::try_from_slice(group_id) {
                    Some(id) => id,
                    None => {
                        tracing::warn!("Invalid group_id length for AnonGroupUpdate");
                        return true;
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
                    if let Err(e) = anon_service.handle_update(anon_group_id, peer_id_clone, &update_clone) {
                        tracing::error!(
                            group_id = %anon_group_id,
                            error = %e,
                            "Failed to handle update"
                        );
                    }
                });
                true
            }

            InboxEvent::AnonGroupMessage {
                peer_id,
                group_id,
                inner_content,
            } => {
                self.handle_anon_group_message(*peer_id, group_id, inner_content);
                // Return false - UI needs to handle display parts (message list, sidebar, etc.)
                false
            }

            // =================================================================
            // Anonymous DM Events (protocol part handled, some return false for UI)
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
                    *peer_id, conversation_id, initiator_mailbox_pk,
                    intro_message, profile_key, encrypted_display_name, encrypted_avatar,
                );
                // Return false - UI needs to add sidebar entry
                false
            }

            InboxEvent::AnonDmAccept {
                peer_id,
                conversation_id,
                responder_mailbox_pk,
                profile_key,
                responder_display_name: _,
                encrypted_display_name,
                encrypted_avatar,
                timestamp: _,
            } => {
                self.handle_anon_dm_accept(
                    *peer_id, conversation_id, responder_mailbox_pk,
                    profile_key, encrypted_display_name, encrypted_avatar,
                );
                // Return false - UI needs to update sidebar entry
                false
            }

            InboxEvent::AnonDmMessage {
                peer_id,
                conversation_id,
                inner_content,
            } => {
                self.handle_anon_dm_message(*peer_id, conversation_id, inner_content);
                true
            }

            // =================================================================
            // Mixed events (protocol part handled here, return false for UI)
            // =================================================================

            InboxEvent::DmReceived {
                peer_id,
                channel_id,
                timestamp,
                message_id,
                attachments,
                ..
            } => {
                // Create attachment records BEFORE UI handler reads from DB
                if !attachments.is_empty() {
                    let pointers: Vec<_> = attachments.iter()
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
                let handle = ConversationHandle::new(
                    ActiveConversation::MailboxDm { channel_id: *channel_id, peer_id: *peer_id },
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
                        self.event_bus.emit_domain(DomainEvent::MessageReceived { message });
                    }
                }
                true
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
                    let pointers: Vec<_> = attachments.iter()
                        .filter(|p| !p.cdn_key.is_empty())
                        .cloned()
                        .collect();
                    if !pointers.is_empty() {
                        self.attachment_service.create_from_pointers(&pointers);
                    }
                }

                // Protocol: pre-fetch sender profile + send delivery receipt
                if self.user_service.get_user_local(sender_id).ok().flatten().is_none() {
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

                // Send delivery receipt
                let handle = ConversationHandle::new(
                    ActiveConversation::Channel(*channel_id),
                    self.client.clone(),
                );
                let ts = *timestamp;
                tokio::spawn(async move {
                    if let Err(e) = handle.send_delivery_receipt(&[ts]).await {
                        tracing::warn!("Failed to send channel delivery receipt: {}", e);
                    }
                });

                // Emit DomainEvent so domain_handler can display the message
                if let Ok(mid) = message_id.parse::<crate::domain::ids::MessageId>() {
                    if let Ok(Some(message)) = self.message_service.get_message_local(&mid) {
                        self.event_bus.emit_domain(DomainEvent::MessageReceived { message });
                    }
                }
                true
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
                self.event_bus.emit_domain(DomainEvent::CallHangupReceived {
                    call_id: call_id.clone(),
                    peer_id: *peer_id,
                    reason: reason.clone(),
                });
                // UI still needs to clean up call UI state
                false
            }

            InboxEvent::DmBootstrapCompleted { peer_id } => {
                tracing::info!(
                    peer_id = %peer_id,
                    "BOOTSTRAP[4/4] DmBootstrapCompleted event received - will flush pending messages"
                );
                self.dm_bootstrap_service.handle_bootstrap_completed(*peer_id);
                // UI still needs to update composer placeholder and message status
                false
            }

            // =================================================================
            // Ephemeral events — emit DomainEvent for DB persistence, UI also handles
            // =================================================================

            InboxEvent::DmReaction { peer_id, emoji, remove, target_sent_timestamp, target_author_aci, .. } => {
                if let Some(msg) = self.lookup_message(&target_author_aci, *target_sent_timestamp) {
                    let event = if *remove {
                        DomainEvent::ReactionRemoved { message_id: msg.id, author_id: msg.sender_id.unwrap_or(*peer_id), reactor_id: *peer_id, emoji: emoji.clone(), timestamp: *target_sent_timestamp }
                    } else {
                        DomainEvent::ReactionAdded { message_id: msg.id, author_id: msg.sender_id.unwrap_or(*peer_id), reactor_id: *peer_id, emoji: emoji.clone(), timestamp: *target_sent_timestamp }
                    };
                    self.event_bus.emit_domain(event);
                }
                false
            }

            InboxEvent::ChannelReaction { channel_id: _, sender_id, emoji, remove, target_sent_timestamp, target_author_aci, .. } => {
                if let Some(msg) = self.lookup_message(&target_author_aci, *target_sent_timestamp) {
                    let event = if *remove {
                        DomainEvent::ReactionRemoved { message_id: msg.id, author_id: msg.sender_id.unwrap_or(*sender_id), reactor_id: *sender_id, emoji: emoji.clone(), timestamp: *target_sent_timestamp }
                    } else {
                        DomainEvent::ReactionAdded { message_id: msg.id, author_id: msg.sender_id.unwrap_or(*sender_id), reactor_id: *sender_id, emoji: emoji.clone(), timestamp: *target_sent_timestamp }
                    };
                    self.event_bus.emit_domain(event);
                }
                false
            }

            InboxEvent::DmEdit { peer_id, target_sent_timestamp, new_body } => {
                if let Some(msg) = self.lookup_message(&peer_id.to_string(), *target_sent_timestamp) {
                    self.event_bus.emit_domain(DomainEvent::MessageEdited {
                        message_id: msg.id,
                        new_content: crate::domain::message::MessageContent::Text(new_body.clone()),
                        channel_id: msg.conversation_id,
                    });
                }
                false
            }

            InboxEvent::ChannelEdit { channel_id, target_sent_timestamp, new_body, sender_id, .. } => {
                if let Some(msg) = self.lookup_message(&sender_id.to_string(), *target_sent_timestamp) {
                    self.event_bus.emit_domain(DomainEvent::MessageEdited {
                        message_id: msg.id,
                        new_content: crate::domain::message::MessageContent::Text(new_body.clone()),
                        channel_id: *channel_id,
                    });
                }
                false
            }

            InboxEvent::DmDeleteForEveryone { peer_id, target_sent_timestamp } => {
                if let Some(msg) = self.lookup_message(&peer_id.to_string(), *target_sent_timestamp) {
                    self.event_bus.emit_domain(DomainEvent::MessageDeleted {
                        message_id: msg.id,
                        author_id: *peer_id,
                        channel_id: Some(msg.conversation_id),
                    });
                }
                false
            }

            InboxEvent::ChannelDeleteForEveryone { channel_id, sender_id, target_sent_timestamp, .. } => {
                if let Some(msg) = self.lookup_message(&sender_id.to_string(), *target_sent_timestamp) {
                    self.event_bus.emit_domain(DomainEvent::MessageDeleted {
                        message_id: msg.id,
                        author_id: *sender_id,
                        channel_id: Some(*channel_id),
                    });
                }
                false
            }

            InboxEvent::DmPin { peer_id, target_sent_timestamp, unpin, target_author_aci, .. } => {
                if let Some(msg) = self.lookup_message(&target_author_aci, *target_sent_timestamp) {
                    if *unpin {
                        self.event_bus.emit_domain(DomainEvent::MessageUnpinned {
                            message_id: msg.id, unpinner_id: *peer_id,
                            channel_id: msg.conversation_id, target_sent_timestamp: *target_sent_timestamp,
                        });
                    } else {
                        self.event_bus.emit_domain(DomainEvent::MessagePinned {
                            message_id: msg.id, pinner_id: *peer_id,
                            channel_id: msg.conversation_id, target_sent_timestamp: *target_sent_timestamp,
                        });
                    }
                }
                false
            }

            InboxEvent::ChannelPin { channel_id, sender_id, target_sent_timestamp, unpin, target_author_aci, .. } => {
                if let Some(msg) = self.lookup_message(&target_author_aci, *target_sent_timestamp) {
                    if *unpin {
                        self.event_bus.emit_domain(DomainEvent::MessageUnpinned {
                            message_id: msg.id, unpinner_id: *sender_id,
                            channel_id: *channel_id, target_sent_timestamp: *target_sent_timestamp,
                        });
                    } else {
                        self.event_bus.emit_domain(DomainEvent::MessagePinned {
                            message_id: msg.id, pinner_id: *sender_id,
                            channel_id: *channel_id, target_sent_timestamp: *target_sent_timestamp,
                        });
                    }
                }
                false
            }

            InboxEvent::SavedMessageSync { message_id, text, sent_at, attachment_ids } => {
                // Saved message from another device — store locally
                match self.message_service.store_synced_saved_message(
                    message_id, text, *sent_at, attachment_ids.clone(),
                ) {
                    Ok(message) => {
                        self.event_bus.emit_domain(DomainEvent::SavedMessageSynced { message });
                    }
                    Err(e) => {
                        tracing::warn!(
                            message_id,
                            error = %e,
                            "Failed to store synced saved message"
                        );
                    }
                }
                false
            }

            InboxEvent::DmDeleteForMe { conversation_id, target_author_aci, target_sent_timestamp } => {
                if let Some(msg) = self.lookup_message(&target_author_aci, *target_sent_timestamp) {
                    if let Ok(channel_id) = conversation_id.parse() {
                        self.event_bus.emit_domain(DomainEvent::DeleteForMeSync {
                            conversation_id: channel_id,
                            target_author_id: msg.sender_id.unwrap_or_else(crate::domain::ids::UserId::new),
                            target_message_id: msg.id,
                            timestamp: crate::util::time::TimestampNanos::now().as_u64(),
                        });
                    }
                }
                false
            }

            InboxEvent::ChannelDeleteForMe { conversation_id, target_author_aci, target_sent_timestamp } => {
                if let Some(msg) = self.lookup_message(&target_author_aci, *target_sent_timestamp) {
                    if let Ok(channel_id) = conversation_id.parse() {
                        self.event_bus.emit_domain(DomainEvent::DeleteForMeSync {
                            conversation_id: channel_id,
                            target_author_id: msg.sender_id.unwrap_or_else(crate::domain::ids::UserId::new),
                            target_message_id: msg.id,
                            timestamp: crate::util::time::TimestampNanos::now().as_u64(),
                        });
                    }
                }
                false
            }

            InboxEvent::ChannelReceipt { channel_id: _, sender_id: _, receipt_type, timestamps, .. } => {
                let status = match receipt_type {
                    1 => "delivered",
                    2 => "read",
                    3 => "read", // viewed = read
                    _ => "sent",
                };
                for ts in timestamps {
                    if let Some(ref svc) = Some(&self.message_service) {
                        let _ = svc.update_delivery_status_by_timestamp(*ts, status);
                    }
                }
                false
            }

            // =================================================================
            // Events the UI handles entirely (no DB persistence needed)
            // =================================================================
            _ => false,
        }
    }

    // =====================================================================
    // Helper methods
    // =====================================================================

    /// Look up a message by sender UUID string + sent_at timestamp.
    fn lookup_message(&self, sender_id_str: &str, sent_timestamp: u64) -> Option<crate::domain::Message> {
        let sender_id: UserId = sender_id_str.parse().ok()?;
        self.message_service.get_by_sender_timestamp(&sender_id, sent_timestamp).ok().flatten()
    }

    fn handle_group_mailbox_announcement(
        &self,
        channel_id: &crate::domain::ChannelId,
        member_id: &UserId,
        mailbox_pk: &[u8],
    ) {
        tracing::info!(
            channel_id = %channel_id,
            member_id = %member_id,
            mailbox_pk_len = mailbox_pk.len(),
            "Group mailbox announcement received - storing member mailbox"
        );

        if mailbox_pk.len() == 32 {
            let mut pk_array = [0u8; 32];
            pk_array.copy_from_slice(mailbox_pk);

            match self.channel_service.store_member_mailbox(channel_id, member_id, &pk_array) {
                Ok(is_new) => {
                    tracing::info!(
                        channel_id = %channel_id,
                        member_id = %member_id,
                        is_new,
                        "Stored member mailbox for group fanout"
                    );

                    if is_new {
                        let message_service = self.message_service.clone();
                        let channel_id_clone = *channel_id;
                        let member_id_clone = *member_id;
                        tokio::spawn(async move {
                            if let Err(e) = message_service
                                .respond_with_mailbox_to_member(&channel_id_clone, &member_id_clone)
                                .await
                            {
                                tracing::warn!(
                                    channel_id = %channel_id_clone,
                                    member_id = %member_id_clone,
                                    error = %e,
                                    "Failed to respond with our mailbox to new member"
                                );
                            }
                        });
                    }
                }
                Err(e) => {
                    tracing::error!(
                        channel_id = %channel_id,
                        member_id = %member_id,
                        error = %e,
                        "Failed to store member mailbox"
                    );
                }
            }
        } else {
            tracing::warn!(
                channel_id = %channel_id,
                member_id = %member_id,
                mailbox_pk_len = mailbox_pk.len(),
                "Invalid mailbox public key length (expected 32)"
            );
        }
    }

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

        use crate::crypto::profile_crypto::{decrypt_display_name, decrypt_avatar};

        let inviter_display_name: Option<String> = match (&profile_key_arr, encrypted_display_name) {
            (Some(pk), Some(encrypted)) if !encrypted.is_empty() => {
                decrypt_display_name(pk, encrypted).ok().filter(|n| !n.is_empty())
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
        self.event_bus.emit_domain(DomainEvent::AnonGroupInviteReceived {
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

        use crate::crypto::profile_crypto::{decrypt_display_name, decrypt_avatar};

        let display_name: Option<String> = match (&profile_key_arr, encrypted_display_name) {
            (Some(pk), Some(encrypted)) if !encrypted.is_empty() => {
                decrypt_display_name(pk, encrypted).ok().filter(|n| !n.is_empty())
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

        tokio::spawn(async move {
            if let Err(e) = anon_service.handle_join_ack(
                anon_group_id,
                peer_id_clone,
                mailbox_pk,
                profile_key_arr,
                display_name,
                avatar,
            ).await {
                tracing::error!(
                    group_id = ?anon_group_id,
                    peer_id = %peer_id_clone,
                    error = %e,
                    "Failed to handle join ack"
                );
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

        use crate::proto::content_v1::content;

        match &inner_content.content {
            // Handle call signaling events (protocol-only, no UI needed)
            Some(content::Content::AnonCallOffer(offer)) => {
                tracing::info!(
                    group_id = %anon_group_id,
                    sender_id = %peer_id,
                    room_name = %offer.room_name,
                    "Received anonymous group call offer"
                );
                self.event_bus.emit_domain(DomainEvent::AnonGroupCallOfferReceived {
                    group_id: anon_group_id,
                    sender_id: peer_id,
                    call_room_id: offer.call_room_id.clone(),
                    room_name: offer.room_name.clone(),
                    call_type: offer.call_type,
                    caller_display_name: offer.caller_display_name.clone(),
                    call_encryption_key: offer.call_encryption_key.clone(),
                    timestamp: offer.timestamp,
                });
            }

            Some(content::Content::AnonCallAnswer(answer)) => {
                tracing::info!(
                    group_id = %anon_group_id,
                    sender_id = %peer_id,
                    accepted = answer.accepted,
                    "Received anonymous group call answer"
                );
                self.event_bus.emit_domain(DomainEvent::AnonGroupCallAnswerReceived {
                    group_id: anon_group_id,
                    sender_id: peer_id,
                    call_room_id: answer.call_room_id.clone(),
                    accepted: answer.accepted,
                    callee_display_name: answer.callee_display_name.clone(),
                    timestamp: answer.timestamp,
                });
            }

            Some(content::Content::AnonCallHangup(hangup)) => {
                tracing::info!(
                    group_id = %anon_group_id,
                    sender_id = %peer_id,
                    "Received anonymous group call hangup"
                );
                self.event_bus.emit_domain(DomainEvent::AnonGroupCallHangupReceived {
                    group_id: anon_group_id,
                    sender_id: peer_id,
                    call_room_id: hangup.call_room_id.clone(),
                    reason: hangup.reason,
                    timestamp: hangup.timestamp,
                });
            }

            // Handle receipt (protocol-only logging)
            Some(content::Content::Receipt(receipt)) => {
                tracing::debug!(
                    group_id = %anon_group_id,
                    peer_id = %peer_id,
                    receipt_type = receipt.r#type,
                    timestamps = ?receipt.timestamps,
                    "Received anon group receipt (not yet implemented in UI)"
                );
            }

            // Handle data messages (store + delete + attachment download)
            Some(content::Content::DataMessage(data_msg)) => {
                // Handle delete
                if let Some(delete) = &data_msg.delete {
                    let _ = self.anon_group_service.delete_message_by_timestamp(
                        &anon_group_id, &peer_id, delete.target_sent_timestamp,
                    );
                } else {
                    // Store the message to DB
                    let text = &data_msg.body;
                    let sent_at = inner_content.timestamp;

                    if !text.is_empty() || !data_msg.attachments.is_empty() {
                        // Create attachment records
                        let attachment_ids = if !data_msg.attachments.is_empty() {
                            let pointers: Vec<_> = data_msg.attachments.iter()
                                .filter(|p| !p.cdn_key.is_empty())
                                .cloned()
                                .collect();
                            if !pointers.is_empty() {
                                self.attachment_service.create_from_pointers(&pointers);
                            }
                            self.anon_group_service.create_attachments_from_pointers(&data_msg.attachments)
                        } else {
                            Vec::new()
                        };

                        // Build content JSON (with quote if present)
                        let store_result = if let Some(ref q) = data_msg.quote {
                            let content_json = serde_json::json!({
                                "text": text,
                                "quote": {
                                    "author_id": q.author_aci,
                                    "timestamp": q.id,
                                    "preview": q.text,
                                }
                            }).to_string();
                            self.anon_group_service.store_message_with_content_json(
                                &anon_group_id, &peer_id, 3, &content_json, &attachment_ids, sent_at,
                            )
                        } else {
                            self.anon_group_service.store_message_with_attachments(
                                &anon_group_id, &peer_id, text, &attachment_ids, sent_at,
                            )
                        };

                        match store_result {
                            Ok(msg_id) => {
                                tracing::debug!(group_id = %anon_group_id, message_id = %msg_id, "Stored anon group message");
                                // Emit domain event so UI updates in real-time
                                self.event_bus.emit_domain(DomainEvent::AnonGroupMessageReceived {
                                    group_id: anon_group_id,
                                    sender_id: peer_id,
                                    message_id: msg_id,
                                    inner_content: inner_content.clone(),
                                    sent_at,
                                });
                            }
                            Err(e) => tracing::error!(group_id = %anon_group_id, error = %e, "Failed to store anon group message"),
                        }
                    }
                }
            }

            // Persist ephemeral actions to anon group DB and emit domain events for real-time UI
            Some(content::Content::Reaction(reaction)) => {
                let message_id = crate::domain::MessageId::from_timestamp(reaction.target_sent_timestamp);
                let author_id = reaction.target_author_aci.parse().unwrap_or(peer_id);

                if reaction.remove {
                    let _ = self.anon_group_service.remove_reaction(&anon_group_id, reaction.target_sent_timestamp, &peer_id, &reaction.emoji);
                    self.event_bus.emit_domain(DomainEvent::ReactionRemoved {
                        message_id,
                        author_id,
                        reactor_id: peer_id,
                        emoji: reaction.emoji.clone(),
                        timestamp: reaction.target_sent_timestamp,
                    });
                } else {
                    let _ = self.anon_group_service.add_reaction(&anon_group_id, reaction.target_sent_timestamp, &peer_id, &reaction.emoji);
                    self.event_bus.emit_domain(DomainEvent::ReactionAdded {
                        message_id,
                        author_id,
                        reactor_id: peer_id,
                        emoji: reaction.emoji.clone(),
                        timestamp: reaction.target_sent_timestamp,
                    });
                }
            }
            Some(content::Content::Pin(pin)) => {
                let new_pinned = !pin.unpin;
                let _ = self.anon_group_service.set_message_pinned(
                    &anon_group_id, pin.target_sent_timestamp, new_pinned,
                );

                // Look up real message ID from DB for correct UI tracking
                let message_id = self.anon_group_service
                    .get_message_by_timestamp(&anon_group_id, pin.target_sent_timestamp)
                    .ok().flatten()
                    .map(|m| m.id)
                    .unwrap_or_else(|| crate::domain::MessageId::from_timestamp(pin.target_sent_timestamp));
                let channel_id: crate::domain::ChannelId = anon_group_id.into();

                if pin.unpin {
                    self.event_bus.emit_domain(DomainEvent::MessageUnpinned {
                        message_id,
                        unpinner_id: peer_id,
                        channel_id,
                        target_sent_timestamp: pin.target_sent_timestamp,
                    });
                } else {
                    self.event_bus.emit_domain(DomainEvent::MessagePinned {
                        message_id,
                        pinner_id: peer_id,
                        channel_id,
                        target_sent_timestamp: pin.target_sent_timestamp,
                    });
                }
            }
            Some(content::Content::Edit(edit)) => {
                let new_text = edit.data_message.as_ref().map(|dm| dm.body.clone()).unwrap_or_default();
                if let Ok(true) = self.anon_group_service.update_message_text_by_sender(
                    &anon_group_id, &peer_id, edit.target_sent_timestamp, &new_text,
                ) {
                    self.event_bus.emit_domain(DomainEvent::AnonGroupMessageEdited {
                        group_id: anon_group_id,
                        message_timestamp: edit.target_sent_timestamp,
                        new_text,
                    });
                }
            }
            // ============================================================
            // Group management messages (arrive as top-level Content, not
            // wrapped in AnonGroupMessage — routed here by decryption.rs)
            // ============================================================
            Some(content::Content::AnonGroupMemberList(list)) => {
                // SECURITY: Verify sender is admin or owner
                match self.anon_group_service.get_member_role(&anon_group_id, &peer_id) {
                    Ok(Some(role)) if role.can_broadcast_member_list() => {}
                    _ => {
                        tracing::warn!(
                            sender_id = %peer_id,
                            group_id = %anon_group_id,
                            "Rejected member list from unauthorized sender"
                        );
                        return;
                    }
                }

                let members = list.members.clone();
                if let Err(e) = self.anon_group_service.handle_member_list(
                    anon_group_id, peer_id, members.clone(),
                ) {
                    tracing::warn!(group_id = %anon_group_id, error = %e, "Failed to handle member list");
                }

                self.event_bus.emit_domain(DomainEvent::AnonGroupMemberListReceived {
                    group_id: anon_group_id,
                    members,
                });

                tracing::info!(group_id = %anon_group_id, sender_id = %peer_id, "Processed group member list");
            }

            Some(content::Content::AnonGroupLeave(_)) => {
                if let Err(e) = self.anon_group_service.handle_leave(anon_group_id, peer_id) {
                    tracing::warn!(error = %e, "Failed to handle group leave");
                }
                self.event_bus.emit_domain(DomainEvent::AnonGroupMemberLeft {
                    group_id: anon_group_id,
                    user_id: peer_id,
                });
                tracing::info!(group_id = %anon_group_id, sender_id = %peer_id, "Member left group");
            }

            Some(content::Content::AnonGroupKick(kick)) => {
                let kicked_user_id = match UserId::try_from_slice(&kick.kicked_user_id) {
                    Some(id) => id,
                    None => {
                        tracing::warn!("Invalid kicked_user_id in AnonGroupKick");
                        return;
                    }
                };
                if let Err(e) = self.anon_group_service.handle_kick(
                    anon_group_id, kicked_user_id, peer_id,
                    &kick.admin_signature, kick.timestamp,
                ) {
                    tracing::warn!(error = %e, "Failed to handle group kick");
                }
                self.event_bus.emit_domain(DomainEvent::AnonGroupMemberKicked {
                    group_id: anon_group_id,
                    user_id: kicked_user_id,
                    kicked_by: peer_id,
                });
                tracing::info!(group_id = %anon_group_id, kicked = %kicked_user_id, by = %peer_id, "Member kicked");
            }

            Some(content::Content::AnonGroupProfileShare(profile)) => {
                // Verify sender is a member
                if !self.anon_group_service.is_member(&anon_group_id, &peer_id).unwrap_or(false) {
                    tracing::debug!(sender_id = %peer_id, "Dropped profile share from non-member");
                    return;
                }

                let profile_key = profile.profile_key.as_ref().and_then(|pk| {
                    if pk.len() == 32 { let mut k = [0u8; 32]; k.copy_from_slice(pk); Some(k) } else { None }
                });

                use crate::crypto::profile_crypto::{decrypt_avatar, decrypt_display_name};

                let display_name = match (&profile_key, &profile.encrypted_display_name) {
                    (Some(pk), Some(enc)) if !enc.is_empty() => decrypt_display_name(pk, enc).ok().filter(|n| !n.is_empty()),
                    _ => None,
                };
                let avatar = match (&profile_key, &profile.encrypted_avatar) {
                    (Some(pk), Some(enc)) if !enc.is_empty() => decrypt_avatar(pk, enc).ok(),
                    _ => None,
                };

                let svc = self.anon_group_service.clone();
                tokio::spawn(async move {
                    if let Err(e) = svc.handle_profile_share(
                        anon_group_id, peer_id, profile_key, display_name.clone(), avatar.clone(),
                    ).await {
                        tracing::warn!(error = %e, "Failed to handle profile share");
                    }
                });

                tracing::debug!(group_id = %anon_group_id, sender_id = %peer_id, "Processed profile share");
            }

            Some(content::Content::AnonGroupUpdate(update)) => {
                // Group metadata update (name, description, etc.)
                self.event_bus.emit_domain(DomainEvent::AnonGroupUpdated {
                    group_id: anon_group_id,
                    update: update.clone(),
                    updated_by: peer_id,
                });
                tracing::info!(group_id = %anon_group_id, sender_id = %peer_id, "Group updated");
            }

            _ => {
                // Typing and unknown content types — UI only
            }
        }
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
            }).await;

            match result {
                Ok(Ok((_dm, profile))) => {
                    event_bus.emit_domain(DomainEvent::AnonDmRequestReceived {
                        conversation_id: anon_dm_id,
                        sender_id: peer_id,
                        sender_mailbox_pk: mailbox_pk,
                        intro_message: intro_msg_for_event,
                        profile_key: profile.profile_key,
                        sender_display_name: Some(profile.display_name.clone()),
                        sender_avatar: profile.avatar.clone(),
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
        let profile_key = profile_key.clone();
        let encrypted_display_name = encrypted_display_name.clone();
        let encrypted_avatar = encrypted_avatar.clone();

        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                anon_dm_service.handle_acceptance_encrypted(
                    anon_dm_id,
                    peer_id,
                    mailbox_pk,
                    profile_key,
                    encrypted_display_name,
                    encrypted_avatar,
                )
            }).await;

            match result {
                Ok(Ok(profile)) => {
                    event_bus.emit_domain(DomainEvent::AnonDmRequestAccepted {
                        conversation_id: anon_dm_id,
                        peer_id,
                        peer_mailbox_pk: mailbox_pk,
                        profile_key: profile.profile_key,
                        peer_display_name: profile.display_name.clone(),
                        peer_avatar: profile.avatar.clone(),
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

        // Extract attachments - create records first, then spawn download
        if let Some(crate::proto::content_v1::content::Content::DataMessage(data_msg)) = &inner_content.content {
            if !data_msg.attachments.is_empty() {
                let download_pointers: Vec<_> = data_msg.attachments
                    .iter()
                    .filter(|p| !p.cdn_key.is_empty())
                    .cloned()
                    .collect();

                if !download_pointers.is_empty() {
                    // Create attachment records BEFORE spawning download
                    self.attachment_service.create_from_pointers(&download_pointers);
                }
            }
        }

        // Send delivery receipt for DataMessages (not typing/reactions/etc.)
        if let Some(crate::proto::content_v1::content::Content::DataMessage(_)) = &inner_content.content {
            let dm_service = self.anon_dm_service.clone();
            let dm_id = anon_dm_id;
            let timestamp = inner_content.timestamp;
            tokio::spawn(async move {
                if let Err(e) = dm_service.send_delivery_receipt(&dm_id, vec![timestamp]).await {
                    tracing::warn!(error = %e, "[ANON_DM] Failed to send delivery receipt");
                }
            });
        }

        // Delegate to service layer for business logic
        let anon_dm_service = self.anon_dm_service.clone();
        let event_bus = self.event_bus.clone();
        let inner_content_ref = inner_content.clone();

        let _ = tokio::task::spawn_blocking(move || {
            match anon_dm_service.process_incoming_content(&anon_dm_id, &peer_id, &inner_content_ref) {
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
