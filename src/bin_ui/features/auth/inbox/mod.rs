//! Inbox event handling for UI updates.
//!
//! Processes incoming DM and channel events to update the UI in real-time.
//!
//! This module is organized into:
//! - `typing`: Typing timer management (Signal-style per-peer timers)
//! - `utils`: Shared utilities (reaction summary, timestamp formatting)
//! - `dm_handlers`: DM-specific event handlers
//! - `channel_handlers`: Channel-specific event handlers

mod channel_handlers;
mod dm_handlers;
pub mod typing;
pub mod utils;

use std::sync::Arc;

use slint::Model;

use chatter_linux::events::EventBus;
use chatter_linux::inbox::handlers::DomainEvent;
use chatter_linux::inbox::InboxEvent;
use chatter_linux::network::grpc::client_pool::GrpcClientPool;
use chatter_linux::services::DmBootstrapService;
use chatter_linux::services::message_service::MessageService;
use chatter_linux::storage::{AttachmentRepository, Database, MessageRepository, UserRepository};

use crate::bin_ui::features::calls::cleanup_call_ui_state;
use crate::MainWindow;

// Re-export public items
pub use utils::{format_timestamp, update_reaction_summary};

/// Handle inbox events and update UI accordingly.
#[allow(clippy::too_many_arguments)]
pub async fn handle_inbox_event(
    event: InboxEvent,
    weak_window: &slint::Weak<MainWindow>,
    user_repo: &UserRepository,
    dm_bootstrap_service: &Arc<DmBootstrapService>,
    attachment_repo: &Arc<AttachmentRepository>,
    grpc_pool: &Arc<GrpcClientPool>,
    _message_service: &Arc<MessageService>,
    database: &Arc<Database>,
    event_bus: &EventBus,
) {
    match event {
        InboxEvent::DmReceived {
            peer_id,
            text,
            timestamp,
            message_id,
            attachments,
        } => {
            let message_repo = Arc::new(MessageRepository::new(database.clone()));
            dm_handlers::handle_dm_received(weak_window, user_repo, attachment_repo, grpc_pool, &message_repo, peer_id, text, timestamp, message_id, attachments);
        }
        InboxEvent::DmDecryptionFailed { peer_id, error, convergence_failed } => {
            tracing::error!(
                "DM decryption failed from {}: {} (convergence_failed={})",
                peer_id, error, convergence_failed
            );

            // If all sessions failed (convergence failed), reset and prepare for fresh session
            if convergence_failed {
                dm_bootstrap_service.handle_convergence_failure(peer_id);
            }
        }
        InboxEvent::DmRetryRequested { peer_id, original_timestamp } => {
            tracing::info!(
                peer_id = %peer_id,
                original_timestamp,
                "DM retry requested - resetting session"
            );

            dm_bootstrap_service.handle_convergence_failure(peer_id);
        }
        InboxEvent::DmPollCompleted { total_messages } => {
            if total_messages > 0 {
                tracing::debug!("DM poll completed: {} messages", total_messages);
            }
        }
        InboxEvent::ChannelMessageReceived {
            channel_id,
            text,
            ..
        } => {
            let message_repo = Arc::new(MessageRepository::new(database.clone()));
            channel_handlers::handle_channel_message_received(
                weak_window,
                user_repo,
                attachment_repo,
                &message_repo,
                channel_id,
                text,
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
            dm_handlers::handle_dm_unread_count(weak_window, channel_id, peer_id, unread_count);
        }
        InboxEvent::DmTyping { peer_id, action } => {
            dm_handlers::handle_dm_typing(weak_window, peer_id, action);
        }
        InboxEvent::DmReceipt {
            peer_id,
            receipt_type,
            timestamps,
        } => {
            dm_handlers::handle_dm_receipt(weak_window, peer_id, receipt_type, timestamps);
        }
        InboxEvent::DmReaction {
            peer_id,
            emoji,
            remove,
            target_sent_timestamp,
            ..
        } => {
            dm_handlers::handle_dm_reaction(weak_window, peer_id, emoji, remove, target_sent_timestamp);
        }
        InboxEvent::DmPin {
            peer_id,
            target_sent_timestamp,
            unpin,
            ..
        } => {
            dm_handlers::handle_dm_pin(weak_window, peer_id, target_sent_timestamp, unpin);
        }
        InboxEvent::DmEdit {
            peer_id,
            target_sent_timestamp,
            new_body,
        } => {
            dm_handlers::handle_dm_edit(weak_window, peer_id, target_sent_timestamp, new_body);
        }
        InboxEvent::DmDeleteForMe {
            conversation_id,
            target_sent_timestamp,
            ..
        } => {
            dm_handlers::handle_dm_delete_for_me(weak_window, conversation_id, target_sent_timestamp);
        }
        InboxEvent::DmDeleteForEveryone {
            peer_id,
            target_sent_timestamp,
        } => {
            dm_handlers::handle_dm_delete_for_everyone(weak_window, peer_id, target_sent_timestamp);
        }
        InboxEvent::ChannelTyping {
            channel_id,
            sender_id,
            action,
        } => {
            channel_handlers::handle_channel_typing(weak_window, user_repo, channel_id, sender_id, action);
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
            channel_handlers::handle_channel_pin(weak_window, channel_id, target_sent_timestamp, unpin);
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
            ..
        } => {
            tracing::debug!(
                channel_id = %channel_id,
                member_id = %member_id,
                "Group mailbox announcement received"
            );
        }
        InboxEvent::FriendRequestsUpdated { received, sent } => {
            tracing::debug!(
                received_count = received.len(),
                sent_count = sent.len(),
                "Friend requests updated"
            );
            // Update UI with friend requests
            let weak = weak_window.clone();
            let received_ui: Vec<crate::FriendRequestData> = received
                .into_iter()
                .map(|r| crate::FriendRequestData {
                    user_id: r.user_id.into(),
                    display_name: r.display_name.into(),
                    username: r.username.into(),
                    avatar_url: r.avatar_url.into(),
                    sent_at: "".into(),
                })
                .collect();
            let sent_ui: Vec<crate::FriendRequestData> = sent
                .into_iter()
                .map(|r| crate::FriendRequestData {
                    user_id: r.user_id.into(),
                    display_name: r.display_name.into(),
                    username: r.username.into(),
                    avatar_url: r.avatar_url.into(),
                    sent_at: "".into(),
                })
                .collect();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
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

            // Fetch the channel from server and add to sidebar
            dm_handlers::handle_friend_accepted(
                weak_window,
                user_repo,
                dm_bootstrap_service,
                grpc_pool,
                friend_user_id,
                channel_id,
            )
            .await;
        }
        InboxEvent::DmBootstrapCompleted { peer_id } => {
            // Bootstrap completed - send any pending messages that were queued
            tracing::info!(
                peer_id = %peer_id,
                "BOOTSTRAP[4/4] DmBootstrapCompleted event received - will flush pending messages"
            );

            // Reset composer placeholder and update message status
            let peer_id_str = peer_id.to_string();
            let weak = weak_window.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    // Reset placeholder to normal
                    w.set_composer_placeholder("Type a message...".into());

                    // Update any "sending" messages to "sent" for this peer
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

            dm_bootstrap_service.handle_bootstrap_completed(peer_id);
        }
        InboxEvent::IncomingCall {
            peer_id,
            call_id,
            key_material,
            call_type,
            channel_id,
            timestamp,
        } => {
            tracing::info!(
                peer_id = %peer_id,
                call_id = %call_id,
                call_type = %call_type,
                channel_id = %channel_id,
                key_len = key_material.len(),
                timestamp = timestamp,
                "Incoming call notification - showing modal"
            );

            // Get caller display name from user repo
            let caller_name = user_repo
                .get_profile(&peer_id)
                .ok()
                .flatten()
                .map(|p| p.display_name.clone())
                .filter(|name| !name.is_empty())
                .or_else(|| {
                    user_repo
                        .get(&peer_id)
                        .ok()
                        .flatten()
                        .map(|u| u.username.clone())
                })
                .unwrap_or_else(|| peer_id.to_string());

            let is_video = call_type.contains("Video");
            let peer_id_str = peer_id.to_string();
            let weak = weak_window.clone();

            // Encode encryption key as base64 for UI transport
            let encryption_key_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &key_material);

            // Pass call start timestamp as string (nanoseconds) for synchronized duration
            let call_start_timestamp_ns = timestamp.to_string();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    // Create incoming call data struct with encryption key
                    let call_data = crate::IncomingCallData {
                        call_id: call_id.into(),
                        caller_id: peer_id_str.into(),
                        caller_name: caller_name.into(),
                        caller_avatar: "".into(),
                        channel_name: channel_id.into(), // Use channel_id as channel_name for now
                        is_video_call: is_video,
                        encryption_key: encryption_key_b64.into(),
                        call_start_timestamp_ns: call_start_timestamp_ns.into(),
                    };

                    // Set incoming call data
                    w.set_incoming_call_data(call_data);

                    // Show the incoming call modal
                    w.set_incoming_call_visible(true);

                    tracing::info!("Incoming call modal displayed");
                }
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

            // Emit domain event for calls.rs to handle (runs on UI thread with CallService access)
            event_bus.emit_domain(DomainEvent::CallHangupReceived {
                call_id: call_id.clone(),
                peer_id,
                reason: reason.clone(),
            });

            let weak = weak_window.clone();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    // Close the active call view if this call is active
                    let current_call_id = w.get_active_call_data().call_id.to_string();
                    if current_call_id == call_id || current_call_id.is_empty() {
                        // Stop call timer and clean up UI state
                        cleanup_call_ui_state();

                        w.set_active_call_visible(false);
                        w.set_incoming_call_visible(false);
                        tracing::info!("Call ended by peer - UI cleaned up");
                    }
                }
            });
        }
    }
}
