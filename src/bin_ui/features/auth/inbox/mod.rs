//! Inbox event handling for UI updates.
//!
//! Processes incoming DM and channel events to update the UI in real-time.
//!
//! # Architecture
//!
//! The inbox system uses a two-layer event architecture:
//!
//! 1. **InboxEvent** (raw polling events) → **InboxEventProcessor** (protocol handling)
//! 2. **DomainEvent** (clean domain events) → **domain_handler** (UI updates only)
//!
//! The old `handle_inbox_event` function still exists for backwards compatibility
//! but should be migrated to use `handle_domain_event` for cleaner separation.
//!
//! # Service-Based Architecture
//!
//! This module uses UIContext for all service access:
//! - Services for business operations
//! - State stores for UI updates
//! - Event bus for domain events

mod anon_dm_actions;
mod anon_group_events;
mod anon_group_handlers;
pub mod attachment_download;
mod channel_handlers;
pub mod dm;
pub mod domain_handler;
mod pin_persistence;
pub mod typing;
pub(crate) mod unified_actions;
pub mod utils;

pub use anon_group_events::spawn_anon_group_event_handler;
pub use pin_persistence::spawn_pin_persistence_handler;

pub use anon_dm_actions::{
    handle_anon_dm_delete,
    handle_anon_dm_edit,
    handle_anon_dm_message_deleted,
    handle_anon_dm_message_edited,
    handle_anon_dm_pin,
    handle_anon_dm_reaction,
    handle_anon_dm_receipt,
    handle_anon_dm_typing,
};

use std::sync::Arc;

use slint::Model;

use chatter_linux::app::UIContext;
use chatter_linux::inbox::handlers::DomainEvent;
use chatter_linux::inbox::InboxEvent;

use crate::bin_ui::features::calls::cleanup_call_ui_state;
use crate::bin_ui::features::chat::{send_dm_delivery_receipt, send_channel_delivery_receipt};
use crate::MainWindow;

// Re-export public items
pub use utils::update_reaction_summary;

/// Handle inbox events and update UI accordingly.
pub async fn handle_inbox_event(
    event: InboxEvent,
    weak_window: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<UIContext>,
) {
    match event {
        InboxEvent::DmReceived {
            peer_id,
            channel_id,
            text,
            timestamp,
            message_id,
            attachments,
        } => {
            dm::handle_dm_received(weak_window, ui_ctx, peer_id, channel_id, text.clone(), timestamp, message_id, attachments);

            // Send delivery receipt back to sender (Signal Protocol)
            tracing::info!(
                peer_id = %peer_id,
                timestamp,
                "DELIVERY_RECEIPT: Triggering delivery receipt for received DM"
            );
            send_dm_delivery_receipt(
                peer_id,
                timestamp,
                ui_ctx.message_service.clone(),
                ui_ctx.mailbox_manager.clone(),
                ui_ctx.runtime_handle.clone(),
            );
        }
        InboxEvent::DmDecryptionFailed { peer_id, error, convergence_failed } => {
            tracing::error!(
                "DM decryption failed from {}: {} (convergence_failed={})",
                peer_id, error, convergence_failed
            );

            if convergence_failed {
                ui_ctx.dm_bootstrap_service.handle_convergence_failure(peer_id);
            }
        }
        InboxEvent::DmRetryRequested { peer_id, original_timestamp } => {
            tracing::info!(
                peer_id = %peer_id,
                original_timestamp,
                "DM retry requested - resetting session"
            );

            ui_ctx.dm_bootstrap_service.handle_convergence_failure(peer_id);
        }
        InboxEvent::DmMailboxExpired { peer_id } => {
            tracing::info!(
                peer_id = %peer_id,
                "HEALING: DmMailboxExpired event received, triggering re-bootstrap"
            );
            ui_ctx.dm_bootstrap_service.handle_mailbox_expired(peer_id);
        }
        InboxEvent::DmPollCompleted { total_messages } => {
            if total_messages > 0 {
                tracing::debug!("DM poll completed: {} messages", total_messages);
            }
        }
        InboxEvent::ChannelMessageReceived {
            channel_id,
            sender_id,
            text,
            timestamp,
            attachments,
            ..
        } => {
            // Pre-fetch sender's profile if not cached
            if ui_ctx.user_service.get_user_local(&sender_id).ok().flatten().is_none() {
                tracing::debug!(
                    sender_id = %sender_id,
                    "Fetching sender profile for channel message"
                );
                match ui_ctx.user_service.get_user_profile(&sender_id).await {
                    Ok(profile) => {
                        tracing::debug!(
                            sender_id = %sender_id,
                            display_name = %profile.display_name,
                            "Fetched and cached sender profile"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            sender_id = %sender_id,
                            error = %e,
                            "Failed to fetch sender profile, will show UUID"
                        );
                    }
                }
            }

            channel_handlers::handle_channel_message_received(
                weak_window,
                ui_ctx,
                channel_id,
                text,
            );

            // Download attachments in background
            if !attachments.is_empty() {
                channel_handlers::trigger_channel_attachment_download(
                    weak_window,
                    ui_ctx,
                    channel_id,
                    attachments,
                );
            }

            // Send delivery receipt for channel message
            send_channel_delivery_receipt(
                channel_id,
                timestamp,
                ui_ctx.message_service.clone(),
                ui_ctx.group_coordinator.clone(),
                ui_ctx.runtime_handle.clone(),
            );
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
            if count > 0 {
                tracing::trace!("Inbox poll completed: {} messages", count);
            }
        }
        InboxEvent::PollFailed { error } => {
            tracing::warn!("Inbox poll failed: {}", error);
        }
        InboxEvent::AcknowledgeFailed { message_ids, error } => {
            tracing::warn!("Inbox ack failed for {:?}: {}", message_ids, error);
        }
        InboxEvent::DmUnreadCountUpdated {
            channel_id,
            peer_id,
            unread_count,
        } => {
            dm::handle_dm_unread_count(weak_window, channel_id, peer_id, unread_count);
        }
        InboxEvent::DmTyping { peer_id, action } => {
            tracing::info!(
                peer_id = %peer_id,
                action,
                action_name = if action == 1 { "STARTED" } else { "STOPPED" },
                "INBOX_EVENT: Received DmTyping event, calling handler"
            );
            dm::handle_dm_typing(weak_window, peer_id, action);
        }
        InboxEvent::DmReceipt {
            peer_id,
            receipt_type,
            timestamps,
        } => {
            dm::handle_dm_receipt(weak_window, peer_id, receipt_type, timestamps);
        }
        InboxEvent::DmReaction {
            peer_id,
            emoji,
            remove,
            target_sent_timestamp,
            ..
        } => {
            dm::handle_dm_reaction(weak_window, peer_id, emoji, remove, target_sent_timestamp);
        }
        InboxEvent::DmPin {
            peer_id,
            target_sent_timestamp,
            unpin,
            ..
        } => {
            dm::handle_dm_pin(weak_window, peer_id, ui_ctx.pinned_state.clone(), target_sent_timestamp, unpin);
        }
        InboxEvent::DmEdit {
            peer_id,
            target_sent_timestamp,
            new_body,
        } => {
            dm::handle_dm_edit(weak_window, peer_id, target_sent_timestamp, new_body);
        }
        InboxEvent::DmDeleteForMe {
            conversation_id,
            target_sent_timestamp,
            ..
        } => {
            dm::handle_dm_delete_for_me(weak_window, conversation_id, target_sent_timestamp);
        }
        InboxEvent::DmDeleteForEveryone {
            peer_id,
            target_sent_timestamp,
        } => {
            dm::handle_dm_delete_for_everyone(weak_window, peer_id, target_sent_timestamp);
        }
        InboxEvent::ChannelTyping {
            channel_id,
            sender_id,
            action,
        } => {
            channel_handlers::handle_channel_typing(weak_window, ui_ctx, channel_id, sender_id, action);
        }
        InboxEvent::ChannelReaction {
            channel_id,
            emoji,
            remove,
            target_sent_timestamp,
            ..
        } => {
            channel_handlers::handle_channel_reaction(weak_window, channel_id, emoji, remove, target_sent_timestamp);
        }
        InboxEvent::ChannelPin {
            channel_id,
            target_sent_timestamp,
            unpin,
            ..
        } => {
            channel_handlers::handle_channel_pin(weak_window, ui_ctx.pinned_state.clone(), channel_id, target_sent_timestamp, unpin);
        }
        InboxEvent::ChannelEdit {
            channel_id,
            target_sent_timestamp,
            new_body,
            ..
        } => {
            channel_handlers::handle_channel_edit(weak_window, channel_id, target_sent_timestamp, new_body);
        }
        InboxEvent::ChannelDeleteForEveryone {
            channel_id,
            target_sent_timestamp,
            ..
        } => {
            channel_handlers::handle_channel_delete_for_everyone(weak_window, channel_id, target_sent_timestamp);
        }
        InboxEvent::ChannelDeleteForMe {
            conversation_id,
            target_sent_timestamp,
            ..
        } => {
            channel_handlers::handle_channel_delete_for_me(weak_window, conversation_id, target_sent_timestamp);
        }
        InboxEvent::ChannelReceipt {
            channel_id,
            receipt_type,
            timestamps,
            ..
        } => {
            channel_handlers::handle_channel_receipt(weak_window, ui_ctx, channel_id, receipt_type, timestamps);
        }
        InboxEvent::ChannelUnreadCountUpdated {
            channel_id,
            unread_count,
        } => {
            channel_handlers::handle_channel_unread_count(weak_window, channel_id, unread_count);
        }
        InboxEvent::GroupMailboxPollCompleted {
            total_groups,
            total_messages,
        } => {
            tracing::trace!(
                total_groups,
                total_messages,
                "Group mailbox poll completed"
            );
        }
        InboxEvent::GroupMailboxAnnouncement {
            channel_id,
            member_id,
            mailbox_pk,
            timestamp: _,
        } => {
            tracing::info!(
                channel_id = %channel_id,
                member_id = %member_id,
                mailbox_pk_len = mailbox_pk.len(),
                "Group mailbox announcement received - storing member mailbox"
            );

            // Store the member's mailbox using GroupMailboxManager
            use chatter_linux::mailbox::group::MemberMailbox;
            use chrono::{Utc, Duration};

            let mailbox_manager = ui_ctx.group_mailbox_manager.clone();

            // Convert mailbox_pk to fixed-size array
            if mailbox_pk.len() == 32 {
                let mut pk_array = [0u8; 32];
                pk_array.copy_from_slice(&mailbox_pk);

                let member_mailbox = MemberMailbox::new(
                    member_id,
                    pk_array,
                    Some(Utc::now() + Duration::days(7)),
                );

                let already_have_mailbox = mailbox_manager
                    .load_group_state(&channel_id)
                    .ok()
                    .flatten()
                    .map(|state| state.get_member_mailbox(&member_id).is_some())
                    .unwrap_or(false);

                if let Err(e) = mailbox_manager.save_member_mailbox(&channel_id, &member_mailbox) {
                    tracing::error!(
                        channel_id = %channel_id,
                        member_id = %member_id,
                        error = %e,
                        "Failed to store member mailbox"
                    );
                } else {
                    tracing::info!(
                        channel_id = %channel_id,
                        member_id = %member_id,
                        already_have_mailbox,
                        "Successfully stored member mailbox for group fanout"
                    );

                    if !already_have_mailbox {
                        let message_service = ui_ctx.message_service.clone();
                        let channel_id_clone = channel_id;
                        let member_id_clone = member_id;
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
                                } else {
                                    tracing::info!(
                                        channel_id = %channel_id_clone,
                                        member_id = %member_id_clone,
                                        "Responded with our mailbox to new member"
                                    );
                                }
                            });
                    } else {
                        tracing::debug!(
                            channel_id = %channel_id,
                            member_id = %member_id,
                            "Skipping mailbox response (already have their mailbox, avoiding ping-pong)"
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
        InboxEvent::FriendRequestsUpdated { received, sent } => {
            tracing::debug!(
                received_count = received.len(),
                sent_count = sent.len(),
                "Friend requests updated"
            );
            let weak = weak_window.clone();
            // Extract string data that is Send-safe, create FriendRequestData inside event loop
            let received_data: Vec<_> = received
                .into_iter()
                .map(|r| (r.user_id, r.display_name, r.username, r.avatar_url))
                .collect();
            let sent_data: Vec<_> = sent
                .into_iter()
                .map(|r| (r.user_id, r.display_name, r.username, r.avatar_url))
                .collect();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    // Create FriendRequestData inside event loop (slint::Image is not Send)
                    let received_ui: Vec<crate::FriendRequestData> = received_data
                        .into_iter()
                        .map(|(user_id, display_name, username, avatar_url)| crate::FriendRequestData {
                            user_id: user_id.into(),
                            display_name: display_name.into(),
                            username: username.into(),
                            avatar_url: avatar_url.into(),
                            avatar_source: slint::Image::default(),
                            has_avatar: false,
                            sent_at: "".into(),
                        })
                        .collect();
                    let sent_ui: Vec<crate::FriendRequestData> = sent_data
                        .into_iter()
                        .map(|(user_id, display_name, username, avatar_url)| crate::FriendRequestData {
                            user_id: user_id.into(),
                            display_name: display_name.into(),
                            username: username.into(),
                            avatar_url: avatar_url.into(),
                            avatar_source: slint::Image::default(),
                            has_avatar: false,
                            sent_at: "".into(),
                        })
                        .collect();
                    w.set_friends_received_requests(slint::ModelRc::new(slint::VecModel::from(received_ui)));
                    w.set_friends_sent_requests(slint::ModelRc::new(slint::VecModel::from(sent_ui)));
                }
            });
        }
        InboxEvent::FriendAccepted {
            friend_user_id,
            channel_id,
        } => {
            tracing::info!(
                friend_user_id = %friend_user_id,
                channel_id = %channel_id,
                "Friend accepted event received - adding DM to sidebar"
            );

            dm::handle_friend_accepted(
                weak_window,
                ui_ctx,
                friend_user_id,
                channel_id,
            )
            .await;
        }
        InboxEvent::DmBootstrapCompleted { peer_id } => {
            tracing::info!(
                peer_id = %peer_id,
                "BOOTSTRAP[4/4] DmBootstrapCompleted event received - will flush pending messages"
            );

            let peer_id_str = peer_id.to_string();
            let weak = weak_window.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_composer_placeholder("Type a message...".into());

                    let selected_dm = w.get_selected_dm_id().to_string();
                    let peer_channel_id = crate::bin_ui::features::chat::get_dm_channel_from_peer(&w, &peer_id_str);
                    let is_active = peer_channel_id.as_deref() == Some(selected_dm.as_str())
                        || selected_dm == peer_id_str;

                    if is_active {
                        let messages = w.get_messages();
                        for i in 0..messages.row_count() {
                            if let Some(msg) = messages.row_data(i) {
                                if msg.is_own && msg.delivery_status.as_str() == "sending" {
                                    let mut updated_msg = msg.clone();
                                    updated_msg.delivery_status = "sent".into();
                                    if let Some(model) = messages.as_any().downcast_ref::<slint::VecModel<crate::MessageItemData>>() {
                                        model.set_row_data(i, updated_msg);
                                    }
                                }
                            }
                        }
                    }
                }
            });

            ui_ctx.dm_bootstrap_service.handle_bootstrap_completed(peer_id);
        }
        InboxEvent::CallKeyOfferReceived {
            peer_id,
            call_id,
            ephemeral_public_key,
            call_type,
            channel_id,
            timestamp,
        } => {
            tracing::info!(
                peer_id = %peer_id,
                call_id = %call_id,
                "Incoming call offer - validating identity and showing modal"
            );

            // =========================================================================
            // Identity Validation
            // =========================================================================
            let mut identity_warning = String::new();
            let is_identity_verified;

            match ui_ctx.key_store.get_identity_by_uuid(&peer_id.to_string()) {
                Ok(Some(_trusted_key)) => {
                    is_identity_verified = true;
                }
                Ok(None) => {
                    identity_warning = "New identity key - verify carefully".to_string();
                    is_identity_verified = false;
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to fetch identity key for validation");
                    identity_warning = "Identity verification error".to_string();
                    is_identity_verified = false;
                }
            }

            let caller_name = ui_ctx.user_service.get_display_name(&peer_id);
            let is_video = call_type.contains("Video");
            let peer_id_str = peer_id.to_string();
            let weak = weak_window.clone();
            let ephemeral_key_b64 = base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &ephemeral_public_key
            );
            let call_start_timestamp_ns = timestamp.to_string();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    let call_data = crate::IncomingCallData {
                        call_id: call_id.into(),
                        caller_id: peer_id_str.into(),
                        caller_name: caller_name.into(),
                        caller_avatar: "".into(),
                        channel_name: channel_id.into(),
                        is_video_call: is_video,
                        is_anonymous_call: false,
                        is_identity_verified,
                        identity_warning: identity_warning.into(),
                        encryption_key: ephemeral_key_b64.into(),
                        call_start_timestamp_ns: call_start_timestamp_ns.into(),
                    };

                    w.set_incoming_call_data(call_data);
                    w.set_incoming_call_visible(true);
                    tracing::info!("Incoming call modal displayed (with identity check)");
                }
            });
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

            ui_ctx.event_bus.emit_domain(DomainEvent::CallKeyAnswerReceived {
                call_id: call_id.clone(),
                peer_id,
                ephemeral_public_key,
            });
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

            ui_ctx.event_bus.emit_domain(DomainEvent::GroupCallKeyReceived {
                call_id: call_id.clone(),
                sender_id,
                key_material,
                key_epoch,
                channel_id,
            });
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

            ui_ctx.event_bus.emit_domain(DomainEvent::CallHangupReceived {
                call_id: call_id.clone(),
                peer_id,
                reason: reason.clone(),
            });

            let weak = weak_window.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    let current_call_id = w.get_active_call_data().call_id.to_string();
                    if current_call_id == call_id || current_call_id.is_empty() {
                        cleanup_call_ui_state();
                        w.set_active_call_visible(false);
                        w.set_incoming_call_visible(false);
                        tracing::info!("Call ended by peer - UI cleaned up");
                    }
                }
            });
        }
        InboxEvent::SenderKeyNeeded { channel_id, sender_id } => {
            tracing::info!(
                channel_id = %channel_id,
                sender_id = %sender_id,
                "Sender key needed - sending request"
            );
            let msg_svc = ui_ctx.message_service.clone();
            let ch_id = channel_id;
            let s_id = sender_id;
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
        InboxEvent::RespondWithSkdm { channel_id, requester_id, skdm_bytes } => {
            tracing::info!(
                channel_id = %channel_id,
                requester_id = %requester_id,
                skdm_len = skdm_bytes.len(),
                "Responding with our SKDM"
            );
            let msg_svc = ui_ctx.message_service.clone();
            let ch_id = channel_id;
            let req_id = requester_id;
            tokio::spawn(async move {
                let skdm_result: Result<chatter_linux::proto::content_v1::SenderKeyDistribution, String> = if skdm_bytes.is_empty() {
                    msg_svc.create_sender_key_for_channel(&ch_id).await
                        .map_err(|e| e.to_string())
                } else {
                    use prost::Message as ProstMessage;
                    chatter_linux::proto::content_v1::SenderKeyDistribution::decode(skdm_bytes.as_slice())
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
        }
        // =========================================================================
        // Anonymous Group Events (NDSS'21 style server-oblivious groups)
        // =========================================================================
        InboxEvent::AnonGroupInvite {
            peer_id,
            group_id,
            group_name,
            inviter_mailbox_pk,
            timestamp,
            profile_key,
            encrypted_display_name,
            encrypted_avatar,
        } => {
            tracing::info!(
                peer_id = %peer_id,
                group_id = ?group_id,
                group_name = %group_name,
                timestamp = timestamp,
                has_profile = profile_key.is_some(),
                "Received anonymous group invite"
            );

            // Convert Vec<u8> to AnonGroupId and [u8; 32]
            let anon_group_id = match chatter_linux::domain::anon_group::AnonGroupId::try_from_slice(&group_id) {
                Some(id) => id,
                None => {
                    tracing::warn!(
                        group_id = ?group_id,
                        "Invalid group_id length for AnonGroupInvite (expected 16 bytes)"
                    );
                    return;
                }
            };

            let mailbox_pk: [u8; 32] = match inviter_mailbox_pk.as_slice().try_into() {
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

            use chatter_linux::crypto::profile_crypto::{decrypt_display_name, decrypt_avatar};

            let inviter_display_name: Option<String> = match (&profile_key_arr, &encrypted_display_name) {
                (Some(pk), Some(encrypted)) if !encrypted.is_empty() => {
                    decrypt_display_name(pk, encrypted).ok().filter(|n| !n.is_empty())
                }
                _ => None,
            };

            let inviter_avatar: Option<Vec<u8>> = match (&profile_key_arr, &encrypted_avatar) {
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
            ui_ctx.event_bus.emit_domain(DomainEvent::AnonGroupInviteReceived {
                group_id: anon_group_id,
                group_name: group_name.clone(),
                inviter_id: peer_id,
                inviter_mailbox_pk: mailbox_pk,
                profile_key: profile_key_arr,
                inviter_display_name,
                inviter_avatar,
            });
            // Add to sidebar as pending private group
            if let Some(w) = weak_window.upgrade() {
                crate::bin_ui::features::sidebar::add_private_group_to_sidebar(
                    &w,
                    &anon_group_id.to_string(),
                    &format!("\u{1F6E1} {}", group_name), // Shield emoji for private group
                    1, // Just the inviter known for now
                );
                
                // Increment message requests badge
                w.set_pending_message_requests(w.get_pending_message_requests() + 1);
            }
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
            tracing::info!(
                peer_id = %peer_id,
                group_id = ?group_id,
                timestamp = timestamp,
                has_profile = profile_key.is_some(),
                "Received anonymous group join acknowledgment"
            );

            // Convert Vec<u8> to AnonGroupId and [u8; 32]
            let anon_group_id = match chatter_linux::domain::anon_group::AnonGroupId::try_from_slice(&group_id) {
                Some(id) => id,
                None => {
                    tracing::warn!("Invalid group_id length for AnonGroupJoinAck");
                    return;
                }
            };

            let mailbox_pk: [u8; 32] = match joiner_mailbox_pk.as_slice().try_into() {
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

            use chatter_linux::crypto::profile_crypto::{decrypt_display_name, decrypt_avatar};

            let display_name: Option<String> = match (&profile_key_arr, &encrypted_display_name) {
                (Some(pk), Some(encrypted)) if !encrypted.is_empty() => {
                    decrypt_display_name(pk, encrypted).ok().filter(|n| !n.is_empty())
                }
                _ => None,
            };

            let avatar: Option<Vec<u8>> = match (&profile_key_arr, &encrypted_avatar) {
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

            let anon_service = ui_ctx.anon_group_service.clone();
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

            // Convert Vec<u8> to AnonGroupId
            let anon_group_id = match chatter_linux::domain::anon_group::AnonGroupId::try_from_slice(&group_id) {
                Some(id) => id,
                None => {
                    tracing::warn!("Invalid group_id length for AnonGroupMemberList");
                    return;
                }
            };

            // Update local member list via service (sync method)
            let anon_service = ui_ctx.anon_group_service.clone();
            let members_clone = members.clone();

            // handle_member_list is a sync method, so we run it in a blocking task
            let sender_id = peer_id;
            let _ = tokio::task::spawn_blocking(move || {
                if let Err(e) = anon_service.handle_member_list(anon_group_id, sender_id, members_clone) {
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

            // Convert Vec<u8> to AnonGroupId
            let anon_group_id = match chatter_linux::domain::anon_group::AnonGroupId::try_from_slice(&group_id) {
                Some(id) => id,
                None => {
                    tracing::warn!("Invalid group_id length for AnonGroupLeave");
                    return;
                }
            };

            // Remove member via service (sync method)
            let anon_service = ui_ctx.anon_group_service.clone();
            let peer_id_clone = peer_id;

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

            // Convert Vec<u8> to AnonGroupId
            let anon_group_id = match chatter_linux::domain::anon_group::AnonGroupId::try_from_slice(&group_id) {
                Some(id) => id,
                None => {
                    tracing::warn!("Invalid group_id length for AnonGroupKick");
                    return;
                }
            };

            // Convert Vec<u8> to UserId
            let kicked_user = match chatter_linux::domain::UserId::try_from_slice(&kicked_user_id) {
                Some(id) => id,
                None => {
                    tracing::warn!("Invalid kicked_user_id for AnonGroupKick");
                    return;
                }
            };

            // Verify signature and remove member via service (sync method)
            let anon_service = ui_ctx.anon_group_service.clone();
            let admin_signature_clone = admin_signature.clone();
            let peer_id_clone = peer_id;

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
            // Convert Vec<u8> to AnonGroupId
            let anon_group_id = match chatter_linux::domain::anon_group::AnonGroupId::try_from_slice(&group_id) {
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

            // Apply update via service (sync method)
            let anon_service = ui_ctx.anon_group_service.clone();
            let update_clone = update.clone();
            let peer_id_clone = peer_id;

            let _ = tokio::task::spawn_blocking(move || {
                if let Err(e) = anon_service.handle_update(anon_group_id, peer_id_clone, &update_clone) {
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
            // Convert Vec<u8> to AnonGroupId
            let anon_group_id = match chatter_linux::domain::anon_group::AnonGroupId::try_from_slice(&group_id) {
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

            // Route based on inner content type
            use chatter_linux::proto::content_v1::content;

            match &inner_content.content {
                // Handle typing indicator
                Some(content::Content::Typing(typing)) => {
                    let action = typing.action;
                    anon_group_handlers::handle_anon_group_typing(
                        weak_window,
                        ui_ctx,
                        anon_group_id,
                        peer_id,
                        action,
                    );
                }

                // Handle reaction
                Some(content::Content::Reaction(reaction)) => {
                    anon_group_handlers::handle_anon_group_reaction(
                        weak_window,
                        ui_ctx,
                        anon_group_id,
                        peer_id,
                        reaction.emoji.clone(),
                        reaction.remove,
                        reaction.target_sent_timestamp,
                    );
                }

                // Handle pin/unpin
                Some(content::Content::Pin(pin)) => {
                    anon_group_handlers::handle_anon_group_pin(
                        weak_window,
                        &ui_ctx.anon_group_service,
                        ui_ctx.pinned_state.clone(),
                        anon_group_id,
                        pin.target_sent_timestamp,
                        pin.unpin,
                    );
                }

                // Handle edit message (separate content type)
                Some(content::Content::Edit(edit)) => {
                    // EditMessage contains target_sent_timestamp and optional data_message with new body
                    let new_body = edit.data_message
                        .as_ref()
                        .map(|dm| dm.body.clone())
                        .unwrap_or_default();

                    // Persist edit to DB (must happen before UI update to avoid data loss on channel switch)
                    let service = ui_ctx.anon_group_service.clone();
                    let gid = anon_group_id;
                    let pid = peer_id;
                    let ts = edit.target_sent_timestamp;
                    let text = new_body.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        match service.update_message_text_by_sender(&gid, &pid, ts, &text) {
                            Ok(true) => {
                                tracing::info!(
                                    group_id = %gid,
                                    sender_id = %pid,
                                    target_timestamp = ts,
                                    "Persisted anon group edit to DB"
                                );
                            }
                            Ok(false) => {
                                tracing::debug!(
                                    group_id = %gid,
                                    sender_id = %pid,
                                    target_timestamp = ts,
                                    "Edit target not found or sender mismatch"
                                );
                            }
                            Err(e) => {
                                tracing::error!(
                                    error = %e,
                                    "Failed to persist anon group edit to DB"
                                );
                            }
                        }
                    });

                    // Update UI immediately
                    anon_group_handlers::handle_anon_group_edit(
                        weak_window,
                        anon_group_id,
                        edit.target_sent_timestamp,
                        new_body,
                    );
                }

                // Handle data message (text, delete, attachments)
                Some(content::Content::DataMessage(data_msg)) => {
                    // Check for delete first
                    if let Some(delete) = &data_msg.delete {
                        anon_group_handlers::handle_anon_group_delete(
                            weak_window,
                            anon_group_id,
                            delete.target_sent_timestamp,
                        );
                        return;
                    }

                    // Check for text or attachments
                    let text = data_msg.body.clone();
                    let has_text = !text.is_empty();
                    let has_attachments = !data_msg.attachments.is_empty();
                    let quote = data_msg.quote.clone();
                    // Extract reply display info from quote (resolve UUID to display name)
                    let has_quote = quote.is_some();
                    let quote_sender_name = quote.as_ref().map(|q| {
                        // Try to resolve author UUID to display name
                        if let Ok(author_id) = q.author_aci.parse::<chatter_linux::domain::UserId>() {
                            // Try group member name first, then user service
                            ui_ctx.anon_group_service
                                .get_member_display_name(&anon_group_id, &author_id)
                                .ok()
                                .flatten()
                                .filter(|n| !n.is_empty())
                                .unwrap_or_else(|| ui_ctx.user_service.get_display_name(&author_id))
                        } else {
                            q.author_aci.clone()
                        }
                    }).unwrap_or_default();
                    let quote_preview = quote.as_ref().map(|q| q.text.clone()).unwrap_or_default();

                    if !has_text && !has_attachments {
                        tracing::debug!(
                            group_id = %anon_group_id,
                            peer_id = %peer_id,
                            "Received empty message (no text, no attachments) in anon group"
                        );
                        return;
                    }

                    // Use sender's timestamp for consistent ordering across devices
                    let sent_at = inner_content.timestamp;

                    // Store message and update UI
                    let anon_service = ui_ctx.anon_group_service.clone();
                    let user_service = ui_ctx.user_service.clone();
                    let peer_id_clone = peer_id;
                    let weak = weak_window.clone();
                    let group_id_str = anon_group_id.to_string();
                    let attachments = data_msg.attachments.clone();
                    let sent_at_for_ui = sent_at; // Capture for UI closure

                    // Convert attachment pointers to display data BEFORE spawn_blocking
                    let mut attachment_display: Vec<_> = attachments
                        .iter()
                        .filter(|p| !p.cdn_key.is_empty())
                        .map(|p| crate::bin_ui::converters::attachment_pointer_to_display_data(p))
                        .collect();
                    // Pre-decode thumbnails before UI thread
                    for att in &mut attachment_display {
                        att.decode_thumbnail();
                    }

                    // Spawn auto-download for attachments
                    // Create attachment records FIRST to prevent race condition
                    if has_attachments {
                        let download_pointers: Vec<_> = attachments
                            .iter()
                            .filter(|p| !p.cdn_key.is_empty())
                            .cloned()
                            .collect();

                        if !download_pointers.is_empty() {
                            // Create attachment records BEFORE spawning download
                            ui_ctx.attachment_service.create_from_pointers(&download_pointers);

                            let ui_ctx_download = ui_ctx.clone();
                            let weak_download = weak_window.clone();
                            let group_id_download = anon_group_id.to_string();

                            tokio::spawn(async move {
                                dm::auto_download_attachments(
                                    download_pointers,
                                    ui_ctx_download,
                                    weak_download,
                                    group_id_download,
                                ).await;
                            });
                        }
                    }

                    let _ = tokio::task::spawn_blocking(move || {
                        // Create attachment records from pointers if any
                        let attachment_ids = if !attachments.is_empty() {
                            let ids = anon_service.create_attachments_from_pointers(&attachments);
                            tracing::info!(
                                group_id = %anon_group_id,
                                attachment_count = ids.len(),
                                "[ANON_GROUP] Created attachment records from pointers"
                            );
                            ids
                        } else {
                            Vec::new()
                        };

                        // Store message with attachments (and quote metadata if reply)
                        let store_result = if let Some(ref q) = quote {
                            let content_json = serde_json::json!({
                                "text": text,
                                "quote": {
                                    "author_id": q.author_aci,
                                    "author_name": quote_sender_name,
                                    "timestamp": q.id,
                                    "preview": q.text,
                                }
                            }).to_string();
                            anon_service.store_message_with_content_json(
                                &anon_group_id,
                                &peer_id_clone,
                                3,
                                &content_json,
                                &attachment_ids,
                                sent_at,
                            )
                        } else {
                            anon_service.store_message_with_attachments(
                                &anon_group_id,
                                &peer_id_clone,
                                &text,
                                &attachment_ids,
                                sent_at,
                            )
                        };
                        match store_result {
                            Ok(message_id) => {
                                tracing::debug!(
                                    group_id = %anon_group_id,
                                    peer_id = %peer_id_clone,
                                    message_id = %message_id,
                                    "Stored anonymous group message"
                                );

                                // Update UI if viewing this group
                                // Look up display name from group member list first, then fall back to user service
                                let sender_name = anon_service
                                    .get_member_display_name(&anon_group_id, &peer_id_clone)
                                    .ok()
                                    .flatten()
                                    .filter(|n| !n.is_empty())
                                    .unwrap_or_else(|| user_service.get_display_name(&peer_id_clone));
                                let text_clone = text.clone();
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(w) = weak.upgrade() {
                                        let selected_group = w.get_selected_private_group_id().to_string();
                                        if selected_group == group_id_str {
                                            // Add message to the current view
                                            let messages = w.get_messages();
                                            if let Some(model) = messages.as_any().downcast_ref::<slint::VecModel<crate::MessageItemData>>() {
                                                // Convert attachment display data to Slint types
                                                let slint_attachments: Vec<crate::MessageAttachmentData> = attachment_display
                                                    .iter()
                                                    .map(|a| a.to_slint())
                                                    .collect();

                                                // Use actual sent_at for correct ordering
                                                let new_msg = crate::MessageItemData {
                                                    message_id: message_id.to_string().into(),
                                                    sender_id: peer_id_clone.to_string().into(),
                                                    sender_name: sender_name.into(),
                                                    sender_initials: "".into(),
                                                    sender_avatar_url: "".into(),
                                                    sender_avatar_source: slint::Image::default(),
                                                    has_avatar: false,
                                                    content: text_clone.clone().into(),
                                                    timestamp: chrono::Utc::now().format("%H:%M").to_string().into(),
                                                    timestamp_ns: sent_at_for_ui.to_string().into(),
                                                    is_own: false,
                                                    is_edited: false,
                                                    is_deleted: false,
                                                    is_system: false,
                                                    is_ai: false,
                                                    has_attachments: !slint_attachments.is_empty(),
                                                    attachment_count: slint_attachments.len() as i32,
                                                    attachments: slint::ModelRc::new(slint::VecModel::from(slint_attachments)),
                                                    has_reactions: false,
                                                    reaction_summary: "".into(),
                                                    reactions: slint::ModelRc::default(),
                                                    reply_count: 0,
                                                    reply_count_text: "".into(),
                                                    is_thread_reply: false,
                                    is_reply: has_quote,
                                    reply_to_sender_name: quote_sender_name.clone().into(),
                                    reply_to_preview: quote_preview.clone().into(),
                                                    show_sender: true,
                                                    delivery_status: "delivered".into(),
                                                    is_pinned: false,
                                                    is_call: false,
                                                    call_type: "".into(),
                                                    call_duration: "".into(),
                                                };
                                                // Collect all messages, add new one, and sort by timestamp + sender_id
                                                let mut all_msgs: Vec<_> = (0..model.row_count())
                                                    .filter_map(|i| model.row_data(i))
                                                    .collect();
                                                all_msgs.push(new_msg);
                                                all_msgs.sort_by(|a, b| {
                                                    let ts_a = a.timestamp_ns.parse::<u64>().unwrap_or(0);
                                                    let ts_b = b.timestamp_ns.parse::<u64>().unwrap_or(0);
                                                    ts_a.cmp(&ts_b).then_with(|| a.sender_id.as_str().cmp(b.sender_id.as_str()))
                                                });
                                                w.set_messages(slint::ModelRc::new(slint::VecModel::from(all_msgs)));
                                                tracing::debug!("Added message to anon group view");
                                            }
                                        }

                                        // Update sidebar preview and move to top
                                        crate::bin_ui::features::sidebar::update_sidebar_preview(
                                            &w,
                                            chatter_linux::state::ActiveConversation::AnonGroup(anon_group_id),
                                            &text_clone,
                                            &chrono::Utc::now().format("%H:%M").to_string(),
                                            true,
                                        );
                                    }
                                });
                            }
                            Err(e) => {
                                tracing::error!(
                                    group_id = %anon_group_id,
                                    peer_id = %peer_id_clone,
                                    error = %e,
                                    "Failed to store message"
                                );
                            }
                        }
                    });
                }

                // Handle receipt (read/delivery)
                Some(content::Content::Receipt(receipt)) => {
                    tracing::debug!(
                        group_id = %anon_group_id,
                        peer_id = %peer_id,
                        receipt_type = receipt.r#type,
                        timestamps = ?receipt.timestamps,
                        "Received anon group receipt (not yet implemented in UI)"
                    );
                    // TODO: Implement receipt handling for anon groups if needed
                }

                // Handle anonymous group call offer
                Some(content::Content::AnonCallOffer(offer)) => {
                    tracing::info!(
                        group_id = %anon_group_id,
                        sender_id = %peer_id,
                        room_name = %offer.room_name,
                        "Received anonymous group call offer"
                    );
                    ui_ctx.event_bus.emit_domain(chatter_linux::inbox::DomainEvent::AnonGroupCallOfferReceived {
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

                // Handle anonymous group call answer
                Some(content::Content::AnonCallAnswer(answer)) => {
                    tracing::info!(
                        group_id = %anon_group_id,
                        sender_id = %peer_id,
                        accepted = answer.accepted,
                        "Received anonymous group call answer"
                    );
                    ui_ctx.event_bus.emit_domain(chatter_linux::inbox::DomainEvent::AnonGroupCallAnswerReceived {
                        group_id: anon_group_id,
                        sender_id: peer_id,
                        call_room_id: answer.call_room_id.clone(),
                        accepted: answer.accepted,
                        callee_display_name: answer.callee_display_name.clone(),
                        timestamp: answer.timestamp,
                    });
                }

                // Handle anonymous group call hangup
                Some(content::Content::AnonCallHangup(hangup)) => {
                    tracing::info!(
                        group_id = %anon_group_id,
                        sender_id = %peer_id,
                        "Received anonymous group call hangup"
                    );
                    ui_ctx.event_bus.emit_domain(chatter_linux::inbox::DomainEvent::AnonGroupCallHangupReceived {
                        group_id: anon_group_id,
                        sender_id: peer_id,
                        call_room_id: hangup.call_room_id.clone(),
                        reason: hangup.reason,
                        timestamp: hangup.timestamp,
                    });
                }

                _ => {
                    tracing::debug!(
                        group_id = %anon_group_id,
                        peer_id = %peer_id,
                        "Received unhandled anonymous group message content type"
                    );
                }
            }
        }
        // =========================================================================
        // Anonymous DM Events
        // =========================================================================
        InboxEvent::AnonDmRequest {
            peer_id,
            conversation_id,
            initiator_mailbox_pk,
            intro_message,
            profile_key,
            encrypted_display_name,
            encrypted_avatar,
            timestamp,
        } => {
            tracing::info!(
                peer_id = %peer_id,
                conversation_id_len = conversation_id.len(),
                has_intro = intro_message.is_some(),
                has_encrypted_name = encrypted_display_name.is_some(),
                has_encrypted_avatar = encrypted_avatar.is_some(),
                timestamp = timestamp,
                "Received anonymous DM request"
            );

            // Convert Vec<u8> to AnonDmId
            let anon_dm_id = match chatter_linux::domain::anon_dm::AnonDmId::try_from_slice(&conversation_id) {
                Some(id) => id,
                None => {
                    tracing::warn!("Invalid conversation_id length for AnonDmRequest");
                    return;
                }
            };

            let mailbox_pk: [u8; 32] = match initiator_mailbox_pk.as_slice().try_into() {
                Ok(pk) => pk,
                Err(_) => {
                    tracing::warn!(
                        mailbox_pk_len = initiator_mailbox_pk.len(),
                        "Invalid mailbox_pk length for AnonDmRequest (expected 32 bytes)"
                    );
                    return;
                }
            };

            // Delegate decryption + storage to service layer (crypto isolated from UI)
            let anon_dm_service = ui_ctx.anon_dm_service.clone();
            let event_bus = ui_ctx.event_bus.clone();
            let intro_msg = intro_message.clone();
            let weak = weak_window.clone();

            tokio::spawn(async move {
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
                        // Emit domain event for UI to show request modal
                        event_bus.emit_domain(DomainEvent::AnonDmRequestReceived {
                            conversation_id: anon_dm_id,
                            sender_id: peer_id,
                            sender_mailbox_pk: mailbox_pk,
                            intro_message,
                            profile_key: profile.profile_key,
                            sender_display_name: Some(profile.display_name.clone()),
                            sender_avatar: profile.avatar.clone(),
                        });

                        // Add to sidebar as pending DM (upgrade weak ref inside event loop)
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                crate::bin_ui::features::sidebar::dm::add_anon_dm_to_sidebar(
                                    &w,
                                    &anon_dm_id.to_string(),
                                    &peer_id.to_string(),
                                    &profile.display_name,
                                    "New message request",
                                    "Just now",
                                    true, // is_pending
                                    profile.avatar,
                                );
                                w.set_pending_message_requests(w.get_pending_message_requests() + 1);
                            }
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
        InboxEvent::AnonDmAccept {
            peer_id,
            conversation_id,
            responder_mailbox_pk,
            profile_key,
            responder_display_name: _, // Ignored - we decrypt ourselves
            encrypted_display_name,
            encrypted_avatar,
            timestamp,
        } => {
            tracing::info!(
                peer_id = %peer_id,
                conversation_id_len = conversation_id.len(),
                has_encrypted_display_name = encrypted_display_name.is_some(),
                has_encrypted_avatar = encrypted_avatar.as_ref().map(|v| !v.is_empty()).unwrap_or(false),
                timestamp = timestamp,
                "Received anonymous DM acceptance"
            );

            // Convert Vec<u8> to AnonDmId
            let anon_dm_id = match chatter_linux::domain::anon_dm::AnonDmId::try_from_slice(&conversation_id) {
                Some(id) => id,
                None => {
                    tracing::warn!("Invalid conversation_id length for AnonDmAccept");
                    return;
                }
            };

            let mailbox_pk: [u8; 32] = match responder_mailbox_pk.as_slice().try_into() {
                Ok(pk) => pk,
                Err(_) => {
                    tracing::warn!(
                        mailbox_pk_len = responder_mailbox_pk.len(),
                        "Invalid mailbox_pk length for AnonDmAccept (expected 32 bytes)"
                    );
                    return;
                }
            };

            // Delegate decryption + storage to service layer (crypto isolated from UI)
            let anon_dm_service = ui_ctx.anon_dm_service.clone();
            let event_bus = ui_ctx.event_bus.clone();
            let weak = weak_window.clone();

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
                        // Emit domain event for UI notification
                        event_bus.emit_domain(DomainEvent::AnonDmRequestAccepted {
                            conversation_id: anon_dm_id,
                            peer_id,
                            peer_mailbox_pk: mailbox_pk,
                            profile_key: profile.profile_key,
                            peer_display_name: profile.display_name.clone(),
                            peer_avatar: profile.avatar.clone(),
                        });

                        // Update sidebar entry from "pending" to active (upgrade weak ref inside event loop)
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                crate::bin_ui::features::sidebar::dm::add_anon_dm_to_sidebar(
                                    &w,
                                    &anon_dm_id.to_string(),
                                    &peer_id.to_string(),
                                    &profile.display_name,
                                    "Request accepted!",
                                    "Just now",
                                    false, // Not pending anymore
                                    profile.avatar,
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
        InboxEvent::AnonDmMessage {
            peer_id,
            conversation_id,
            inner_content,
        } => {
            // Convert Vec<u8> to AnonDmId
            let anon_dm_id = match chatter_linux::domain::anon_dm::AnonDmId::try_from_slice(&conversation_id) {
                Some(id) => id,
                None => {
                    tracing::warn!("Invalid conversation_id length for AnonDmMessage");
                    return;
                }
            };

            // Extract attachments - create records FIRST, then spawn download
            if let Some(chatter_linux::proto::content_v1::content::Content::DataMessage(data_msg)) = &inner_content.content {
                if !data_msg.attachments.is_empty() {
                    let download_pointers: Vec<_> = data_msg.attachments
                        .iter()
                        .filter(|p| !p.cdn_key.is_empty())
                        .cloned()
                        .collect();

                    if !download_pointers.is_empty() {
                        // Create attachment records BEFORE spawning download
                        // This prevents race condition where download checks for record that doesn't exist yet
                        ui_ctx.attachment_service.create_from_pointers(&download_pointers);

                        let ui_ctx_download = ui_ctx.clone();
                        let weak_download = weak_window.clone();
                        let conv_id_str = anon_dm_id.to_string();

                        tokio::spawn(async move {
                            dm::auto_download_attachments(
                                download_pointers,
                                ui_ctx_download,
                                weak_download,
                                conv_id_str,
                            ).await;
                        });
                    }
                }
            }

            // Delegate to service layer for business logic (proper separation of concerns)
            let anon_dm_service = ui_ctx.anon_dm_service.clone();
            let event_bus = ui_ctx.event_bus.clone();
            let inner_content_ref = (*inner_content).clone();

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
}

