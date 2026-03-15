//! Send message callback for channels and DMs.
//!
//! All conversation types follow the same pipeline:
//! 1. Auth check + user info
//! 2. Upload attachments (all types pre-upload)
//! 3. Build quote context
//! 4. Build OutgoingIntent per conversation type
//! 5. Route through gateway
//! 6. Handle result + update UI

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, VecModel};

use chatter_linux::app::UIContext;
use chatter_linux::domain::channel::ChannelType;
use chatter_linux::services::gateway;
use chatter_linux::state::ActiveConversation;

use crate::bin_ui::converters::attachment_to_display_data;
use super::typing::clear_outgoing_typing;
use crate::{MainWindow, PendingAttachmentData};

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
                return;
            }
        }

        // Clear outgoing typing state (Signal: clearTypingTimers on message send)
        clear_outgoing_typing();
        if has_attachments { chat_state.clear_pending_attachments(); }

        // Track user activity for presence updates
        {
            let ui_ctx = ui_ctx_jarvis.clone();
            tokio::spawn(async move {
                crate::bin_ui::features::auth::presence_integration::track_activity(&ui_ctx).await;
            });
        }

        let message_service = message_service.clone();
        let auth_state = auth_state.clone();
        let grpc_pool = grpc_pool.clone();
        let mailbox_manager = mailbox_manager.clone();
        let attachment_service = attachment_service.clone();
        let anon_group_service = anon_group_service.clone();
        let anon_dm_service = anon_dm_service.clone();
        let channel_service = channel_service.clone();
        let group_fanout = group_fanout.clone();
        let weak = weak_window.clone();

        // Determine active conversation from chat state
        let conversation = chat_state.active_conversation();
        if matches!(conversation, ActiveConversation::None) { return; }

        // Get reply state from window
        let (reply_to_sender, reply_to_timestamp, reply_sender_name, reply_preview_text) = weak.upgrade().map(|w| {
            let is_replying = w.get_composer_is_replying();
            let reply_sender = if is_replying { w.get_composer_reply_to_sender().to_string() } else { String::new() };
            let reply_ts = if is_replying { w.get_composer_reply_to_timestamp().to_string() } else { String::new() };
            let reply_name = if is_replying { w.get_composer_reply_to_sender_name().to_string() } else { String::new() };
            let reply_preview = if is_replying { w.get_composer_reply_preview().to_string() } else { String::new() };
            (reply_sender, reply_ts, reply_name, reply_preview)
        }).unwrap_or_default();

        // Clear reply state immediately after capturing
        let is_reply = !reply_to_sender.is_empty() && !reply_to_timestamp.is_empty();
        if is_reply {
            let weak_clear_reply = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_clear_reply.upgrade() {
                    w.set_composer_is_replying(false);
                    w.set_composer_reply_to_sender("".into());
                    w.set_composer_reply_to_sender_name("".into());
                    w.set_composer_reply_to_timestamp("".into());
                    w.set_composer_reply_preview("".into());
                    w.set_composer_placeholder("Type a message...".into());
                }
            });
        }

        if has_attachments {
            let weak_clear = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_clear.upgrade() {
                    w.set_pending_attachments(ModelRc::new(VecModel::from(Vec::<PendingAttachmentData>::new())));
                    w.set_composer_placeholder("Type a message...".into());
                }
            });
        }

        // ================================================================
        // Unified send pipeline (one tokio::spawn for all types)
        // ================================================================
        tokio::spawn(async move {
            // 1. Auth check + user info (shared)
            if !auth_state.is_logged_in() { return; }
            let current_user_id = auth_state.get().current_user_id.map(|id| id.to_string()).unwrap_or_default();
            let current_user_name = auth_state.get().display_name.unwrap_or_else(|| "Me".to_string());

            // 2. Upload attachments (shared — all types pre-upload now)
            let uploaded = super::helpers::upload_pending_attachments(
                &pending_attachments, &attachment_service, &grpc_pool, "send",
            ).await;

            // 3. Build quote context (shared)
            let quote = if is_reply {
                let reply = match super::helpers::parse_reply_context(&reply_to_sender, &reply_to_timestamp) {
                    Some(r) => r,
                    None => return,
                };
                // For DM/Channel, look up preview text from DB; anon paths leave it None
                let preview_text = if !conversation.is_anon() {
                    message_service.get_by_sender_timestamp(&reply.sender_id, reply.timestamp)
                        .ok()
                        .flatten()
                        .map(|msg| gateway::shared::truncate_preview(&msg.as_text().unwrap_or_default()))
                } else {
                    None
                };
                Some(gateway::QuoteIntent {
                    author_id: reply.sender_id,
                    timestamp: reply.timestamp,
                    preview_text,
                })
            } else {
                None
            };

            // 4. Build services (shared — from_ctx fills all references)
            let services = gateway::OutgoingServices {
                message_service: &message_service,
                mailbox_manager: &mailbox_manager,
                user_service: None,
                group_fanout: group_fanout.as_deref(),
                anon_dm_service: Some(&anon_dm_service),
                anon_group_service: Some(&anon_group_service),
            };

            // 5. Build intent (per-type — thin match)
            let intent = match conversation {
                ActiveConversation::AnonDm(id) => gateway::OutgoingIntent::AnonDm {
                    conversation_id: id,
                    text: text.clone(),
                    attachment_pointers: uploaded.pointers.clone(),
                    quote,
                },
                ActiveConversation::AnonGroup(id) => gateway::OutgoingIntent::AnonGroup {
                    group_id: id,
                    text: text.clone(),
                    attachment_pointers: uploaded.pointers.clone(),
                    quote,
                },
                ActiveConversation::Dm { channel_id, peer_id } => gateway::OutgoingIntent::ChannelDm {
                    peer_id,
                    dm_channel_id: channel_id,
                    text: text.clone(),
                    attachment_pointers: uploaded.pointers.clone(),
                    file_paths: vec![],
                    quote,
                },
                ActiveConversation::Channel(id) => {
                    // SavedMessages goes through its own intent
                    let is_saved = channel_service.get_channel_local(&id)
                        .ok().flatten()
                        .map(|c| c.channel_type == ChannelType::SavedMessages)
                        .unwrap_or(false);

                    if is_saved {
                        let att_ids = uploaded.pointers.iter()
                            .map(|p| chatter_linux::domain::AttachmentId::from_cdn_key(&p.cdn_key))
                            .collect();
                        gateway::OutgoingIntent::SavedMessages {
                            text: text.clone(),
                            attachment_ids: att_ids,
                        }
                    } else {
                        gateway::OutgoingIntent::Channel {
                            channel_id: id,
                            text: text.clone(),
                            attachment_pointers: uploaded.pointers.clone(),
                            file_paths: vec![],
                            thread_id: None,
                            mentioned_integrations: vec![],
                            targets_ai_bot: false,
                        }
                    }
                }
                ActiveConversation::None => return,
            };

            // 6. Route through gateway (shared)
            let result = match gateway::route_outgoing(intent, &services).await {
                Ok(r) => r,
                Err(e) => {
                    gateway::shared::handle_send_error(&e, "send");
                    return;
                }
            };

            // 7. Handle result + update UI (fully unified — no per-type branches)
            let sent_at = result.sent_at;
            let message_id_str = result.message_id_string();
            let timestamp_str = chrono::DateTime::from_timestamp_nanos(sent_at as i64)
                .with_timezone(&chrono::Local).format("%H:%M").to_string();

            // Sidebar preview (shared)
            let preview = if text.chars().count() > 50 {
                format!("You: {}...", text.chars().take(47).collect::<String>())
            } else {
                format!("You: {}", text)
            };

            // DM bootstrap: first message to new peer, key exchange in progress.
            // Show "sending" placeholder — the message will be delivered when bootstrap completes.
            let delivery_status = if result.bootstrap_initiated { "sending" } else { "sent" };
            let msg_id = if result.bootstrap_initiated {
                format!("pending-{}", sent_at)
            } else {
                message_id_str
            };

            // Attachments: always use pre-uploaded data (available for all types)
            let mut attachment_display: Vec<_> = uploaded.attachments.iter()
                .map(|a| attachment_to_display_data(a))
                .collect();
            // Bootstrap sends have no attachments yet (key exchange must complete first)
            if result.bootstrap_initiated {
                attachment_display.clear();
            }
            for att in &mut attachment_display {
                att.decode_thumbnail();
            }

            let attachment_service_for_gifs = attachment_service.clone();
            let sent_text = text.clone();
            let ts_for_sidebar = timestamp_str.clone();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    let slint_attachments: Vec<_> = attachment_display.iter().map(|a| a.to_slint()).collect();
                    let msg_item = super::helpers::build_own_message_item(
                        &msg_id, &current_user_id, &current_user_name, &sent_text,
                        &timestamp_str, sent_at, slint_attachments,
                        is_reply, &reply_sender_name, &reply_preview_text, delivery_status,
                    );
                    super::helpers::append_message_to_ui(&w, msg_item);

                    if result.bootstrap_initiated {
                        w.set_composer_placeholder("Setting up secure channel...".into());
                    }

                    crate::bin_ui::features::sidebar::update_sidebar_preview(
                        &w, conversation, &preview, &ts_for_sidebar, false,
                    );

                    crate::bin_ui::gif_state::auto_register_gifs_from_messages(
                        w.as_weak(),
                        attachment_service_for_gifs,
                    );
                }
            });
        });
    });
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
            let query = format!("What do you know about {}?", entity);
            let r = ui_ctx.jarvis_service.ask(&context, &query, author_id.as_ref()).await;
            (format!("/knowledge {}", entity), r)
        }
        JarvisCommand::Remember(fact) => {
            ui_ctx.jarvis_state.add_user_message(&format!("/remember {}", fact), channel_id.map(|id| id.to_string()).as_deref());
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
