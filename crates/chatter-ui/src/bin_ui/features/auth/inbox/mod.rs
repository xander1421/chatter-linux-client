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
pub mod typing;
pub(crate) mod unified_actions;
pub mod utils;

pub use anon_group_events::spawn_anon_group_event_handler;

// Legacy anon DM action handlers — now unified in unified_actions.rs
// Kept for anon_dm_actions.rs internal use (message_deleted/edited still used by some paths)

use std::sync::Arc;

use slint::Model;

use chatter_core::client::ChatterClient;
use chatter_core::inbox::InboxEvent;

use crate::bin_ui::features::calls::cleanup_call_ui_state;
use crate::MainWindow;

// Re-export public items
pub use utils::update_reaction_summary;

/// Handle inbox events and update UI accordingly.
///
/// Protocol-only logic (delivery receipts, convergence, sender keys, etc.)
/// is handled by `InboxEventProcessor` in chatter-core. This function
/// only handles events that require UI updates (Slint model changes).
pub async fn handle_inbox_event(
    event: InboxEvent,
    weak_window: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
) {
    match event {
        // =================================================================
        // DM Events (UI-only parts — protocol handled by InboxEventProcessor)
        // =================================================================
        InboxEvent::DmReceived {
            peer_id,
            channel_id,
            text,
            timestamp,
            message_id,
            attachments,
        } => {
            // UI display only — delivery receipt is sent by InboxEventProcessor
            dm::handle_dm_received(weak_window, ui_ctx, peer_id, channel_id, text.clone(), timestamp, message_id, attachments);
        }

        // Protocol-only: handled entirely by InboxEventProcessor
        InboxEvent::DmDecryptionFailed { .. }
        | InboxEvent::DmRetryRequested { .. }
        | InboxEvent::DmMailboxExpired { .. }
        | InboxEvent::DmPollCompleted { .. }
        | InboxEvent::MessageReceived { .. }
        | InboxEvent::MessageProcessed { .. }
        | InboxEvent::MessageFailed { .. }
        | InboxEvent::PollCompleted { .. }
        | InboxEvent::PollFailed { .. }
        | InboxEvent::AcknowledgeFailed { .. }
        | InboxEvent::GroupMailboxPollCompleted { .. }
        | InboxEvent::GroupMailboxAnnouncement { .. }
        | InboxEvent::CallKeyAnswerReceived { .. }
        | InboxEvent::GroupCallKeyReceived { .. }
        | InboxEvent::SenderKeyNeeded { .. }
        | InboxEvent::RespondWithSkdm { .. }
        | InboxEvent::AnonGroupJoinAck { .. }
        | InboxEvent::AnonGroupMemberList { .. }
        | InboxEvent::AnonGroupLeave { .. }
        | InboxEvent::AnonGroupKick { .. }
        | InboxEvent::AnonGroupUpdate { .. }
        | InboxEvent::AnonDmMessage { .. } => {
            // Fully handled by InboxEventProcessor — nothing to do here
        }

        InboxEvent::ChannelMessageReceived {
            channel_id,
            text,
            attachments,
            ..
        } => {
            // UI display only — profile prefetch + delivery receipt handled by InboxEventProcessor
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
        }

        InboxEvent::DmUnreadCountUpdated {
            channel_id,
            peer_id,
            unread_count,
        } => {
            dm::handle_dm_unread_count(weak_window, channel_id, peer_id, unread_count);
        }
        InboxEvent::DmTyping { peer_id, action } => {
            unified_actions::handle_typing(weak_window, &ui_ctx.chat_state() , unified_actions::EventSource::Peer(peer_id), action == 1);
        }
        InboxEvent::DmReceipt {
            peer_id,
            receipt_type,
            timestamps,
        } => {
            unified_actions::handle_receipt(weak_window, &ui_ctx.chat_state() , unified_actions::EventSource::Peer(peer_id), receipt_type, timestamps);
        }
        InboxEvent::DmReaction {
            peer_id,
            emoji,
            remove,
            target_sent_timestamp,
            ..
        } => {
            unified_actions::handle_reaction(weak_window, &ui_ctx.chat_state() , unified_actions::EventSource::Peer(peer_id), emoji, remove, target_sent_timestamp);
        }
        InboxEvent::DmPin {
            peer_id,
            target_sent_timestamp,
            unpin,
            ..
        } => {
            unified_actions::handle_pin(weak_window, &ui_ctx.chat_state() , &ui_ctx.pinned_state(), unified_actions::EventSource::Peer(peer_id), target_sent_timestamp, !unpin);
        }
        InboxEvent::DmEdit {
            peer_id,
            target_sent_timestamp,
            new_body,
        } => {
            unified_actions::handle_edit(weak_window, &ui_ctx.chat_state() , unified_actions::EventSource::Peer(peer_id), target_sent_timestamp, new_body);
        }
        InboxEvent::DmDeleteForMe {
            conversation_id,
            target_sent_timestamp,
            ..
        } => {
            if let Ok(channel_id) = conversation_id.parse() {
                unified_actions::handle_delete(weak_window, &ui_ctx.chat_state() , unified_actions::EventSource::Channel(channel_id), target_sent_timestamp);
            }
        }
        InboxEvent::DmDeleteForEveryone {
            peer_id,
            target_sent_timestamp,
        } => {
            unified_actions::handle_delete(weak_window, &ui_ctx.chat_state() , unified_actions::EventSource::Peer(peer_id), target_sent_timestamp);
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
            channel_handlers::handle_channel_pin(weak_window, ui_ctx.pinned_state().clone(), channel_id, target_sent_timestamp, unpin);
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
            // Protocol part (handle_bootstrap_completed) is handled by InboxEventProcessor
            // UI part: update composer placeholder and message delivery status
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

            match ui_ctx.signal().get_peer_identity(&peer_id.to_string()) {
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

            let caller_name = ui_ctx.users().get_display_name(&peer_id);
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
        InboxEvent::CallHangup {
            call_id,
            ..
        } => {
            // Protocol part (emit_domain CallHangupReceived) is handled by InboxEventProcessor
            // UI cleanup only
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
        // =========================================================================
        // Anonymous Group Invite (UI part — protocol handled by InboxEventProcessor)
        // =========================================================================
        InboxEvent::AnonGroupInvite {
            group_id,
            group_name,
            ..
        } => {
            // Protocol part (decrypt profile, emit domain event) handled by InboxEventProcessor
            // UI part: add to sidebar
            let anon_group_id = match chatter_core::domain::anon_group::AnonGroupId::try_from_slice(&group_id) {
                Some(id) => id,
                None => return,
            };

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
        // =========================================================================
        // Anonymous Group Message (UI part — protocol handled by InboxEventProcessor)
        // =========================================================================
        InboxEvent::AnonGroupMessage {
            peer_id,
            group_id,
            inner_content,
        } => {
            // Convert Vec<u8> to AnonGroupId
            let anon_group_id = match chatter_core::domain::anon_group::AnonGroupId::try_from_slice(&group_id) {
                Some(id) => id,
                None => {
                    return;
                }
            };

            // Route based on inner content type (UI updates only)
            use chatter_core::proto::content_v1::content;

            match &inner_content.content {
                // Handle typing indicator
                Some(content::Content::Typing(typing)) => {
                    let channel_id: chatter_core::domain::ChannelId = anon_group_id.into();
                    unified_actions::handle_typing(
                        weak_window, &ui_ctx.chat_state() ,
                        unified_actions::EventSource::Channel(channel_id),
                        typing.action == 1,
                    );
                }

                // Handle reaction
                Some(content::Content::Reaction(reaction)) => {
                    // Persist to DB
                    let service = ui_ctx.anon_groups().clone();
                    let emoji_db = reaction.emoji.clone();
                    let gid = anon_group_id;
                    let ts = reaction.target_sent_timestamp;
                    let remove = reaction.remove;
                    let pid = peer_id;
                    let _ = std::thread::spawn(move || {
                        if remove {
                            let _ = service.remove_reaction(&gid, ts, &pid, &emoji_db);
                        } else {
                            let _ = service.add_reaction(&gid, ts, &pid, &emoji_db);
                        }
                    });

                    // Update UI
                    let channel_id: chatter_core::domain::ChannelId = anon_group_id.into();
                    unified_actions::handle_reaction(
                        weak_window, &ui_ctx.chat_state() ,
                        unified_actions::EventSource::Channel(channel_id),
                        reaction.emoji.clone(), reaction.remove, reaction.target_sent_timestamp,
                    );
                }

                // Handle pin/unpin
                Some(content::Content::Pin(pin)) => {
                    // DB persistence handled by InboxEventProcessor → EventHandlers

                    // Update UI
                    let channel_id: chatter_core::domain::ChannelId = anon_group_id.into();
                    unified_actions::handle_pin(
                        weak_window, &ui_ctx.chat_state() , &ui_ctx.pinned_state(),
                        unified_actions::EventSource::Channel(channel_id),
                        pin.target_sent_timestamp, !pin.unpin,
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
                    let service = ui_ctx.anon_groups().clone();
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
                    let channel_id: chatter_core::domain::ChannelId = anon_group_id.into();
                    unified_actions::handle_edit(
                        weak_window, &ui_ctx.chat_state() ,
                        unified_actions::EventSource::Channel(channel_id),
                        edit.target_sent_timestamp, new_body,
                    );
                }

                // Handle data message (text, delete, attachments)
                Some(content::Content::DataMessage(data_msg)) => {
                    // Check for delete first
                    if let Some(delete) = &data_msg.delete {
                        let channel_id: chatter_core::domain::ChannelId = anon_group_id.into();
                        unified_actions::handle_delete(
                            weak_window, &ui_ctx.chat_state() ,
                            unified_actions::EventSource::Channel(channel_id),
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
                        if let Ok(author_id) = q.author_aci.parse::<chatter_core::domain::UserId>() {
                            // Try group member name first, then user service
                            ui_ctx.anon_groups()
                                .get_member_display_name(&anon_group_id, &author_id)
                                .ok()
                                .flatten()
                                .filter(|n| !n.is_empty())
                                .unwrap_or_else(|| ui_ctx.users().get_display_name(&author_id))
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
                    let anon_service = ui_ctx.anon_groups().clone();
                    let user_service = ui_ctx.users().clone();
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
                            ui_ctx.attachments().create_from_pointers(&download_pointers);

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
                                            chatter_core::state::ActiveConversation::AnonGroup(anon_group_id),
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

                // Receipt, call offer/answer/hangup are handled by InboxEventProcessor
                // Other content types not handled by UI
                _ => {}
            }
        }

        // =========================================================================
        // Anonymous DM Request (UI part — sidebar addition)
        // Protocol part (decrypt profile, emit domain event) handled by InboxEventProcessor
        // =========================================================================
        InboxEvent::AnonDmRequest {
            peer_id,
            conversation_id,
            ..
        } => {
            // InboxEventProcessor handles decryption + domain event emission
            // UI: listen for DomainEvent::AnonDmRequestReceived via domain_handler
            // The sidebar update is handled by domain_handler when it receives the domain event
            tracing::debug!(
                peer_id = %peer_id,
                conversation_id_len = conversation_id.len(),
                "AnonDmRequest UI stub - protocol handled by InboxEventProcessor"
            );
        }

        // =========================================================================
        // Anonymous DM Accept (UI part — sidebar update)
        // Protocol part (decrypt profile, emit domain event) handled by InboxEventProcessor
        // =========================================================================
        InboxEvent::AnonDmAccept {
            peer_id,
            conversation_id,
            ..
        } => {
            // InboxEventProcessor handles decryption + domain event emission
            // UI: listen for DomainEvent::AnonDmRequestAccepted via domain_handler
            tracing::debug!(
                peer_id = %peer_id,
                conversation_id_len = conversation_id.len(),
                "AnonDmAccept UI stub - protocol handled by InboxEventProcessor"
            );
        }
    }
}

