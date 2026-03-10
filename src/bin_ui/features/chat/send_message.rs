//! Send message callback for channels and DMs.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use chatter_linux::app::UIContext;
use chatter_linux::domain::anon_dm::AnonDmId;
use chatter_linux::domain::channel::ChannelType;

use crate::bin_ui::converters::{get_initials, message_to_display_data, attachment_to_display_data};
use crate::bin_ui::features::auth::inbox::update_channel_sidebar_preview;
use crate::bin_ui::features::chat::helpers::get_peer_id_from_dm_channel;
use super::typing::clear_outgoing_typing;
use crate::{DMListItemData, MainWindow, MessageItemData, PendingAttachmentData, PrivateGroupListItemData};

pub fn wire_send(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let message_service = ui_ctx.message_service.clone();
    let chat_state = ui_ctx.chat_state.clone();
    let auth_state = ui_ctx.auth_state.clone();
    let grpc_pool = ui_ctx.grpc_pool.clone();
    let mailbox_manager = ui_ctx.mailbox_manager.clone();
    let channel_service = ui_ctx.channel_service.clone();
    let attachment_service = ui_ctx.attachment_service.clone();
    let anon_group_service = ui_ctx.anon_group_service.clone();
    let anon_dm_service = ui_ctx.anon_dm_service.clone();
    let group_fanout = ui_ctx.group_fanout.get().cloned();
    let ui_ctx_jarvis = ui_ctx.clone();

    let weak_window = window.as_weak();

    window.on_send(move |text: slint::SharedString| {
        let text = text.to_string();
        let pending_attachments = chat_state.get_pending_attachments();
        let has_attachments = !pending_attachments.is_empty();

        if text.trim().is_empty() && !has_attachments { return; }

        // Check for Jarvis slash commands first (/ask, /summarize, /knowledge, /remember)
        if let Some(cmd) = crate::bin_ui::features::jarvis::parse_slash_command(&text) {
            let ui_ctx = ui_ctx_jarvis.clone();
            let channel_id = chat_state.get().active_channel_id;
            let weak = weak_window.clone();
            tokio::spawn(async move {
                handle_jarvis_slash_command(&ui_ctx, cmd, channel_id.as_ref(), weak).await;
            });
            return;
        }

        // Check for @Jarvis mention - intercept before sending to channel
        if crate::bin_ui::features::jarvis::contains_jarvis_mention(&text) {
            if let Some(query) = crate::bin_ui::features::jarvis::extract_jarvis_query(&text) {
                let ui_ctx = ui_ctx_jarvis.clone();
                let channel_id = chat_state.get().active_channel_id;
                let weak = weak_window.clone();
                tokio::spawn(async move {
                    crate::bin_ui::features::jarvis::handle_jarvis_mention(
                        &ui_ctx,
                        &query,
                        channel_id.as_ref(),
                        weak,
                    ).await;
                });
                // Don't send @Jarvis messages to the channel - they're Jarvis queries
                return;
            }
        }

        // Clear outgoing typing state (Signal: clearTypingTimers on message send)
        clear_outgoing_typing();
        if has_attachments { chat_state.clear_pending_attachments(); }

        // Special handling for Klipy GIFs: if the text is just a GIF marker, 
        // we can treat it differently (e.g. by wrapping it in an attachment later).
        // For now, we keep it as text but ensure the UI can render it.
        // In a full implementation, we might want to download and re-upload to our own CDN.
        
        let message_service = message_service.clone();
        let chat_state = chat_state.clone();
        let auth_state = auth_state.clone();
        let grpc_pool = grpc_pool.clone();
        let mailbox_manager = mailbox_manager.clone();
        let attachment_service = attachment_service.clone();
        let anon_group_service = anon_group_service.clone();
        let anon_dm_service = anon_dm_service.clone();
        let channel_service = channel_service.clone();
        let group_fanout = group_fanout.clone();
        let weak = weak_window.clone();

        let (selected_dm_id, selected_private_group_id) = weak.upgrade().map(|w| (w.get_selected_dm_id().to_string(), w.get_selected_private_group_id().to_string())).unwrap_or_default();

        if has_attachments {
            let weak_clear = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_clear.upgrade() {
                    w.set_pending_attachments(ModelRc::new(VecModel::from(Vec::<PendingAttachmentData>::new())));
                    w.set_composer_placeholder("Type a message...".into());
                }
            });
        }

        // Check for anonymous DM (prefixed with "anon_dm:")
        if let Some(anon_dm_id_str) = selected_dm_id.strip_prefix("anon_dm:") {
            let anon_dm_id: AnonDmId = match anon_dm_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid anon DM ID: {}", e);
                    return;
                }
            };

            tokio::spawn(async move {
                if !auth_state.is_logged_in() { return; }
                let current_user_id = auth_state.get().current_user_id.map(|id| id.to_string()).unwrap_or_default();
                let current_user_name = auth_state.get().display_name.unwrap_or_else(|| "Me".to_string());

                // Upload attachments first if any (skip if already uploaded with pointer)
                let mut attachment_pointers = Vec::new();
                let mut uploaded_attachments = Vec::new();
                for att in &pending_attachments {
                    // Check if attachment was pre-uploaded (e.g., GIF from picker)
                    if let Some(pointer) = &att.pointer {
                        attachment_pointers.push(pointer.clone());
                        // Get the full attachment for UI display
                        if let Ok(Some(full_att)) = attachment_service.get(&att.id) {
                            uploaded_attachments.push(full_att);
                        }
                        continue;
                    }

                    // Upload attachment to CDN
                    match attachment_service.upload_to_cdn_encrypted(&grpc_pool, &att.path).await {
                        Ok(res) => {
                            // Use to_attachment_pointer() to include all metadata (thumbnail, dimensions, etc.)
                            attachment_pointers.push(res.to_attachment_pointer());
                            // Get the full attachment for UI display
                            if let Ok(Some(full_att)) = attachment_service.get(&res.attachment_id) {
                                uploaded_attachments.push(full_att);
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to upload anon DM attachment: {}", e);
                        }
                    }
                }

                // Send via anon DM service with attachments
                match anon_dm_service.send_with_attachments(&anon_dm_id, &text, attachment_pointers).await {
                    Ok((message_id, sent_at)) => {
                        // Use the actual sent_at from the service for timestamp display
                        // This ensures the UI timestamp matches what was sent over the wire
                        let sent_datetime = chrono::DateTime::from_timestamp_nanos(sent_at as i64);
                        let timestamp_str = sent_datetime.with_timezone(&chrono::Local).format("%H:%M").to_string();
                        let message_id_str = message_id.as_str().to_string();
                        let sent_text = text.clone();
                        let sender_name = current_user_name.clone();
                        let sender_initials = get_initials(&sender_name);
                        // Convert to display data outside the closure (these are Send-safe)
                        let mut attachment_display_data: Vec<_> = uploaded_attachments
                            .iter()
                            .map(|a| attachment_to_display_data(a))
                            .collect();
                        // Pre-decode thumbnails before UI thread
                        for att in &mut attachment_display_data {
                            att.decode_thumbnail();
                        }

                        let attachment_service_for_gifs = attachment_service.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                // Convert to Slint types inside the event loop
                                let slint_attachments: Vec<_> = attachment_display_data
                                    .iter()
                                    .map(|a| a.to_slint())
                                    .collect();

                                let msg_item = MessageItemData {
                                    message_id: message_id_str.into(),
                                    sender_id: current_user_id.into(),
                                    sender_name: sender_name.into(),
                                    sender_initials: sender_initials.into(),
                                    sender_avatar_url: "".into(),
                                    content: sent_text.into(),
                                    timestamp: timestamp_str.into(),
                                    timestamp_ns: sent_at.to_string().into(),
                                    is_own: true,
                                    is_edited: false,
                                    is_deleted: false,
                                    is_system: false,
                                    is_ai: false,
                                    has_attachments: !slint_attachments.is_empty(),
                                    attachment_count: slint_attachments.len() as i32,
                                    attachments: ModelRc::new(VecModel::from(slint_attachments)),
                                    has_reactions: false,
                                    reaction_summary: "".into(),
                                    reply_count: 0,
                                    reply_count_text: "".into(),
                                    is_thread_reply: false,
                                    show_sender: true,
                                    delivery_status: "sent".into(),
                                    is_pinned: false,
                                    is_call: false,
                                    call_type: "".into(),
                                    call_duration: "".into(),
                                };

                                // Append directly to existing model to avoid full re-render
                                let model = w.get_messages();
                                if let Some(vec_model) = model.as_any().downcast_ref::<VecModel<MessageItemData>>() {
                                    vec_model.push(msg_item);
                                } else {
                                    // Fallback: replace entire model (causes full re-render)
                                    let mut messages: Vec<_> = (0..model.row_count()).filter_map(|i| model.row_data(i)).collect();
                                    messages.push(msg_item);
                                    w.set_messages(ModelRc::new(VecModel::from(messages)));
                                }
                                w.set_composer_text("".into());
                                w.set_message_scroll_position(-99999.0);
                                w.set_is_at_bottom(true);

                                // Auto-register GIF animations for sender's attachments
                                crate::bin_ui::gif_state::auto_register_gifs_from_messages(
                                    w.as_weak(),
                                    attachment_service_for_gifs,
                                );
                            }
                        });

                        tracing::info!("Sent anon DM message to conversation {}", anon_dm_id);
                    }
                    Err(e) => {
                        tracing::error!("Failed to send anon DM message: {}", e);
                    }
                }
            });
            return;
        }

        // Regular DM handling
        if !selected_dm_id.is_empty() {
            let peer_user_id = weak.upgrade().and_then(|w| get_peer_id_from_dm_channel(&w, &selected_dm_id));
            if let Some(peer_user_id) = peer_user_id {
                tokio::spawn(async move {
                    if !auth_state.is_logged_in() { return; }
                    let peer_id: chatter_linux::domain::UserId = match peer_user_id.parse() { Ok(id) => id, _ => return };
                    let current_user_id = auth_state.get().current_user_id.map(|id| id.to_string()).unwrap_or_default();
                    let current_user_name = auth_state.get().display_name.unwrap_or_else(|| "Me".to_string());
                    let dm_channel_id: chatter_linux::domain::ids::ChannelId = selected_dm_id.parse().unwrap_or_else(|_| peer_id.into());
                    let file_paths: Vec<_> = pending_attachments.iter().map(|att| att.path.as_path()).collect();

                    match message_service.send_dm_with_files(&peer_id, &dm_channel_id, &text, file_paths, &mailbox_manager).await {
                        Ok(result) => {
                            let gif_ids = result.gif_attachment_ids.clone();
                            let preview = if text.chars().count() > 50 { format!("You: {}...", text.chars().take(47).collect::<String>()) } else { format!("You: {}", text) };
                            let sidebar_peer_id = peer_id.to_string();
                            
                            if !result.bootstrap_initiated {
                                let message = result.message.unwrap();
                                let mut attachments: Vec<_> = message.attachments.iter().filter_map(|id| attachment_service.get(id).ok().flatten()).map(|a| attachment_to_display_data(&a)).collect();
                                // Pre-decode thumbnails before UI thread
                                for att in &mut attachments {
                                    att.decode_thumbnail();
                                }
                                let msg_sent_at = message.sent_at;
                                let timestamp_str = message.created_at.with_timezone(&chrono::Local).format("%H:%M").to_string();
                                let message_id_str = message.id.to_string();
                                let sent_text = text.clone();
                                let sender_name = current_user_name.clone();
                                let sender_initials = get_initials(&sender_name);

                                let weak_copy = weak.clone();
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(w) = weak_copy.upgrade() {
                                        let slint_attachments: Vec<_> = attachments.iter().map(|a| a.to_slint()).collect();
                                        let msg_item = MessageItemData {
                                            message_id: message_id_str.into(),
                                            sender_id: current_user_id.into(),
                                            sender_name: sender_name.into(),
                                            sender_initials: sender_initials.into(),
                                            sender_avatar_url: "".into(),
                                            content: sent_text.into(),
                                            timestamp: timestamp_str.clone().into(),
                                            timestamp_ns: msg_sent_at.to_string().into(),
                                            is_own: true,
                                            is_edited: false,
                                            is_deleted: false,
                                            is_system: false,
                                            is_ai: false,
                                            has_attachments: !slint_attachments.is_empty(),
                                            attachment_count: slint_attachments.len() as i32,
                                            attachments: ModelRc::new(VecModel::from(slint_attachments)),
                                            has_reactions: false,
                                            reaction_summary: "".into(),
                                            reply_count: 0,
                                            reply_count_text: "".into(),
                                            is_thread_reply: false,
                                            show_sender: true,
                                            delivery_status: "sent".into(),
                                            is_pinned: false,
                                            is_call: false,
                                            call_type: "".into(),
                                            call_duration: "".into(),
                                        };
                                        let model = w.get_messages();
                                        let mut messages: Vec<_> = (0..model.row_count()).filter_map(|i| model.row_data(i)).collect();
                                        messages.push(msg_item);
                                        messages.sort_by(|a, b| {
                                            let ts_a = a.timestamp_ns.parse::<u64>().unwrap_or(0);
                                            let ts_b = b.timestamp_ns.parse::<u64>().unwrap_or(0);
                                            ts_a.cmp(&ts_b).then_with(|| a.sender_id.as_str().cmp(b.sender_id.as_str()))
                                        });
                                        w.set_messages(ModelRc::new(VecModel::from(messages)));
                                        w.set_composer_text("".into());
                                        w.set_message_scroll_position(-99999.0);
                                        w.set_is_at_bottom(true);
                                        update_dm_sidebar_preview(&w, &sidebar_peer_id, &preview, &timestamp_str);
                                    }
                                });
                            } else {
                                // Bootstrap case - still add message to UI with "sending" status
                                let sent_at = result.sent_at;
                                let timestamp_str = chatter_linux::util::time::TimestampNanos::from_nanos(sent_at).to_datetime().unwrap_or_else(chrono::Utc::now).with_timezone(&chrono::Local).format("%H:%M").to_string();
                                let sent_text = text.clone();
                                let sender_name = current_user_name.clone();
                                let sender_initials = get_initials(&sender_name);
                                let weak_copy = weak.clone();
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(w) = weak_copy.upgrade() {
                                        // Add message to UI with "sending" status
                                        let msg_item = MessageItemData {
                                            message_id: format!("pending-{}", sent_at).into(),
                                            sender_id: current_user_id.into(),
                                            sender_name: sender_name.into(),
                                            sender_initials: sender_initials.into(),
                                            sender_avatar_url: "".into(),
                                            content: sent_text.into(),
                                            timestamp: timestamp_str.clone().into(),
                                            timestamp_ns: sent_at.to_string().into(),
                                            is_own: true,
                                            is_edited: false,
                                            is_deleted: false,
                                            is_system: false,
                                            is_ai: false,
                                            has_attachments: false,
                                            attachment_count: 0,
                                            attachments: ModelRc::default(),
                                            has_reactions: false,
                                            reaction_summary: "".into(),
                                            reply_count: 0,
                                            reply_count_text: "".into(),
                                            is_thread_reply: false,
                                            show_sender: true,
                                            delivery_status: "sending".into(),
                                            is_pinned: false,
                                            is_call: false,
                                            call_type: "".into(),
                                            call_duration: "".into(),
                                        };
                                        let model = w.get_messages();
                                        let mut messages: Vec<_> = (0..model.row_count()).filter_map(|i| model.row_data(i)).collect();
                                        messages.push(msg_item);
                                        messages.sort_by(|a, b| {
                                            let ts_a = a.timestamp_ns.parse::<u64>().unwrap_or(0);
                                            let ts_b = b.timestamp_ns.parse::<u64>().unwrap_or(0);
                                            ts_a.cmp(&ts_b).then_with(|| a.sender_id.as_str().cmp(b.sender_id.as_str()))
                                        });
                                        w.set_messages(ModelRc::new(VecModel::from(messages)));
                                        w.set_composer_text("".into());
                                        w.set_composer_placeholder("Setting up secure channel...".into());
                                        w.set_message_scroll_position(-99999.0);
                                        w.set_is_at_bottom(true);
                                        update_dm_sidebar_preview(&w, &sidebar_peer_id, &preview, &timestamp_str);
                                    }
                                });
                            }

                            // Register GIFs for progressive loading
                            for gif_id in gif_ids {
                                crate::bin_ui::gif_state::register_gif_attachment(gif_id.to_string(), attachment_service.clone(), weak.clone());
                            }
                        }
                        Err(e) => { tracing::error!("Failed to send DM: {}", e); }
                    }
                });
            }
        } else if !selected_private_group_id.is_empty() {
            tokio::spawn(async move {
                if !auth_state.is_logged_in() { return; }
                let group_id: chatter_linux::domain::anon_group::AnonGroupId = match selected_private_group_id.parse() { Ok(id) => id, _ => return };

                let current_user_id = auth_state.get().current_user_id.map(|id| id.to_string()).unwrap_or_default();
                let current_user_name = auth_state.get().display_name.unwrap_or_else(|| "Me".to_string());

                // Upload attachments first if any (skip if already uploaded with pointer)
                let mut attachment_pointers = Vec::new();
                let mut uploaded_attachments = Vec::new();
                for att in &pending_attachments {
                    // Check if attachment was pre-uploaded (e.g., GIF from picker)
                    if let Some(pointer) = &att.pointer {
                        attachment_pointers.push(pointer.clone());
                        if let Ok(Some(full_att)) = attachment_service.get(&att.id) {
                            uploaded_attachments.push(full_att);
                        }
                        continue;
                    }

                    match attachment_service.upload_to_cdn_encrypted(&grpc_pool, &att.path).await {
                        Ok(res) => {
                            attachment_pointers.push(res.to_attachment_pointer());
                            if let Ok(Some(full_att)) = attachment_service.get(&res.attachment_id) {
                                uploaded_attachments.push(full_att);
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to upload private group attachment: {}", e);
                        }
                    }
                }

                // Send with attachments if any, otherwise just text
                let send_result = if attachment_pointers.is_empty() {
                    anon_group_service.send_text(&group_id, &text).await
                } else {
                    anon_group_service.send_with_attachments(&group_id, &text, attachment_pointers).await
                };

                if let Ok(message_id) = send_result {
                    let now = chrono::Utc::now();
                    let timestamp_str = now.with_timezone(&chrono::Local).format("%H:%M").to_string();
                    let sent_at = now.timestamp_nanos_opt().unwrap_or(0) as u64;
                    let preview = if text.chars().count() > 50 { format!("You: {}...", text.chars().take(47).collect::<String>()) } else { format!("You: {}", text) };
                    let group_id_str = group_id.to_string();
                    let sender_initials = get_initials(&current_user_name);
                    let sent_text = text.clone();
                    let mut attachment_display_data: Vec<_> = uploaded_attachments
                        .iter()
                        .map(|a| attachment_to_display_data(a))
                        .collect();
                    // Pre-decode thumbnails before UI thread
                    for att in &mut attachment_display_data {
                        att.decode_thumbnail();
                    }

                    let attachment_service_for_gifs = attachment_service.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            let slint_attachments: Vec<_> = attachment_display_data
                                .iter()
                                .map(|a| a.to_slint())
                                .collect();

                            let msg_item = MessageItemData {
                                message_id: message_id.to_string().into(),
                                sender_id: current_user_id.into(),
                                sender_name: current_user_name.into(),
                                sender_initials: sender_initials.into(),
                                sender_avatar_url: "".into(),
                                content: sent_text.into(),
                                timestamp: timestamp_str.clone().into(),
                                timestamp_ns: sent_at.to_string().into(),
                                is_own: true,
                                is_edited: false,
                                is_deleted: false,
                                is_system: false,
                                is_ai: false,
                                has_attachments: !slint_attachments.is_empty(),
                                attachment_count: slint_attachments.len() as i32,
                                attachments: ModelRc::new(VecModel::from(slint_attachments)),
                                has_reactions: false,
                                reaction_summary: "".into(),
                                reply_count: 0,
                                reply_count_text: "".into(),
                                is_thread_reply: false,
                                show_sender: true,
                                delivery_status: "sent".into(),
                                is_pinned: false,
                                is_call: false,
                                call_type: "".into(),
                                call_duration: "".into(),
                            };
                            let model = w.get_messages();
                            let mut messages: Vec<_> = (0..model.row_count()).filter_map(|i| model.row_data(i)).collect();
                            messages.push(msg_item);
                            messages.sort_by(|a, b| {
                                let ts_a = a.timestamp_ns.parse::<u64>().unwrap_or(0);
                                let ts_b = b.timestamp_ns.parse::<u64>().unwrap_or(0);
                                ts_a.cmp(&ts_b).then_with(|| a.sender_id.as_str().cmp(b.sender_id.as_str()))
                            });
                            w.set_messages(ModelRc::new(VecModel::from(messages)));
                            w.set_composer_text("".into());
                            w.set_message_scroll_position(-99999.0);
                            w.set_is_at_bottom(true);
                            update_private_group_sidebar_preview(&w, &group_id_str, &preview, &timestamp_str);

                            // Auto-register GIF animations for sender's attachments
                            crate::bin_ui::gif_state::auto_register_gifs_from_messages(
                                w.as_weak(),
                                attachment_service_for_gifs,
                            );
                        }
                    });
                }
            });
        } else {
            tokio::spawn(async move {
                let channel_id = match chat_state.get().active_channel_id { Some(id) => id, None => return };
                if !auth_state.is_logged_in() { return; }
                let is_saved = match channel_service.get_channel_local(&channel_id) { Ok(Some(c)) => c.channel_type == ChannelType::SavedMessages, _ => false };

                // Get current user ID for is_own calculation
                let current_user_id = auth_state.get().current_user_id.map(|id| id.to_string()).unwrap_or_default();

                if is_saved {
                    let mut uploaded = Vec::new();
                    for att in &pending_attachments {
                        if let Some(pointer) = &att.pointer {
                            // Pre-uploaded attachment (e.g., GIF from picker)
                            let _ = pointer; // pointer already used in att.id
                            uploaded.push(att.id);
                        } else if let Ok(res) = attachment_service.upload_to_cdn_encrypted(&grpc_pool, &att.path).await {
                            uploaded.push(res.attachment_id);
                        }
                    }
                    if let Ok(message) = message_service.save_message(&text, uploaded).await {
                        let resolved: Vec<_> = message.attachments.iter().filter_map(|id| attachment_service.get(id).ok().flatten()).collect();
                        let mut display = message_to_display_data(&message, "Me", &resolved);
                        // Pre-decode attachment thumbnails before UI thread
                        display.decode_attachment_thumbnails();
                        let attachment_service_for_gifs = attachment_service.clone();
                        let _ = slint::invoke_from_event_loop(move || { if let Some(w) = weak.upgrade() {
                            let model = w.get_messages();
                            let mut messages: Vec<_> = (0..model.row_count()).filter_map(|i| model.row_data(i)).collect();
                            messages.push(display.to_slint_with_user(&current_user_id));
                            messages.sort_by(|a, b| {
                                            let ts_a = a.timestamp_ns.parse::<u64>().unwrap_or(0);
                                            let ts_b = b.timestamp_ns.parse::<u64>().unwrap_or(0);
                                            ts_a.cmp(&ts_b).then_with(|| a.sender_id.as_str().cmp(b.sender_id.as_str()))
                                        });
                            w.set_messages(ModelRc::new(VecModel::from(messages)));
                            w.set_composer_text("".into());
                            crate::bin_ui::gif_state::auto_register_gifs_from_messages(
                                w.as_weak(),
                                attachment_service_for_gifs,
                            );
                        }});
                    }
                } else {
                    let paths: Vec<_> = pending_attachments.iter().map(|a| a.path.as_path()).collect();
                    match message_service.send_with_files(group_fanout.as_deref(), &channel_id, &text, paths, None, vec![], false).await {
                        Ok(message) => {
                            let resolved: Vec<_> = message.attachments.iter().filter_map(|id| attachment_service.get(id).ok().flatten()).collect();
                            let mut display = message_to_display_data(&message, "Me", &resolved);
                            // Pre-decode attachment thumbnails before UI thread
                            display.decode_attachment_thumbnails();
                            let sidebar_channel_id = channel_id.to_string();
                            let preview = if text.chars().count() > 50 { format!("You: {}...", text.chars().take(47).collect::<String>()) } else { format!("You: {}", text) };
                            let ts = crate::bin_ui::features::auth::inbox::utils::format_timestamp(message.sent_at);
                            let attachment_service_for_gifs = attachment_service.clone();
                            let _ = slint::invoke_from_event_loop(move || { if let Some(w) = weak.upgrade() {
                                let model = w.get_messages();
                                let mut messages: Vec<_> = (0..model.row_count()).filter_map(|i| model.row_data(i)).collect();
                                messages.push(display.to_slint_with_user(&current_user_id));
                                messages.sort_by(|a, b| {
                                            let ts_a = a.timestamp_ns.parse::<u64>().unwrap_or(0);
                                            let ts_b = b.timestamp_ns.parse::<u64>().unwrap_or(0);
                                            ts_a.cmp(&ts_b).then_with(|| a.sender_id.as_str().cmp(b.sender_id.as_str()))
                                        });
                                w.set_messages(ModelRc::new(VecModel::from(messages)));
                                w.set_composer_text("".into());
                                update_channel_sidebar_preview(&w, &sidebar_channel_id, &preview, &ts);
                                crate::bin_ui::gif_state::auto_register_gifs_from_messages(
                                    w.as_weak(),
                                    attachment_service_for_gifs,
                                );
                            }});
                        }
                        Err(e) => { tracing::error!("Failed to send channel message: {}", e); }
                    }
                }
            });
        }
    });
}

fn update_dm_sidebar_preview(window: &MainWindow, peer_id_str: &str, preview: &str, timestamp_str: &str) {
    let dms = window.get_dm_conversations();
    for i in 0..dms.row_count() {
        if let Some(dm) = dms.row_data(i) {
            if dm.user_id.as_str() == peer_id_str {
                let mut updated = dm.clone();
                updated.last_message = preview.into();
                updated.timestamp = timestamp_str.into();
                if let Some(model) = dms.as_any().downcast_ref::<VecModel<DMListItemData>>() { model.set_row_data(i, updated); }
                return;
            }
        }
    }
}

fn update_private_group_sidebar_preview(window: &MainWindow, group_id_str: &str, preview: &str, timestamp_str: &str) {
    let groups = window.get_private_groups();
    for i in 0..groups.row_count() {
        if let Some(group) = groups.row_data(i) {
            if group.group_id.as_str() == group_id_str {
                let mut updated = group.clone();
                let truncated = if preview.chars().count() > 50 { format!("You: {}...", preview.chars().take(47).collect::<String>()) } else { format!("You: {}", preview) };
                updated.last_message = truncated.into();
                updated.timestamp = timestamp_str.into();
                if let Some(model) = groups.as_any().downcast_ref::<VecModel<PrivateGroupListItemData>>() { model.set_row_data(i, updated); }
                return;
            }
        }
    }
}

/// Handle Jarvis slash commands (/ask, /summarize, /knowledge, /remember).
async fn handle_jarvis_slash_command(
    ui_ctx: &Arc<UIContext>,
    cmd: crate::bin_ui::features::jarvis::JarvisCommand,
    channel_id: Option<&chatter_linux::domain::ids::ChannelId>,
    weak_window: slint::Weak<MainWindow>,
) {
    use crate::bin_ui::features::jarvis::JarvisCommand;
    use chatter_linux::domain::jarvis::JarvisContext;

    // Build context
    let context = match channel_id {
        Some(id) => JarvisContext::channel(id.clone()),
        None => {
            let placeholder_id: chatter_linux::domain::ids::ChannelId = uuid::Uuid::nil().into();
            JarvisContext::dm(placeholder_id)
        }
    };

    let author_id = ui_ctx.auth_state.get().current_user_id;

    // Set thinking state and add placeholder
    ui_ctx.jarvis_state.set_thinking(true);
    let thinking_id = {
        let weak = weak_window.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_jarvis_thinking(true);
                let id = crate::bin_ui::features::jarvis::add_jarvis_thinking_placeholder(&w);
                let _ = tx.send(id);
            }
        });
        rx.recv().ok()
    };

    // Execute command
    let (query_display, result) = match cmd {
        JarvisCommand::Ask(query) => {
            ui_ctx.jarvis_state.add_user_message(&query, channel_id.map(|id| id.to_string()).as_deref());
            let r = ui_ctx.jarvis_service.ask(&context, &query, author_id.as_ref()).await;
            (format!("/ask {}", query), r)
        }
        JarvisCommand::Summarize => {
            ui_ctx.jarvis_state.add_user_message("/summarize", channel_id.map(|id| id.to_string()).as_deref());
            let r = ui_ctx.jarvis_service.summarize_conversation(&context).await;
            ("/summarize".to_string(), r)
        }
        JarvisCommand::Knowledge(entity) => {
            ui_ctx.jarvis_state.add_user_message(&format!("/knowledge {}", entity), channel_id.map(|id| id.to_string()).as_deref());
            // Use ask with a knowledge-focused query
            let query = format!("What do you know about {}?", entity);
            let r = ui_ctx.jarvis_service.ask(&context, &query, author_id.as_ref()).await;
            (format!("/knowledge {}", entity), r)
        }
        JarvisCommand::Remember(fact) => {
            ui_ctx.jarvis_state.add_user_message(&format!("/remember {}", fact), channel_id.map(|id| id.to_string()).as_deref());
            // Store the fact via observe (treated as important message)
            let r = ui_ctx.jarvis_service.ask(&context, &format!("Please remember this fact: {}", fact), author_id.as_ref()).await;
            (format!("/remember {}", fact), r)
        }
    };

    // Handle result
    match result {
        Ok(response) => {
            ui_ctx.jarvis_state.add_assistant_message(&response);
            let response_clone = response.clone();
            let query_clone = query_display.clone();
            let thinking_id_clone = thinking_id.clone();
            let jarvis_state = ui_ctx.jarvis_state.clone();
            let _ = slint::invoke_from_event_loop(move || {
                jarvis_state.set_thinking(false);
                if let Some(w) = weak_window.upgrade() {
                    w.set_jarvis_thinking(false);
                    if let Some(tid) = thinking_id_clone {
                        crate::bin_ui::features::jarvis::replace_thinking_with_response(
                            &w, &tid, &query_clone, &response_clone,
                        );
                    }
                }
            });
        }
        Err(e) => {
            let error_msg = format!("Error: {}", e);
            ui_ctx.jarvis_state.add_assistant_message(&error_msg);
            let query_clone = query_display.clone();
            let thinking_id_clone = thinking_id.clone();
            let jarvis_state = ui_ctx.jarvis_state.clone();
            let _ = slint::invoke_from_event_loop(move || {
                jarvis_state.set_thinking(false);
                if let Some(w) = weak_window.upgrade() {
                    w.set_jarvis_thinking(false);
                    if let Some(tid) = thinking_id_clone {
                        crate::bin_ui::features::jarvis::replace_thinking_with_response(
                            &w, &tid, &query_clone, &error_msg,
                        );
                    }
                }
            });
        }
    }
}
