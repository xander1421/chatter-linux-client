//! Domain event handler for UI updates.
//!
//! This module handles `DomainEvent` for pure UI updates. Protocol concerns
//! (receipts, convergence, sender keys) are handled by `InboxEventProcessor`.
//!
//! # Architecture
//!
//! ```text
//! InboxEvent → InboxEventProcessor → DomainEvent → domain_handler (this module)
//!                 (protocol)                           (UI only)
//! ```

use std::sync::Arc;

use slint::{Model, Weak};

use chatter_linux::inbox::handlers::DomainEvent;
use chatter_linux::app::UIContext;
use crate::MainWindow;

/// Handle a domain event for UI updates only.
///
/// This function should contain NO protocol logic - only UI updates.
/// Protocol concerns are handled by `InboxEventProcessor` before events reach here.
pub async fn handle_domain_event(
    event: DomainEvent,
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<UIContext>,
) {
    match event {
        // =========================================================================
        // Unread Count Updates
        // =========================================================================
        DomainEvent::UnreadCountUpdated {
            channel_id,
            peer_id,
            unread_count,
        } => {
            let weak = window.clone();
            let channel_id_str = channel_id.to_string();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    // Update sidebar unread badge
                    update_unread_count(&w, &channel_id_str, unread_count);
                }
            });
            tracing::debug!(
                channel_id = %channel_id,
                ?peer_id,
                unread_count,
                "Updated unread count in UI"
            );
        }

        // =========================================================================
        // DM Bootstrap Completed
        // =========================================================================
        DomainEvent::DmBootstrapCompleted { peer_id } => {
            let weak = window.clone();
            let ui_ctx = ui_ctx.clone();
            let peer_id_str = peer_id.to_string();

            // Flush any pending messages for this peer
            let dm_bootstrap = ui_ctx.dm_bootstrap_service.clone();
            let pending = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    dm_bootstrap.take_pending_messages(&peer_id).await
                })
            });
            
            if !pending.is_empty() {
                tracing::info!(
                    peer_id = %peer_id,
                    pending_count = pending.len(),
                    "Flushing pending messages after bootstrap"
                );

                let mailbox = ui_ctx.mailbox_manager.clone();
                let msg_svc = ui_ctx.message_service.clone();
                tokio::spawn(async move {
                    for text in pending {
                        let now = chrono::Utc::now().timestamp_millis() as u64;
                        let content = chatter_linux::services::build_text_content(&text, now);
                        if let Err(e) = msg_svc.send_dm(&peer_id, content, &*mailbox).await {
                            tracing::warn!(error = %e, "Failed to send pending message");
                        }
                    }
                });
            }

            // Update UI to enable composer
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(_w) = weak.upgrade() {
                    // If we're in a DM with this peer, enable the composer
                    // For now just log - we need to implement proper UI updates
                    tracing::debug!(peer_id = %peer_id_str, "DM bootstrap complete - composer should be enabled");
                    // This should trigger a UI update to enable the composer for this DM
                }
            });
        }

        // =========================================================================
        // Friend Request Updates
        // =========================================================================
        DomainEvent::FriendRequestsUpdated { received, sent } => {
            let weak = window.clone();
            let received_count = received.len();
            let sent_count = sent.len();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    // Update friend request lists
                    let received_model: Vec<crate::FriendRequestData> = received
                        .into_iter()
                        .map(|r| crate::FriendRequestData {
                            user_id: r.user_id.into(),
                            display_name: r.display_name.into(),
                            username: r.username.into(),
                            avatar_url: r.avatar_url.into(),
                            avatar_source: slint::Image::default(),
                            has_avatar: false,
                            sent_at: "".into(),
                        })
                        .collect();

                    let sent_model: Vec<crate::FriendRequestData> = sent
                        .into_iter()
                        .map(|r| crate::FriendRequestData {
                            user_id: r.user_id.into(),
                            display_name: r.display_name.into(),
                            username: r.username.into(),
                            avatar_url: r.avatar_url.into(),
                            avatar_source: slint::Image::default(),
                            has_avatar: false,
                            sent_at: "".into(),
                        })
                        .collect();

                    // Update friend requests in UI
                    super::super::initial_data::update_friend_requests(&w, received_model, sent_model);
                }
            });

            tracing::debug!(
                received = received_count,
                sent = sent_count,
                "Updated friend requests in UI"
            );
        }

        // =========================================================================
        // Friend Accepted
        // =========================================================================
        DomainEvent::FriendAccepted { friend_id, channel_id } => {
            let weak = window.clone();
            let ui_ctx = ui_ctx.clone();

            // Add DM channel to sidebar, then re-merge anon DMs so they aren't lost
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    // Refresh regular DM list (replaces entire list)
                    super::super::initial_data::refresh_dm_list(&w, &ui_ctx);
                    // Re-add anon DMs that were dropped by the full replace
                    super::super::initial_data::refresh_anon_dms(&w, &ui_ctx);
                    tracing::info!(
                        friend_id = %friend_id,
                        channel_id = %channel_id,
                        "Friend accepted - refreshed DM sidebar with anon DMs preserved"
                    );
                }
            });
        }

        // =========================================================================
        // Call Key Offer (Incoming Call)
        // =========================================================================
        DomainEvent::CallKeyOfferReceived {
            call_id,
            peer_id,
            ephemeral_public_key,
            call_type,
            channel_id: _,
            is_identity_verified,
            identity_warning,
            caller_name,
            timestamp: _,
        } => {
            let weak = window.clone();
            let ui_ctx = ui_ctx.clone();

            // Store ephemeral key for later use
            ui_ctx.chat_state.set_incoming_call_ephemeral_pk(Some(ephemeral_public_key.to_vec()));

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    // Show incoming call modal
                    super::super::initial_data::show_incoming_call(
                        &w,
                        &call_id,
                        &peer_id.to_string(),
                        &caller_name,
                        &call_type,
                        is_identity_verified,
                        &identity_warning,
                        false, // not anonymous
                    );

                    tracing::info!(
                        peer_id = %peer_id,
                        call_type = %call_type,
                        verified = is_identity_verified,
                        "Showing incoming call modal"
                    );
                }
            });
        }

        // =========================================================================
        // Call Key Answer
        // =========================================================================
        DomainEvent::CallKeyAnswerReceived {
            call_id,
            peer_id,
            ephemeral_public_key: _,
        } => {
            let weak = window.clone();
            let ui_ctx = ui_ctx.clone();

            // Complete the call key exchange
            if let Some(_our_eph_sk) = ui_ctx.chat_state.take_outgoing_call_ephemeral_sk() {
                // Derive call key from ECDH
                // TODO: Implement x25519_ecdh or use call_key_provider
                // let shared = chatter_linux::crypto::x25519_ecdh(&our_eph_sk, &ephemeral_public_key);
                let shared = [0u8; 32]; // Placeholder
                ui_ctx.chat_state.set_current_call_key(Some(shared.to_vec()));

                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(_w) = weak.upgrade() {
                        // Update call state - callee answered
                        // w.set_call_state("connecting".into());
                        tracing::info!(
                            call_id = %call_id,
                            peer_id = %peer_id,
                            "Call key exchange complete - connecting"
                        );
                    }
                });
            }
        }

        // =========================================================================
        // Call Hangup
        // =========================================================================
        DomainEvent::CallHangupReceived { call_id, peer_id, reason } => {
            let weak = window.clone();
            let ui_ctx = ui_ctx.clone();

            // Clear call state
            ui_ctx.chat_state.clear_call_state();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    // Hide call modals
                    super::super::initial_data::hide_incoming_call(&w);
                    tracing::info!(
                        call_id = %call_id,
                        peer_id = %peer_id,
                        reason = %reason,
                        "Call ended"
                    );
                }
            });
        }

        // =========================================================================
        // Group Call Key
        // =========================================================================
        DomainEvent::GroupCallKeyReceived {
            call_id,
            sender_id,
            key_material,
            key_epoch,
            channel_id,
        } => {
            let ui_ctx = ui_ctx.clone();

            // Update group call key
            ui_ctx.chat_state.set_group_call_key(Some(key_material.to_vec()));

            tracing::debug!(
                call_id = %call_id,
                sender_id = %sender_id,
                channel_id = %channel_id,
                key_epoch,
                "Updated group call key"
            );
        }

        // =========================================================================
        // Typing Indicators
        // =========================================================================
        DomainEvent::TypingStarted { user_id, channel_id } => {
            // These events are for tracking who's typing on the server side
            // The UI handles typing display through the existing typing.rs functions
            // which are called from the message handlers
            tracing::debug!(
                user_id = %user_id,
                channel_id = %channel_id,
                "Typing started event received"
            );
        }

        DomainEvent::TypingStopped { user_id, channel_id } => {
            // These events are for tracking who's typing on the server side
            // The UI handles typing display through the existing typing.rs functions
            // which are called from the message handlers
            tracing::debug!(
                user_id = %user_id,
                channel_id = %channel_id,
                "Typing stopped event received"
            );
        }

        // =========================================================================
        // Message Events (handled by EventHandlers state store)
        // =========================================================================
        DomainEvent::MessageReceived { .. }
        | DomainEvent::MessageEdited { .. }
        | DomainEvent::MessageDeleted { .. }
        | DomainEvent::MessagesRead { .. }
        | DomainEvent::MessagesDelivered { .. }
        | DomainEvent::ViewedReceipt { .. }
        | DomainEvent::ReactionAdded { .. }
        | DomainEvent::ReactionRemoved { .. }
        | DomainEvent::DeleteForMeSync { .. } => {
            // These are handled by EventHandlers in events/handlers.rs
            // which updates state stores directly
        }

        DomainEvent::MessagePinned { channel_id, target_sent_timestamp, .. } => {
            // Pin persistence is handled by pin_persistence.rs task.
            // Here we update the pinned bar UI.
            handle_pin_ui_update(window, ui_ctx, channel_id, target_sent_timestamp, false);
        }

        DomainEvent::MessageUnpinned { channel_id, target_sent_timestamp, .. } => {
            handle_pin_ui_update(window, ui_ctx, channel_id, target_sent_timestamp, true);
        }

        // =========================================================================
        // Protocol Events (handled by InboxEventProcessor, no UI action needed)
        // =========================================================================
        DomainEvent::KeyExchangeReceived { .. }
        | DomainEvent::SenderKeyReceived { .. }
        | DomainEvent::GroupMailboxReceived { .. }
        | DomainEvent::AttachmentReady { .. } => {
            // Protocol events - no direct UI action
        }

        // =========================================================================
        // Anonymous Group Events
        // =========================================================================
        DomainEvent::AnonGroupInviteReceived { group_id: _, group_name: _, inviter_id: _, .. } => {
            let weak = window.clone();
            let ui_ctx = ui_ctx.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    // Refresh anonymous groups to show the invite
                    super::super::initial_data::refresh_anon_groups(&w, &ui_ctx);
                }
            });
        }

        DomainEvent::AnonGroupJoined { group_id } => {
            let weak = window.clone();
            let ui_ctx = ui_ctx.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    // Refresh sidebar to show new group
                    super::super::initial_data::refresh_anon_groups(&w, &ui_ctx);
                    tracing::info!(group_id = %group_id, "Joined anon group - refreshed sidebar");
                }
            });
        }

        DomainEvent::AnonGroupMemberJoined { group_id, user_id, display_name, .. } => {
            tracing::info!(
                group_id = %group_id,
                user_id = %user_id,
                display_name = ?display_name,
                "Member joined anon group"
            );
            // Could show a system message or notification
        }

        DomainEvent::AnonGroupMemberLeft { group_id, user_id } => {
            tracing::info!(group_id = %group_id, user_id = %user_id, "Member left anon group");
        }

        DomainEvent::AnonGroupMemberKicked { group_id, user_id, kicked_by } => {
            tracing::info!(
                group_id = %group_id,
                user_id = %user_id,
                kicked_by = %kicked_by,
                "Member kicked from anon group"
            );
        }

        DomainEvent::AnonGroupMessageReceived { group_id, sender_id, message_id, .. } => {
            // Resolve sender display name from group member list
            let sender_name = super::anon_group_handlers::get_anon_member_name(ui_ctx, &group_id, &sender_id);
            let group_id_str = group_id.to_string();
            let weak = window.clone();
            let ui_ctx = ui_ctx.clone();

            // Fetch the stored message and push it to the UI
            let msg = match ui_ctx.anon_group_service.get_message(&message_id) {
                Ok(Some(m)) => m,
                _ => {
                    tracing::warn!(message_id = %message_id, "AnonGroupMessageReceived: message not found in store");
                    return;
                }
            };

            // Get sender avatar from member profile
            let sender_avatar = ui_ctx.anon_group_service
                .get_member(&group_id, &sender_id)
                .ok()
                .flatten()
                .and_then(|m| m.avatar.clone());

            // Resolve attachments
            let attachment_ids = msg.attachment_ids();
            let resolved_attachments: Vec<chatter_linux::domain::Attachment> = attachment_ids
                .iter()
                .filter_map(|att_id| ui_ctx.attachment_service.get(att_id).ok().flatten())
                .collect();

            let mut display_data = crate::bin_ui::converters::anon_group_message_to_display_data(
                &msg,
                &sender_name,
                sender_avatar,
                &resolved_attachments,
            );
            display_data.decode_attachment_thumbnails();

            let current_user_id = ui_ctx.session_service.current_user_id()
                .map(|uid| uid.to_string())
                .unwrap_or_default();

            let _ = slint::invoke_from_event_loop(move || {
                let Some(window) = weak.upgrade() else { return };
                let selected_group = window.get_selected_private_group_id().to_string();
                if selected_group != group_id_str {
                    return;
                }

                let messages = window.get_messages();
                if let Some(model) = messages.as_any().downcast_ref::<slint::VecModel<crate::MessageItemData>>() {
                    let msg_item = display_data.to_slint_with_user(&current_user_id);
                    model.push(msg_item);
                }
            });
        }

        DomainEvent::AnonGroupBootstrapComplete { group_id } => {
            tracing::debug!(group_id = %group_id, "Anon group bootstrap complete");
        }

        DomainEvent::AnonGroupMemberListReceived { group_id, members } => {
            tracing::debug!(
                group_id = %group_id,
                member_count = members.len(),
                "Received anon group member list"
            );
            
            // Refresh the message list if we're currently viewing this group
            // This ensures member names are updated in existing messages
            let weak = window.clone();
            let ui_ctx = ui_ctx.clone();
            let group_id_str = group_id.to_string();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    let selected_group = w.get_selected_private_group_id().to_string();
                    if selected_group == group_id_str {
                        tracing::debug!("Refreshing message list for group {} after member list update", group_id_str);
                        // Reload messages to show updated member names
                        crate::bin_ui::features::sidebar::private_group::load_private_group_messages(
                            &w,
                            &ui_ctx,
                            &group_id_str,
                        );
                    }
                }
            });
        }

        DomainEvent::AnonGroupUpdated { group_id, updated_by, .. } => {
            let weak = window.clone();
            let ui_ctx = ui_ctx.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    // Refresh group info
                    super::super::initial_data::refresh_anon_groups(&w, &ui_ctx);
                    tracing::info!(
                        group_id = %group_id,
                        updated_by = %updated_by,
                        "Anon group updated"
                    );
                }
            });
        }

        // =========================================================================
        // Anonymous DM Events
        // =========================================================================
        DomainEvent::AnonDmRequestReceived {
            conversation_id: _,
            sender_id: _,
            sender_display_name: _,
            intro_message: _,
            ..
        } => {
            let weak = window.clone();
            let ui_ctx = ui_ctx.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    // Refresh anon DM list to show the request
                    super::super::initial_data::refresh_anon_dms(&w, &ui_ctx);
                    
                    // Update pending message request count
                    if let Ok(requests) = ui_ctx.anon_dm_service.list_message_requests() {
                        w.set_pending_message_requests(requests.len() as i32);
                    }
                }
            });
        }

        DomainEvent::AnonDmRequestAccepted { conversation_id, peer_id, .. } => {
            let weak = window.clone();
            let ui_ctx = ui_ctx.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    // Refresh anon DM list
                    super::super::initial_data::refresh_anon_dms(&w, &ui_ctx);
                    tracing::info!(
                        conversation_id = %conversation_id,
                        peer_id = %peer_id,
                        "Anon DM request accepted"
                    );
                }
            });
        }

        DomainEvent::AnonDmMessageReceived { .. } => {
            // Message display handled by message state
            // But we need to refresh the sidebar to update latest message preview
            let weak = window.clone();
            let ui_ctx = ui_ctx.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    super::super::initial_data::refresh_anon_dms(&w, &ui_ctx);
                }
            });
        }

        DomainEvent::AnonDmInitReceived { conversation_id: _, sender_id: _, intro_message: _, .. } => {
            let weak = window.clone();
            let ui_ctx = ui_ctx.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    // Refresh anon DM list for new init
                    super::super::initial_data::refresh_anon_dms(&w, &ui_ctx);
                }
            });
        }

        DomainEvent::AnonDmMessageDeleted { .. }
        | DomainEvent::AnonDmMessageEdited { .. }
        | DomainEvent::AnonGroupMessageEdited { .. } => {
            // Handled by message state
        }

        // =========================================================================
        // Anonymous Call Events
        // =========================================================================
        DomainEvent::AnonCallOfferReceived {
            conversation_id,
            sender_id,
            call_room_id,
            room_name,
            call_type,
            caller_display_name,
            call_encryption_key,
            ..
        } => {
            // Store pending call state for later use when accepting/declining
            // We'll get a real token when accepting
            let dummy_token = chatter_linux::mailbox::CallRoomToken {
                id: String::new(),
                public_key: vec![],
                private_key: vec![],
                signature: vec![],
                randomizer: None,
            };
            
            // Convert call_room_id from Vec<u8> to [u8; 16]
            let mut call_room_id_array = [0u8; 16];
            if call_room_id.len() >= 16 {
                call_room_id_array.copy_from_slice(&call_room_id[..16]);
            } else {
                call_room_id_array[..call_room_id.len()].copy_from_slice(&call_room_id);
            }
            
            let pending = chatter_linux::services::anon_dm::PendingAnonCall {
                call_room_id: call_room_id_array,
                room_name: room_name.clone(),
                conversation_id,
                token: dummy_token, // Will be replaced when accepting
                is_video_call: call_type == 2,
                is_outgoing: false,  // This is an incoming call
                display_name: caller_display_name.clone(),
                encryption_key: call_encryption_key,
                sender_id: Some(sender_id),
                started_at: None, // Will be set when call is accepted
            };
            ui_ctx.anon_dm_service.store_pending_anon_call(room_name.clone(), pending);
            
            let weak = window.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    // Show incoming anonymous call UI
                    let display_name = if caller_display_name.is_empty() {
                        "Anonymous"
                    } else {
                        &caller_display_name
                    };
                    
                    let call_type_str = if call_type == 2 { "video" } else { "voice" };
                    
                    // Show incoming call modal
                    super::super::initial_data::show_incoming_call(
                        &w,
                        &room_name,  // Use room name as call ID
                        &sender_id.to_string(),
                        display_name,
                        call_type_str,
                        true,  // Anonymous calls are always verified
                        "",    // No identity warning for anonymous calls
                        true,  // anonymous call
                    );
                    
                    // Store the offer details for when user answers
                    // We'll get a token and send the answer when they click accept
                    tracing::info!(
                        conversation_id = %conversation_id,
                        room_name = %room_name,
                        call_type = call_type,
                        "Showing incoming anonymous call UI"
                    );
                }
            });
        }

        DomainEvent::AnonCallAnswerReceived { 
            conversation_id, 
            accepted,
            call_room_id,
            callee_display_name,
            .. 
        } => {
            let weak = window.clone();
            let ui_ctx = ui_ctx.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    if accepted {
                        // Call was accepted - connect to LiveKit
                        tracing::info!(
                            conversation_id = %conversation_id,
                            "Anonymous call accepted, connecting to LiveKit"
                        );
                        
                        // Get the pending call info from the service
                        // We need to determine the room name from the call_room_id
                        // Since we don't have the room name here, we need to iterate through pending calls
                        // or store it differently. For now, let's construct it.
                        let room_name = format!("anon-{}", uuid::Uuid::from_bytes(call_room_id.as_slice().try_into().unwrap_or_default()));
                        if let Some(pending) = ui_ctx.anon_dm_service.take_pending_anon_call(&room_name) {
                            // Validate that this matches our pending call
                            if call_room_id.as_slice() != pending.call_room_id.as_slice() {
                                tracing::warn!(
                                    "Call answer received for different room ID: expected {:?}, got {:?}",
                                    pending.call_room_id,
                                    call_room_id
                                );
                                return;
                            }
                            
                            // Mark call as started for duration tracking
                            ui_ctx.anon_dm_service.mark_anon_call_started(&pending.room_name);
                            
                            // Join the call room using our token
                            let anon_dm_service = ui_ctx.anon_dm_service.clone();
                            let weak = weak.clone();
                            
                            let call_service = ui_ctx.call_service.clone();
                            slint::spawn_local(async move {
                                // Get a fresh token for joining (the stored one may have been used for signaling)
                                let join_token = match anon_dm_service.get_available_call_room_token() {
                                    Ok(Some(t)) => {
                                        let _ = anon_dm_service.mark_call_room_token_used(&t.id);
                                        t
                                    }
                                    _ => {
                                        // Try requesting more tokens
                                        match anon_dm_service.request_call_room_tokens(5).await {
                                            Ok(_) => match anon_dm_service.get_available_call_room_token() {
                                                Ok(Some(t)) => {
                                                    let _ = anon_dm_service.mark_call_room_token_used(&t.id);
                                                    t
                                                }
                                                _ => {
                                                    tracing::error!("No call room tokens available for join");
                                                    return;
                                                }
                                            },
                                            Err(e) => {
                                                tracing::error!("Failed to request call room tokens: {}", e);
                                                return;
                                            }
                                        }
                                    }
                                };
                                // Use fresh token for joining the call room
                                match anon_dm_service.join_anon_call_room(
                                    &pending.room_name,
                                    &join_token,
                                    &pending.display_name,
                                    pending.is_video_call,
                                ).await {
                                    Ok(room_info) => {
                                        tracing::info!(
                                            "Got LiveKit credentials for anonymous call with token"
                                        );
                                        
                                        // Get CallService and connect to LiveKit
                                        let service = match call_service.get() {
                                            Some(svc) => svc.clone(),
                                            None => {
                                                tracing::error!("CallService not initialized");
                                                if let Some(w) = weak.upgrade() {
                                                    super::super::initial_data::hide_incoming_call(&w);
                                                }
                                                ui_ctx.anon_dm_service.clear_pending_anon_call(&pending.room_name);
                                                return;
                                            }
                                        };
                                        
                                        // Create dummy IDs for anonymous calls
                                        let call_id = chatter_linux::domain::ids::CallId::new();
                                        let channel_id = chatter_linux::domain::ids::ChannelId::new();
                                        
                                        // Connect to LiveKit with the access token and E2EE key
                                        match service.connect_with_token(
                                            &room_info.livekit_token,
                                            &room_info.livekit_url,
                                            pending.encryption_key.clone(),
                                            &pending.room_name,
                                            call_id,
                                            channel_id,
                                        ).await {
                                            Ok(()) => {
                                                tracing::info!("Connected to LiveKit for anonymous call (outgoing)");
                                                
                                                // Publish microphone immediately
                                                if let Err(e) = service.publish_microphone(None).await {
                                                    tracing::error!("Failed to publish microphone: {}", e);
                                                } else {
                                                    tracing::info!("Microphone published successfully");
                                                }
                                                
                                                // Update UI
                                                if let Some(w) = weak.upgrade() {
                                                    super::super::initial_data::hide_incoming_call(&w);
                                                    
                                                    // Create participants for UI
                                                    let local_participant = crate::CallParticipantData {
                                                        user_id: "You".into(),
                                                        display_name: "You".into(),
                                                        avatar_url: "".into(),
                                                        is_muted: false,
                                                        is_video_enabled: pending.is_video_call,
                                                        is_screen_sharing: false,
                                                        is_speaking: false,
                                                    };
                                                    
                                                    let remote_participant = crate::CallParticipantData {
                                                        user_id: "Anonymous".into(),
                                                        display_name: callee_display_name.into(),
                                                        avatar_url: "".into(),
                                                        is_muted: false,
                                                        is_video_enabled: pending.is_video_call,
                                                        is_screen_sharing: false,
                                                        is_speaking: false,
                                                    };
                                                    
                                                    // Derive verification emojis from shared E2EE key + room name
                                                    let mut e2ee_key = [0u8; 32];
                                                    if pending.encryption_key.len() >= 32 { e2ee_key.copy_from_slice(&pending.encryption_key[..32]); }
                                                    let verification = chatter_linux::services::CallVerification::derive_verification_emojis(
                                                        &e2ee_key,
                                                        &pending.room_name,
                                                    );
                                                    let (e1, e2, e3, e4) = chatter_linux::services::CallVerification::to_slint_tuple(&verification);

                                                    // Create call data for UI (use room_name as call_id so "anon-" prefix is preserved for cleanup)
                                                    let call_data = crate::ActiveCallData {
                                                        call_id: pending.room_name.clone().into(),
                                                        channel_id: channel_id.to_string().into(),
                                                        title: "Anonymous Call".into(),
                                                        is_video_call: pending.is_video_call,
                                                        is_anonymous_call: true,
                                                        is_encrypted: true,
                                                        is_identity_verified: true,
                                                        identity_warning: "".into(),
                                                        participant_count: 2,
                                                        duration_seconds: 0,
                                                        participants: slint::ModelRc::new(slint::VecModel::from(vec![
                                                            local_participant,
                                                            remote_participant,
                                                        ])),
                                                        verification_emoji_1: e1.into(),
                                                        verification_emoji_2: e2.into(),
                                                        verification_emoji_3: e3.into(),
                                                        verification_emoji_4: e4.into(),
                                                    };
                                                    
                                                    w.set_active_call_data(call_data);
                                                    w.set_call_is_muted(false);
                                                    w.set_call_is_video_enabled(pending.is_video_call);
                                                    w.set_call_is_screen_sharing(false);
                                                    w.set_active_call_visible(true);
                                                    
                                                    // Start timers
                                                    crate::bin_ui::features::calls::timers::start_call_timer(&w, None);
                                                    crate::bin_ui::features::calls::timers::start_video_frame_timer(&w, &service);
                                                    
                                                    // Publish camera if video call
                                                    if pending.is_video_call {
                                                        let service = service.clone();
                                                        slint::spawn_local(async move {
                                                            if let Err(e) = service.publish_camera(None).await {
                                                                tracing::error!("Failed to publish camera: {}", e);
                                                            } else {
                                                                tracing::info!("Camera published successfully");
                                                            }
                                                        }).unwrap();
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                tracing::error!("Failed to connect to LiveKit: {}", e);
                                                
                                                // Create failed call history entry
                                                let channel_id_str = format!("anon-dm-{}", pending.conversation_id);
                                                if let Ok(channel_id) = channel_id_str.parse::<chatter_linux::domain::ids::ChannelId>() {
                                                    let anonymous_user_id = chatter_linux::domain::ids::UserId::new();
                                                    crate::bin_ui::features::calls::call_history::create_call_history_message(
                                                        ui_ctx.message_service.clone(),
                                                        &channel_id,
                                                        &pending.room_name,
                                                        &anonymous_user_id,
                                                        &pending.display_name,
                                                        pending.is_outgoing,
                                                        0, // Failed call, no duration
                                                        true, // Mark as missed/failed
                                                    );
                                                }
                                                
                                                // Show error to user
                                                if let Some(w) = weak.upgrade() {
                                                    super::super::initial_data::hide_incoming_call(&w);
                                                    // TODO: Show toast notification about call failure
                                                }
                                                ui_ctx.anon_dm_service.clear_pending_anon_call(&pending.room_name);
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            "Failed to join anonymous call room: {}", e
                                        );
                                        
                                        // Create failed call history entry
                                        let channel_id_str = format!("anon-dm-{}", pending.conversation_id);
                                        if let Ok(channel_id) = channel_id_str.parse::<chatter_linux::domain::ids::ChannelId>() {
                                            let anonymous_user_id = chatter_linux::domain::ids::UserId::new();
                                            crate::bin_ui::features::calls::call_history::create_call_history_message(
                                                ui_ctx.message_service.clone(),
                                                &channel_id,
                                                &pending.room_name,
                                                &anonymous_user_id,
                                                &pending.display_name,
                                                pending.is_outgoing,
                                                0, // Failed call, no duration
                                                true, // Mark as missed/failed
                                            );
                                        }
                                        
                                        // Hide call UI on error
                                        if let Some(w) = weak.upgrade() {
                                            super::super::initial_data::hide_incoming_call(&w);
                                            // TODO: Show toast notification about room join failure
                                        }
                                        ui_ctx.anon_dm_service.clear_pending_anon_call(&pending.room_name);
                                    }
                                }
                            }).unwrap();
                        }
                    } else {
                        // Call was declined
                        tracing::info!(
                            conversation_id = %conversation_id,
                            "Anonymous call declined by peer"
                        );
                        
                        // Create declined call history entry
                        // Construct room name from call_room_id
                        let room_name = format!("anon-{}", uuid::Uuid::from_bytes(call_room_id.as_slice().try_into().unwrap_or_default()));
                        if let Some(pending) = ui_ctx.anon_dm_service.take_pending_anon_call(&room_name) {
                            let channel_id_str = format!("anon-dm-{}", pending.conversation_id);
                            if let Ok(channel_id) = channel_id_str.parse::<chatter_linux::domain::ids::ChannelId>() {
                                let anonymous_user_id = chatter_linux::domain::ids::UserId::new();
                                crate::bin_ui::features::calls::call_history::create_call_history_message(
                                    ui_ctx.message_service.clone(),
                                    &channel_id,
                                    &pending.room_name,
                                    &anonymous_user_id,
                                    &callee_display_name,
                                    pending.is_outgoing,
                                    0, // Declined call, no duration
                                    true, // Mark as missed/declined
                                );
                            }
                            // Already consumed by take_pending_anon_call
                        }
                        
                        w.set_active_call_visible(false);
                        super::super::initial_data::hide_incoming_call(&w);
                    }
                }
            });
        }

        DomainEvent::AnonCallHangupReceived { conversation_id, .. } => {
            let weak = window.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    // Hide any call-related UI elements
                    super::super::initial_data::hide_incoming_call(&window);
                    tracing::info!(conversation_id = %conversation_id, "Anon call ended");
                }
            });
        }
        
        // Message send events (handled by message state and UI)
        DomainEvent::MessageSent { message_id: _, channel_id: _, content: _, .. } => {
            tracing::debug!("Message sent successfully");
            // UI updates handled by message state
        }
        
        DomainEvent::MessageSendFailed { channel_id, error } => {
            tracing::error!(
                channel_id = %channel_id,
                error = %error,
                "Message send failed"
            );
            // TODO: Show error notification in UI
        }

        // =========================================================================
        // Channel Member Events
        // =========================================================================
        DomainEvent::ChannelMemberJoined { channel_id, user_id, display_name, message: _ } => {
            tracing::info!(
                channel_id = %channel_id,
                user_id = %user_id,
                display_name = %display_name,
                "Channel member joined"
            );
            // System message is already added to message state by EventHandlers
            // UI will update automatically through message state observation
        }

        DomainEvent::ChannelMemberLeft { channel_id, user_id, display_name, message: _ } => {
            tracing::info!(
                channel_id = %channel_id,
                user_id = %user_id,
                display_name = %display_name,
                "Channel member left"
            );
            // System message is already added to message state by EventHandlers
            // UI will update automatically through message state observation
        }

        // =========================================================================
        // User Presence Updates
        // =========================================================================
        DomainEvent::UserPresenceChanged {
            user_id,
            presence,
            timestamp,
        } => {
            let weak = window.clone();
            let user_id_str = user_id.to_string();
            let presence_str = match presence {
                chatter_linux::proto::UserPresence::Online => "online",
                chatter_linux::proto::UserPresence::Away => "away",
                chatter_linux::proto::UserPresence::DoNotDisturb => "dnd",
                chatter_linux::proto::UserPresence::Offline | _ => "offline",
            };

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(_w) = weak.upgrade() {
                    // Update user presence in UI
                    // This could update:
                    // 1. Sidebar DM status indicator
                    // 2. Chat header presence indicator
                    // 3. User profile presence status
                    
                    // TODO: Implement UI updates when the Slint UI has presence indicators
                    // For now, just log the update
                    tracing::debug!(
                        user_id = %user_id_str,
                        presence = presence_str,
                        timestamp,
                        "Would update presence in UI (not yet implemented)"
                    );
                }
            });

            tracing::info!(
                user_id = %user_id,
                presence = ?presence,
                timestamp,
                "User presence updated"
            );
        }

        // =========================================================================
        // Anonymous DM Profile Updates
        // =========================================================================
        DomainEvent::AnonDmProfileUpdated {
            conversation_id,
            user_id,
            display_name,
            avatar_url,
            bio,
            timestamp,
        } => {
            let weak = window.clone();
            let conversation_id_str = conversation_id.to_string();
            let user_id_str = user_id.to_string();
            let display_name_clone = display_name.clone();
            let avatar_url_clone = avatar_url.clone();
            
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    // Update the DM conversation in sidebar with new profile info
                    let dms = w.get_dm_conversations();
                    let mut found = false;
                    
                    for i in 0..dms.row_count() {
                        if let Some(mut dm) = dms.row_data(i) {
                            // Check if this is the conversation to update
                            // For DMs, the channel_id holds the conversation ID
                            if dm.channel_id == conversation_id_str {
                                // Update display name if provided
                                if let Some(name) = &display_name_clone {
                                    dm.display_name = name.clone().into();
                                }
                                
                                // Update avatar if provided
                                if let Some(avatar) = &avatar_url_clone {
                                    // Load avatar image from URL
                                    // For now, just log that we would update it
                                    tracing::debug!(
                                        "Would update avatar for {} to: {}",
                                        conversation_id_str, avatar
                                    );
                                }
                                
                                // Update the model
                                dms.set_row_data(i, dm);
                                found = true;
                                break;
                            }
                        }
                    }
                    
                    if found {
                        w.set_dm_conversations(dms);
                        tracing::info!(
                            "Updated profile for anon DM {} with user {}",
                            conversation_id_str, user_id_str
                        );
                    }
                }
            });
            
            tracing::info!(
                conversation_id = %conversation_id,
                user_id = %user_id,
                ?display_name,
                ?avatar_url,
                ?bio,
                timestamp,
                "Anonymous DM profile updated"
            );
        }
    }
}

/// Update unread count badge in sidebar.
fn update_unread_count(window: &MainWindow, channel_id: &str, count: u32) {
    // This would update the sidebar model
    // Implementation depends on how sidebar models are structured
    tracing::trace!(channel_id, count, "Updating unread count badge");
    let _ = (window, channel_id, count); // Suppress unused warnings for now
}

/// Handle pin/unpin domain events for UI update.
///
/// Updates the message's `is_pinned` flag, the pinned bar preview/count,
/// and the `PinnedState` so that navigation arrows work correctly.
fn handle_pin_ui_update(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<UIContext>,
    channel_id: chatter_linux::domain::ids::ChannelId,
    target_sent_timestamp: u64,
    unpin: bool,
) {
    let weak = window.clone();
    let pinned_state = ui_ctx.pinned_state.clone();
    let channel_id_str = channel_id.to_string();
    let target_ts_str = target_sent_timestamp.to_string();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };

        // Check if the pin is for the currently active conversation
        let selected_dm = w.get_selected_dm_id().to_string();
        let selected_channel = w.get_selected_channel_id().to_string();
        let selected_group = w.get_selected_private_group_id().to_string();

        let is_active = selected_dm.contains(&channel_id_str)
            || selected_channel == channel_id_str
            || selected_group == channel_id_str;
        if !is_active {
            return;
        }

        let messages = w.get_messages();
        let new_pinned = !unpin;
        let mut preview = String::new();

        // Find message by timestamp_ns, update is_pinned, generate preview
        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.timestamp_ns.as_str() == target_ts_str.as_str() {
                    preview = super::utils::generate_preview_from_ui_message(&msg);

                    let mut updated_msg = msg.clone();
                    updated_msg.is_pinned = new_pinned;
                    if let Some(model) = messages.as_any().downcast_ref::<slint::VecModel<crate::MessageItemData>>() {
                        model.set_row_data(i, updated_msg);
                    }

                    // Update pinned_state with the real message_id from the UI model
                    if let Ok(msg_id) = msg.message_id.to_string().parse::<chatter_linux::domain::ids::MessageId>() {
                        if new_pinned {
                            pinned_state.add_pinned_message(msg_id);
                        } else {
                            pinned_state.remove_pinned_message(&msg_id);
                        }
                    }
                    break;
                }
            }
        }

        let pinned_count = pinned_state.get().pinned_message_ids.len() as i32;
        if unpin {
            w.set_pinned_total_count(pinned_count);
            w.set_has_pinned_messages(pinned_count > 0);
            if pinned_count == 0 {
                w.set_pinned_message_preview("".into());
            }
        } else {
            w.set_pinned_total_count(pinned_count);
            w.set_has_pinned_messages(true);
            w.set_pinned_message_preview(preview.into());
            w.set_pinned_current_index(pinned_count);
        }

        tracing::debug!(
            channel_id = %channel_id_str,
            target_sent_timestamp,
            pinned = new_pinned,
            pinned_count,
            "Updated pin state from domain event"
        );
    });
}
