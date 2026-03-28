//! Anonymous DM request event handler.
//!
//! Handles DomainEvent::AnonDmRequestReceived to update the pending requests list.
//! Also handles DomainEvent::AnonDmRequestAccepted for updating conversation state.
//! And DomainEvent::AnonDmInitReceived for NDSS'21 ECIES-encrypted initial contacts.

use std::sync::Arc;

use slint::{Model, ModelRc, VecModel, Weak};

use chatter_core::client::ChatterClient;
use chatter_core::inbox::DomainEvent;
use crate::{DMListItemData, FriendRequestData, MainWindow};
use crate::bin_ui::converters::get_initials;
use crate::bin_ui::features::auth::inbox::unified_actions::{self, EventSource};

/// Spawn anonymous DM request handler to process incoming request events.
///
/// Subscribes to:
/// - `DomainEvent::AnonDmRequestReceived` - Adds to pending requests in UI
/// - `DomainEvent::AnonDmRequestAccepted` - Updates conversation to active in sidebar
///
/// Returns a handle to the spawned task.
pub fn spawn_anon_dm_request_handler(
    weak_window: Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
) -> tokio::task::JoinHandle<()> {
    let anon_dm_service = ui_ctx.anon_dms().clone();
    let chat_state = ui_ctx.chat_state().clone();
    let pinned_state = ui_ctx.pinned_state().clone();
    let ui_ctx = ui_ctx.clone();
    let mut event_rx = ui_ctx.event_bus().subscribe_domain();

    tokio::spawn(async move {
        tracing::info!("[ANON_DM_HANDLER] Started listening for domain events");
        loop {
            let event = match event_rx.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(dropped_count = n, "[ANON_DM_HANDLER] Domain event receiver lagged, continuing");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            tracing::debug!("[ANON_DM_HANDLER] Received domain event: {:?}", std::mem::discriminant(&event));
            match event {
                DomainEvent::AnonDmRequestReceived {
                    conversation_id,
                    sender_id,
                    sender_mailbox_pk: _,
                    intro_message,
                    profile_key: _,
                    sender_display_name,
                    sender_avatar,
                } => {
                    // Display name is required - reject if missing
                    let sender_name = match sender_display_name {
                        Some(name) if !name.is_empty() => name,
                        _ => {
                            tracing::warn!(
                                conversation_id = %conversation_id,
                                sender_id = %sender_id,
                                "Ignoring anon DM request with missing display name"
                            );
                            continue;
                        }
                    };
                    tracing::info!(
                        conversation_id = %conversation_id,
                        sender_id = %sender_id,
                        sender_name = %sender_name,
                        has_intro = intro_message.is_some(),
                        has_avatar = sender_avatar.is_some(),
                        "Received anonymous DM request - adding to pending list"
                    );

                    // Add to pending requests in UI (no auto-accept)
                    let conv_id_str = conversation_id.to_string();
                    let peer_name = sender_name.clone();
                    let intro = intro_message.unwrap_or_default();
                    let weak = weak_window.clone();

                    // Decode avatar OUTSIDE UI thread - image decoding is expensive
                    // Only raw pixels are Send, so decode to RGBA bytes here
                    let avatar_pixels: Option<(Vec<u8>, u32, u32)> = sender_avatar.as_ref().and_then(|bytes| {
                        if bytes.is_empty() {
                            return None;
                        }
                        image::load_from_memory(bytes).ok().map(|img| {
                            let rgba = img.to_rgba8();
                            let (w, h) = rgba.dimensions();
                            (rgba.into_raw(), w, h)
                        })
                    });

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            // Create slint::Image on UI thread from pre-decoded pixels
                            let (avatar_source, has_avatar) = match avatar_pixels {
                                Some((pixels, width, height)) => {
                                    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                        &pixels, width, height,
                                    );
                                    (slint::Image::from_rgba8(buffer), true)
                                }
                                None => (slint::Image::default(), false),
                            };

                            // Add to private requests list
                            let current_requests = w.get_friends_private_requests();
                            let mut requests: Vec<FriendRequestData> = (0..current_requests.row_count())
                                .filter_map(|i| current_requests.row_data(i))
                                .collect();

                            // Add new request
                            requests.push(FriendRequestData {
                                user_id: conv_id_str.clone().into(), // conversation_id
                                display_name: peer_name.clone().into(),
                                username: "Private".into(),
                                avatar_url: "".into(),
                                avatar_source,
                                has_avatar,
                                sent_at: if intro.is_empty() {
                                    "Just now".into()
                                } else {
                                    intro.into()
                                },
                            });

                            w.set_friends_private_requests(ModelRc::new(VecModel::from(requests.clone())));

                            // Update sidebar badge count
                            w.set_pending_message_requests(requests.len() as i32);

                            tracing::debug!(
                                conversation_id = %conv_id_str,
                                peer_name = %peer_name,
                                request_count = requests.len(),
                                "Added anonymous DM request to pending list and updated sidebar badge"
                            );
                        }
                    });
                }
                DomainEvent::AnonDmRequestAccepted {
                    conversation_id,
                    peer_id,
                    peer_mailbox_pk: _,
                    profile_key: _,
                    peer_display_name,
                    peer_avatar,
                } => {
                    // Our outgoing request was accepted - update sidebar
                    // Display name is required - reject if missing
                    let peer_name = if peer_display_name.is_empty() {
                        tracing::warn!(
                            conversation_id = %conversation_id,
                            peer_id = %peer_id,
                            "Ignoring acceptance with missing display name"
                        );
                        continue;
                    } else {
                        peer_display_name.clone()
                    };
                    tracing::info!(
                        conversation_id = %conversation_id,
                        peer_id = %peer_id,
                        peer_name = %peer_name,
                        has_avatar = peer_avatar.is_some(),
                        "Our anonymous DM request was accepted"
                    );

                    // Get the full conversation to add to sidebar
                    let dm = match anon_dm_service.get_conversation(&conversation_id) {
                        Ok(Some(dm)) => dm,
                        Ok(None) => {
                            tracing::warn!(
                                conversation_id = %conversation_id,
                                "Conversation not found after acceptance"
                            );
                            continue;
                        }
                        Err(e) => {
                            tracing::warn!(
                                conversation_id = %conversation_id,
                                error = %e,
                                "Failed to get conversation after acceptance"
                            );
                            continue;
                        }
                    };

                    let dm_id = dm.id;
                    let weak = weak_window.clone();
                    let peer_name_for_ui = peer_name.clone();

                    // Decode avatar BEFORE invoke_from_event_loop to avoid blocking UI thread
                    let avatar_bytes = peer_avatar.as_ref().or(dm.peer_avatar.as_ref());
                    let avatar_pixels: Option<(Vec<u8>, u32, u32)> = avatar_bytes.and_then(|bytes| {
                        image::load_from_memory(bytes).ok().map(|img| {
                            let rgba = img.to_rgba8();
                            let (w, h) = rgba.dimensions();
                            (rgba.into_raw(), w, h)
                        })
                    });

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            // Create Slint Image from pre-decoded pixels (fast)
                            let (avatar_source, has_avatar) = match avatar_pixels {
                                Some((pixels, width, height)) => {
                                    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                        &pixels, width, height,
                                    );
                                    (slint::Image::from_rgba8(buffer), true)
                                }
                                None => (slint::Image::default(), false),
                            };

                            // Update existing pending DM or add new active DM
                            let current_dms = w.get_dm_conversations();
                            let mut dm_vec: Vec<DMListItemData> = Vec::new();
                            let channel_id_str = format!("anon_dm:{}", dm_id);

                            // Filter out old pending version and collect others
                            for i in 0..current_dms.row_count() {
                                if let Some(existing_dm) = current_dms.row_data(i) {
                                    if existing_dm.channel_id.as_str() != channel_id_str {
                                        dm_vec.push(existing_dm);
                                    }
                                }
                            }

                            // Add active conversation
                            dm_vec.push(DMListItemData {
                                channel_id: channel_id_str.into(),
                                user_id: peer_id.to_string().into(),
                                display_name: format!("\u{1F512} {}", peer_name_for_ui).into(),
                                avatar_url: "".into(),
                                avatar_source,
                                has_avatar,
                                initials: get_initials(&peer_name_for_ui).into(),
                                presence_status: "online".into(),
                                last_message: "Conversation started".into(),
                                timestamp: chrono::Local::now().format("%H:%M").to_string().into(),
                                unread_count: 0,
                                is_typing: false,
                            });

                            w.set_dm_conversations(ModelRc::new(VecModel::from(dm_vec)));

                            tracing::info!(
                                conversation_id = %dm_id,
                                peer_name = %peer_name_for_ui,
                                "Updated sidebar with active anonymous DM"
                            );
                        }
                    });
                }
                DomainEvent::AnonDmMessageReceived {
                    conversation_id,
                    sender_id: _,
                    message_id: _,
                    sent_at,
                } => {
                    // UI insertion and sidebar updates are handled by domain_handler::anon_dm.
                    // This handler only sends read receipts when the conversation is active.
                    let channel_id_str = format!("anon_dm:{}", conversation_id);
                    let receipt_conv_id = conversation_id;
                    let receipt_msg_ts = sent_at;
                    let receipt_weak = weak_window.clone();
                    let receipt_client = ui_ctx.clone();

                    tokio::spawn(async move {
                        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
                        let _ = slint::invoke_from_event_loop(move || {
                            let is_selected = if let Some(w) = receipt_weak.upgrade() {
                                w.get_selected_dm_id().to_string() == channel_id_str
                            } else {
                                false
                            };
                            let _ = tx.send(is_selected);
                        });
                        match rx.await {
                            Ok(true) => {
                                let handle = chatter_core::services::conversation::ConversationHandle::new(
                                    chatter_core::state::ActiveConversation::AnonDm(receipt_conv_id),
                                    receipt_client,
                                );
                                if let Err(e) = handle.send_read_receipt(&[receipt_msg_ts]).await {
                                    tracing::warn!(error = %e, "[ANON_DM_RECEIPT] Failed to send read receipt");
                                } else {
                                    tracing::debug!("[ANON_DM_RECEIPT] Sent read receipt for ts={}", receipt_msg_ts);
                                }
                            }
                            Ok(false) => {
                                tracing::trace!("[ANON_DM_RECEIPT] Conversation not selected, skipping read receipt");
                            }
                            Err(_) => {
                                tracing::trace!("[ANON_DM_RECEIPT] UI check channel closed");
                            }
                        }
                    });
                }
                DomainEvent::AnonDmInitReceived {
                    conversation_id,
                    sender_id,
                    sender_display_name,
                    sender_avatar,
                    peer_mailbox_pk: _,
                    peer_identity_key: _,
                    peer_delivery_token: _,
                    intro_message,
                    server_timestamp: _,
                } => {
                    // NDSS'21 ECIES-encrypted initial contact received.
                    // Sender identity is revealed after decryption per NDSS'21 Section II-A.
                    // Use sender_display_name from the init message
                    // Do NOT use user_service - these are strangers
                    let sender_name = if sender_display_name.is_empty() {
                        format!("User {}", &sender_id.to_string()[..8])
                    } else {
                        sender_display_name.clone()
                    };
                    tracing::info!(
                        conversation_id = %conversation_id,
                        sender_id = %sender_id,
                        sender_name = %sender_name,
                        intro_len = intro_message.len(),
                        has_avatar = sender_avatar.is_some(),
                        avatar_size = sender_avatar.as_ref().map(|v| v.len()).unwrap_or(0),
                        "[ANON_DM_INIT] Received ECIES init message"
                    );

                    // Conversation creation happens in parallel via core EventHandlers.
                    // We don't need to query the DB — the event carries all the data we need.

                    // Add to pending requests in UI
                    let conv_id_str = conversation_id.to_string();
                    let peer_name = sender_name.clone();
                    let intro = if intro_message.is_empty() {
                        "Private contact request".to_string()
                    } else {
                        intro_message.clone()
                    };
                    let weak = weak_window.clone();

                    // Decode avatar BEFORE invoke_from_event_loop to avoid blocking UI thread
                    let avatar_pixels: Option<(Vec<u8>, u32, u32)> = sender_avatar.as_ref().and_then(|bytes| {
                        image::load_from_memory(bytes).ok().map(|img| {
                            let rgba = img.to_rgba8();
                            let (w, h) = rgba.dimensions();
                            (rgba.into_raw(), w, h)
                        })
                    });

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            // Create Slint Image from pre-decoded pixels (fast)
                            let (avatar_source, has_avatar) = match avatar_pixels {
                                Some((pixels, width, height)) => {
                                    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                        &pixels, width, height,
                                    );
                                    (slint::Image::from_rgba8(buffer), true)
                                }
                                None => (slint::Image::default(), false),
                            };

                            // Add to private requests list
                            let current_requests = w.get_friends_private_requests();
                            let mut requests: Vec<FriendRequestData> = (0..current_requests.row_count())
                                .filter_map(|i| current_requests.row_data(i))
                                .collect();

                            // Add new request with sender's identity (revealed after ECIES decryption)
                            requests.push(FriendRequestData {
                                user_id: conv_id_str.clone().into(), // conversation_id
                                display_name: format!("\u{1F512} {}", peer_name).into(), // Lock icon for private
                                username: "Private".into(),
                                avatar_url: "".into(),
                                avatar_source,
                                has_avatar,
                                sent_at: intro.into(),
                            });

                            w.set_friends_private_requests(ModelRc::new(VecModel::from(requests.clone())));

                            // Update sidebar badge count
                            w.set_pending_message_requests(requests.len() as i32);

                            tracing::info!(
                                conversation_id = %conv_id_str,
                                peer_name = %peer_name,
                                request_count = requests.len(),
                                "Added ECIES init request to pending list and updated sidebar badge"
                            );
                        }
                    });
                }
                // =============================================================
                // Anonymous DM Action Events (typing, reactions, pins, etc.)
                // These events are emitted by AnonDmHandler with channel_id
                // derived from AnonDmId. We check if the channel_id is an
                // anon DM by checking if our selected_dm_id matches.
                // =============================================================
                DomainEvent::TypingStarted { channel_id, user_id: _ } => {
                    unified_actions::handle_typing(&weak_window, &chat_state, EventSource::Channel(channel_id), true);
                }
                DomainEvent::TypingStopped { channel_id, user_id: _ } => {
                    unified_actions::handle_typing(&weak_window, &chat_state, EventSource::Channel(channel_id), false);
                }
                // ReactionAdded/ReactionRemoved are handled by domain_handler::handle_reaction_update
                // (which handles all conversation types). Handling them here too caused double-counting.
                DomainEvent::ReactionAdded { .. } | DomainEvent::ReactionRemoved { .. } => {}
                DomainEvent::MessagePinned {
                    message_id,
                    channel_id,
                    ..
                } => {
                    // Use message_id's timestamp for matching
                    let target_sent_timestamp = message_id.as_bytes()
                        .get(..8)
                        .and_then(|b| b.try_into().ok())
                        .map(u64::from_be_bytes)
                        .unwrap_or(0);
                    unified_actions::handle_pin(
                        &weak_window, &chat_state, &pinned_state,
                        EventSource::Channel(channel_id),
                        target_sent_timestamp, true, // is_pinned = true
                    );
                }
                DomainEvent::MessageUnpinned {
                    message_id,
                    channel_id,
                    ..
                } => {
                    let target_sent_timestamp = message_id.as_bytes()
                        .get(..8)
                        .and_then(|b| b.try_into().ok())
                        .map(u64::from_be_bytes)
                        .unwrap_or(0);
                    unified_actions::handle_pin(
                        &weak_window, &chat_state, &pinned_state,
                        EventSource::Channel(channel_id),
                        target_sent_timestamp, false, // is_pinned = false (unpin)
                    );
                }
                DomainEvent::MessageDeleted {
                    message_id,
                    channel_id: Some(channel_id),
                    ..
                } => {
                    let target_sent_timestamp = message_id.as_bytes()
                        .get(..8)
                        .and_then(|b| b.try_into().ok())
                        .map(u64::from_be_bytes)
                        .unwrap_or(0);
                    unified_actions::handle_delete(
                        &weak_window, &chat_state,
                        EventSource::Channel(channel_id),
                        target_sent_timestamp,
                    );
                }
                DomainEvent::AnonDmMessageDeleted {
                    conversation_id,
                    message_timestamp,
                } => {
                    unified_actions::handle_delete(
                        &weak_window, &chat_state,
                        EventSource::Channel(conversation_id.into()),
                        message_timestamp,
                    );
                }
                DomainEvent::AnonDmMessageEdited {
                    conversation_id,
                    message_timestamp,
                    new_text,
                } => {
                    unified_actions::handle_edit(
                        &weak_window, &chat_state,
                        EventSource::Channel(conversation_id.into()),
                        message_timestamp, new_text,
                    );
                }
                DomainEvent::MessagesRead {
                    message_timestamps,
                    channel_id,
                    ..
                } => {
                    // Use channel_id from event if present, otherwise try to determine from UI
                    let weak = weak_window.clone();
                    let cs = chat_state.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            // Use channel_id from event if available
                            let ch_id = if let Some(id) = channel_id {
                                Some(id)
                            } else {
                                // Fallback: try to determine from selected DM
                                let selected_dm = w.get_selected_dm_id().to_string();
                                if let Some(anon_dm_id_str) = selected_dm.strip_prefix("anon_dm:") {
                                    anon_dm_id_str.parse::<chatter_core::domain::ChannelId>().ok()
                                } else {
                                    None
                                }
                            };

                            if let Some(channel_id) = ch_id {
                                unified_actions::handle_receipt(
                                    &weak, &cs,
                                    EventSource::Channel(channel_id),
                                    2, // read
                                    message_timestamps.clone(),
                                );
                            }
                        }
                    });
                }
                DomainEvent::MessagesDelivered {
                    message_timestamps,
                    ..
                } => {
                    // For anon DM receipts, check if we're viewing an anon DM
                    let weak = weak_window.clone();
                    let cs = chat_state.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            let selected_dm = w.get_selected_dm_id().to_string();
                            if let Some(anon_dm_id_str) = selected_dm.strip_prefix("anon_dm:") {
                                if let Ok(channel_id) = anon_dm_id_str.parse::<chatter_core::domain::ChannelId>() {
                                    unified_actions::handle_receipt(
                                        &weak, &cs,
                                        EventSource::Channel(channel_id),
                                        1, // delivered
                                        message_timestamps.clone(),
                                    );
                                }
                            }
                        }
                    });
                }
                DomainEvent::MessageEdited {
                    message_id,
                    new_content,
                    channel_id,
                } => {
                    // Handle message edit for anon DMs
                    let target_sent_timestamp = message_id.as_bytes()
                        .get(..8)
                        .and_then(|b| b.try_into().ok())
                        .map(u64::from_be_bytes)
                        .unwrap_or(0);
                    let new_body = match new_content {
                        chatter_core::domain::message::MessageContent::Text(text) => text,
                        _ => "[Unsupported content]".to_string(),
                    };
                    unified_actions::handle_edit(
                        &weak_window, &chat_state,
                        EventSource::Channel(channel_id),
                        target_sent_timestamp, new_body,
                    );
                }
                _ => {
                    // Ignore other events
                }
            }
        }
    })
}
