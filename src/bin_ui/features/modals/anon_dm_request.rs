//! Anonymous DM request event handler.
//!
//! Handles DomainEvent::AnonDmRequestReceived to update the pending requests list.
//! Also handles DomainEvent::AnonDmRequestAccepted for updating conversation state.
//! And DomainEvent::AnonDmInitReceived for NDSS'21 ECIES-encrypted initial contacts.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, VecModel, Weak};

use chatter_linux::app::UIContext;
use chatter_linux::domain::anon_dm::AnonDm;
use chatter_linux::inbox::DomainEvent;
use crate::{DMListItemData, FriendRequestData, MainWindow, MessageItemData};
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
    ui_ctx: &Arc<UIContext>,
) -> tokio::task::JoinHandle<()> {
    let anon_dm_service = ui_ctx.anon_dm_service.clone();
    let attachment_service = ui_ctx.attachment_service.clone();
    let chat_state = ui_ctx.chat_state.clone();
    let pinned_state = ui_ctx.pinned_state.clone();
    let mut event_rx = ui_ctx.event_bus.subscribe_domain();

    tokio::spawn(async move {
        tracing::info!("[ANON_DM_HANDLER] Started listening for domain events");
        while let Ok(event) = event_rx.recv().await {
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
                    sender_id,
                    message_id,
                    inner_content,
                    sent_at,
                } => {
                    // Message received in an active anon DM conversation
                    // Update the sidebar preview and message list if conversation is selected
                    tracing::info!(
                        conversation_id = %conversation_id,
                        sender_id = %sender_id,
                        message_id = %message_id,
                        sent_at = sent_at,
                        "[ANON_DM_UI] Received message event with sent_at"
                    );

                    // Get sender display name from conversation (strangers aren't in user_service)
                    let sender_name = match anon_dm_service.get_conversation(&conversation_id) {
                        Ok(Some(dm)) => dm.peer_display_name.unwrap_or_else(|| {
                            format!("User {}", &sender_id.to_string()[..8])
                        }),
                        _ => format!("User {}", &sender_id.to_string()[..8]),
                    };

                    // Extract text, attachments, and quote for display
                    let (text, attachment_pointers, has_quote, quote_author, quote_preview) = if let Some(chatter_linux::proto::content_v1::content::Content::DataMessage(data_msg)) = &inner_content.content {
                        let q_has = data_msg.quote.is_some();
                        let q_author = data_msg.quote.as_ref().map(|q| {
                            // Resolve author UUID to display name
                            if let Ok(uid) = q.author_aci.parse::<chatter_linux::domain::UserId>() {
                                anon_dm_service.get_conversation(&conversation_id)
                                    .ok()
                                    .flatten()
                                    .and_then(|dm| {
                                        if dm.peer_id == uid { dm.peer_display_name } else { None }
                                    })
                                    .unwrap_or_else(|| sender_name.clone())
                            } else {
                                q.author_aci.clone()
                            }
                        }).unwrap_or_default();
                        let q_preview = data_msg.quote.as_ref().map(|q| q.text.clone()).unwrap_or_default();
                        (data_msg.body.clone(), data_msg.attachments.clone(), q_has, q_author, q_preview)
                    } else {
                        ("[Message]".to_string(), Vec::new(), false, String::new(), String::new())
                    };

                    // Convert attachment pointers to display data (before closure)
                    let mut attachment_display: Vec<_> = attachment_pointers
                        .iter()
                        .filter(|p| !p.cdn_key.is_empty())
                        .map(|p| crate::bin_ui::converters::attachment_pointer_to_display_data(p))
                        .collect();
                    // Pre-decode thumbnails before UI thread
                    for att in &mut attachment_display {
                        att.decode_thumbnail();
                    }
                    let attachment_count = attachment_display.len();
                    let has_attachments = attachment_count > 0;

                    let channel_id_str = format!("anon_dm:{}", conversation_id);
                    let preview = if text.is_empty() && has_attachments {
                        format!("📎 {} attachment{}", attachment_count, if attachment_count == 1 { "" } else { "s" })
                    } else if text.chars().count() > 30 {
                        format!("{}...", text.chars().take(27).collect::<String>())
                    } else {
                        text.clone()
                    };
                    let timestamp = chrono::Local::now().format("%H:%M").to_string();
                    let sender_initials = get_initials(&sender_name);
                    let timestamp_ns_str = sent_at.to_string();
                    let weak = weak_window.clone();
                    let attachment_service_for_gifs = attachment_service.clone();

                    // Send read receipt immediately if conversation is currently selected
                    // Move the check inside invoke_from_event_loop to ensure proper UI thread access
                    let receipt_service = anon_dm_service.clone();
                    let receipt_conv_id = conversation_id;
                    let receipt_msg_ts = sent_at;
                    let receipt_channel_id_str = channel_id_str.clone();
                    let receipt_weak = weak_window.clone();

                    // Spawn a task that will check UI state in event loop then send receipt
                    tokio::spawn(async move {
                        // Use a oneshot channel to get the result from the UI thread
                        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();

                        let _ = slint::invoke_from_event_loop(move || {
                            let is_selected = if let Some(w) = receipt_weak.upgrade() {
                                let selected = w.get_selected_dm_id().to_string();
                                let matches = selected == receipt_channel_id_str;
                                tracing::info!(
                                    selected_dm = %selected,
                                    expected = %receipt_channel_id_str,
                                    matches = matches,
                                    "[ANON_DM_RECEIPT] Checking if conversation is selected for immediate receipt"
                                );
                                matches
                            } else {
                                tracing::warn!("[ANON_DM_RECEIPT] Window upgrade failed for selection check");
                                false
                            };
                            let _ = tx.send(is_selected);
                        });

                        // Wait for the result from UI thread
                        if let Ok(true) = rx.await {
                            tracing::info!(
                                conversation_id = %receipt_conv_id,
                                sent_at = receipt_msg_ts,
                                "[ANON_DM_RECEIPT] Sending immediate read receipt for incoming message"
                            );
                            if let Err(e) = receipt_service.send_read_receipt(&receipt_conv_id, vec![receipt_msg_ts]).await {
                                tracing::warn!(
                                    error = %e,
                                    "[ANON_DM_RECEIPT] Failed to send read receipt"
                                );
                            } else {
                                tracing::info!(
                                    conversation_id = %receipt_conv_id,
                                    "[ANON_DM_RECEIPT] Successfully sent immediate read receipt"
                                );
                            }
                        }
                    });

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            // Update DM sidebar preview
                            let current_dms = w.get_dm_conversations();
                            let mut dm_vec: Vec<crate::DMListItemData> = Vec::new();
                            let mut found = false;

                            for i in 0..current_dms.row_count() {
                                if let Some(mut dm) = current_dms.row_data(i) {
                                    if dm.channel_id.as_str() == channel_id_str {
                                        dm.last_message = preview.clone().into();
                                        dm.timestamp = timestamp.clone().into();
                                        // Move to top by adding first
                                        dm_vec.insert(0, dm);
                                        found = true;
                                    } else {
                                        dm_vec.push(dm);
                                    }
                                }
                            }

                            if found {
                                w.set_dm_conversations(slint::ModelRc::new(slint::VecModel::from(dm_vec)));
                                tracing::debug!(
                                    channel_id = %channel_id_str,
                                    "Updated anon DM sidebar preview"
                                );
                            }

                            // If this conversation is currently selected, append message to the list
                            let selected_dm = w.get_selected_dm_id().to_string();
                            if selected_dm == channel_id_str {
                                // Check if user is at bottom before updating (for auto-scroll)
                                let was_at_bottom = w.get_is_at_bottom();

                                // Create message display data and append to list
                                let current_messages = w.get_messages();
                                let mut messages: Vec<MessageItemData> = (0..current_messages.row_count())
                                    .filter_map(|i| current_messages.row_data(i))
                                    .collect();

                                // Convert attachment display data to Slint types
                                let slint_attachments: Vec<crate::MessageAttachmentData> = attachment_display
                                    .iter()
                                    .map(|a| a.to_slint())
                                    .collect();

                                // Create new message data with correct sent_at for ordering
                                let new_message = MessageItemData {
                                    message_id: message_id.as_str().into(),
                                    sender_id: sender_id.to_string().into(),
                                    sender_name: sender_name.clone().into(),
                                    sender_initials: sender_initials.clone().into(),
                                    sender_avatar_url: "".into(),
                                    sender_avatar_source: slint::Image::default(),
                                    has_avatar: false,
                                    content: text.clone().into(),
                                    timestamp: timestamp.clone().into(),
                                    timestamp_ns: timestamp_ns_str.clone().into(),
                                    is_own: false,
                                    is_edited: false,
                                    is_deleted: false,
                                    is_system: false,
                                    is_ai: false,
                                    has_attachments: !slint_attachments.is_empty(),
                                    attachment_count: slint_attachments.len() as i32,
                                    attachments: ModelRc::new(VecModel::from(slint_attachments)),
                                    has_reactions: false,
                                    reaction_summary: "".into(),
                                    reactions: ModelRc::default(),
                                    reply_count: 0,
                                    reply_count_text: "".into(),
                                    is_thread_reply: false,
                                    is_reply: has_quote,
                                    reply_to_sender_name: quote_author.clone().into(),
                                    reply_to_preview: quote_preview.clone().into(),
                                    show_sender: true,
                                    delivery_status: "".into(),
                                    is_pinned: false,
                                    is_call: false,
                                    call_type: "".into(),
                                    call_duration: "".into(),
                                };

                                // Insert in sorted position (by timestamp_ns ascending)
                                let new_ts: u64 = timestamp_ns_str.parse().unwrap_or(0);
                                let msg_count = current_messages.row_count();

                                // Find insert position
                                let insert_pos = (0..msg_count).find(|&i| {
                                    if let Some(m) = current_messages.row_data(i) {
                                        let existing_ts: u64 = m.timestamp_ns.to_string().parse().unwrap_or(0);
                                        existing_ts > new_ts
                                    } else {
                                        false
                                    }
                                }).unwrap_or(msg_count);

                                tracing::info!(
                                    channel_id = %channel_id_str,
                                    message_id = %message_id,
                                    sent_at = new_ts,
                                    insert_pos = insert_pos,
                                    msg_count = msg_count,
                                    "[ANON_DM_UI] Inserting message at position"
                                );

                                // Insert directly into existing model to avoid full re-render
                                if let Some(model) = current_messages.as_any().downcast_ref::<VecModel<MessageItemData>>() {
                                    model.insert(insert_pos, new_message);
                                } else {
                                    // Fallback: replace entire model (causes full re-render)
                                    messages.insert(insert_pos, new_message);
                                    w.set_messages(ModelRc::new(VecModel::from(messages)));
                                }

                                // Auto-register GIF animations for inline playback
                                crate::bin_ui::gif_state::auto_register_gifs_from_messages(
                                    w.as_weak(),
                                    attachment_service_for_gifs,
                                );

                                // Auto-scroll to bottom if user was at bottom before new message
                                if was_at_bottom {
                                    w.set_message_scroll_position(-99999.0);
                                    w.set_is_at_bottom(true);
                                }
                            }
                        }
                    });
                }
                DomainEvent::AnonDmInitReceived {
                    conversation_id,
                    sender_id,
                    sender_display_name,
                    sender_avatar,
                    peer_mailbox_pk,
                    peer_identity_key,
                    peer_delivery_token,
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
                        "[ANON_DM_INIT] Received ECIES init message - creating anon DM conversation"
                    );

                    // Create the conversation in the database with display name and avatar
                    let dm = AnonDm::from_request(
                        conversation_id,
                        sender_id,
                        peer_mailbox_pk,
                    ).with_display_name(&sender_display_name)
                     .with_avatar_opt(sender_avatar.clone());

                    // Store the conversation with peer keys
                    if let Err(e) = anon_dm_service.create_incoming_conversation(
                        &dm,
                        &peer_identity_key,
                        &peer_delivery_token,
                    ) {
                        tracing::error!(
                            conversation_id = %conversation_id,
                            error = %e,
                            "Failed to create anon DM conversation from ECIES init"
                        );
                        continue;
                    }

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
                DomainEvent::ReactionAdded {
                    emoji,
                    timestamp,
                    ..
                } => {
                    // Derive channel_id from message context
                    // Note: For anon DMs, we need the conversation_id
                    // Since ReactionAdded doesn't include channel_id, we rely on
                    // the currently selected conversation. The handler checks if
                    // the selected DM is an anon DM.
                    let weak = weak_window.clone();
                    let cs = chat_state.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            let selected_dm = w.get_selected_dm_id().to_string();
                            if let Some(anon_dm_id_str) = selected_dm.strip_prefix("anon_dm:") {
                                if let Ok(channel_id) = anon_dm_id_str.parse::<chatter_linux::domain::ChannelId>() {
                                    unified_actions::handle_reaction(
                                        &weak, &cs, EventSource::Channel(channel_id),
                                        emoji.clone(), false, timestamp,
                                    );
                                }
                            }
                        }
                    });
                }
                DomainEvent::ReactionRemoved {
                    emoji,
                    timestamp,
                    ..
                } => {
                    let weak = weak_window.clone();
                    let cs = chat_state.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            let selected_dm = w.get_selected_dm_id().to_string();
                            if let Some(anon_dm_id_str) = selected_dm.strip_prefix("anon_dm:") {
                                if let Ok(channel_id) = anon_dm_id_str.parse::<chatter_linux::domain::ChannelId>() {
                                    unified_actions::handle_reaction(
                                        &weak, &cs, EventSource::Channel(channel_id),
                                        emoji.clone(), true, timestamp,
                                    );
                                }
                            }
                        }
                    });
                }
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
                                    anon_dm_id_str.parse::<chatter_linux::domain::ChannelId>().ok()
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
                                if let Ok(channel_id) = anon_dm_id_str.parse::<chatter_linux::domain::ChannelId>() {
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
                        chatter_linux::domain::message::MessageContent::Text(text) => text,
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
