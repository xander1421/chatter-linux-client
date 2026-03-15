//! Pin message callback for DM and channel conversations.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_linux::app::UIContext;
use chatter_linux::domain::{Attachment, FileCategory, Message, UserId};
use chatter_linux::services::attachment::AttachmentService;
use chatter_linux::services::{build_pin_content, build_pin_content_with_channel};
use chatter_linux::state::ActiveConversation;

use super::helpers::{send_channel_content, send_dm_content};
use crate::{MainWindow, MessageItemData};

/// Generate a preview text for a pinned message.
fn generate_pinned_preview(message: &Message, attachments: &[Attachment]) -> String {
    if let Some(text) = message.as_text() {
        if !text.is_empty() {
            return if text.len() > 80 { format!("{}...", &text[..77]) } else { text.to_string() };
        }
    }
    if !attachments.is_empty() {
        if attachments.len() == 1 {
            let cat = attachments[0].metadata.category();
            let emoji = match cat { FileCategory::Image => "📷", FileCategory::Video => "🎬", FileCategory::Audio => "🎵", FileCategory::Document => "📄", FileCategory::Code => "📝", FileCategory::Archive => "📦", FileCategory::Other => "📎" };
            let name = match cat { FileCategory::Image => "Photo", FileCategory::Video => "Video", FileCategory::Audio => "Voice message", FileCategory::Document => "Document", FileCategory::Code => "Code", FileCategory::Archive => "Archive", FileCategory::Other => "File" };
            return format!("{} {}", emoji, name);
        }
        return format!("📎 {} attachments", attachments.len());
    }
    if !message.attachments.is_empty() {
        if message.attachments.len() == 1 { return "📎 Attachment".to_string(); }
        return format!("📎 {} attachments", message.attachments.len());
    }
    String::new()
}

fn resolve_attachments(message: &Message, attachment_service: &AttachmentService) -> Vec<Attachment> {
    message.attachments.iter().filter_map(|id| attachment_service.get(id).ok().flatten()).collect()
}

/// Generate preview from Slint UI message model (used for anon DM/group sender-side).
fn generate_pinned_preview_from_ui(msg: &MessageItemData) -> String {
    crate::bin_ui::features::auth::inbox::utils::generate_preview_from_ui_message(msg)
}

pub fn wire_pin_message(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let auth_state_global = ui_ctx.auth_state.clone();
    let mailbox_manager_global = ui_ctx.mailbox_manager.clone();
    let message_service_global = ui_ctx.message_service.clone();
    let attachment_service_global = ui_ctx.attachment_service.clone();
    let pinned_state_global = ui_ctx.pinned_state.clone();
    let group_coordinator_global = ui_ctx.group_coordinator.clone();
    let chat_state_global = ui_ctx.chat_state.clone();
    let anon_dm_service_global = ui_ctx.anon_dm_service.clone();
    let anon_group_service_global = ui_ctx.anon_group_service.clone();
    let weak_window = window.as_weak();

    window.on_pin_message(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let auth_state = auth_state_global.clone();
        let mailbox_manager = mailbox_manager_global.clone();
        let message_service = message_service_global.clone();
        let attachment_service = attachment_service_global.clone();
        let pinned_state = pinned_state_global.clone();
        let group_coordinator = group_coordinator_global.clone();
        let anon_dm_service = anon_dm_service_global.clone();
        let anon_group_service = anon_group_service_global.clone();

        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();

        let conversation = chat_state_global.active_conversation();
        let weak = weak_window.clone();

        match conversation {
            ActiveConversation::AnonDm(anon_dm_id) => {
                let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                    Ok(ts) => ts,
                    Err(_) => return,
                };

                let service = anon_dm_service.clone();
                let sender_for_ui = sender_id_str.clone();
                let timestamp_for_ui = timestamp_ns_str.clone();

                // Get pin state and preview on UI thread BEFORE spawning
                let (is_currently_pinned, preview_text, msg_id_str) = weak.upgrade().map(|w| {
                    let model = w.get_messages();
                    for i in 0..model.row_count() {
                        if let Some(m) = model.row_data(i) {
                            if m.sender_id.as_str() == sender_for_ui.as_str()
                                && m.timestamp_ns.as_str() == timestamp_for_ui.as_str()
                            {
                                let preview = generate_pinned_preview_from_ui(&m);
                                return (m.is_pinned, preview, m.message_id.to_string());
                            }
                        }
                    }
                    (false, String::new(), String::new())
                }).unwrap_or((false, String::new(), String::new()));

                let unpin = is_currently_pinned;
                let pinned_state = pinned_state_global.clone();

                tokio::spawn(async move {

                    // Send pin/unpin to peer
                    if let Err(e) = service.send_pin(
                        &anon_dm_id,
                        &sender_for_ui,
                        target_sent_timestamp,
                        unpin,
                    ).await {
                        tracing::error!(error = %e, "Failed to send anon DM pin");
                        return;
                    }

                    let new_pinned_state = !unpin;

                    // Persist pin state to DB
                    if let Err(e) = service.set_message_pinned(&anon_dm_id, target_sent_timestamp, new_pinned_state) {
                        tracing::error!(error = %e, "Failed to persist anon DM pin state");
                    }

                    // Update pinned_state
                    if let Ok(msg_id) = msg_id_str.parse::<chatter_linux::domain::ids::MessageId>() {
                        if new_pinned_state {
                            pinned_state.add_pinned_message(msg_id);
                        } else {
                            pinned_state.remove_pinned_message(&msg_id);
                        }
                    }
                    let pinned_count = pinned_state.get().pinned_message_ids.len();

                    // Update UI
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            let model = w.get_messages();
                            let messages: Vec<_> = (0..model.row_count())
                                .filter_map(|i| model.row_data(i))
                                .map(|mut m| {
                                    if m.sender_id.as_str() == sender_for_ui.as_str()
                                        && m.timestamp_ns.as_str() == timestamp_for_ui.as_str()
                                    {
                                        m.is_pinned = new_pinned_state;
                                    }
                                    m
                                })
                                .collect();
                            w.set_messages(ModelRc::new(VecModel::from(messages)));
                            w.set_has_pinned_messages(pinned_count > 0);
                            if new_pinned_state {
                                w.set_pinned_message_preview(preview_text.into());
                                w.set_pinned_current_index(pinned_count as i32);
                                w.set_pinned_total_count(pinned_count as i32);
                            } else {
                                w.set_pinned_total_count(pinned_count as i32);
                                if pinned_count == 0 {
                                    w.set_pinned_message_preview("".into());
                                }
                            }

                            tracing::info!(
                                anon_dm = %anon_dm_id,
                                timestamp = %timestamp_for_ui,
                                pinned = new_pinned_state,
                                "Updated anon DM message pin state"
                            );
                        }
                    });
                });
            }

            ActiveConversation::AnonGroup(anon_group_id) => {
                let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                    Ok(ts) => ts,
                    Err(_) => return,
                };

                let service = anon_group_service.clone();
                let sender_for_ui = sender_id_str.clone();
                let timestamp_for_ui = timestamp_ns_str.clone();

                // Get pin state and preview on UI thread BEFORE spawning
                let (is_currently_pinned, preview_text, msg_id_str) = weak.upgrade().map(|w| {
                    let model = w.get_messages();
                    for i in 0..model.row_count() {
                        if let Some(m) = model.row_data(i) {
                            if m.sender_id.as_str() == sender_for_ui.as_str()
                                && m.timestamp_ns.as_str() == timestamp_for_ui.as_str()
                            {
                                let preview = generate_pinned_preview_from_ui(&m);
                                return (m.is_pinned, preview, m.message_id.to_string());
                            }
                        }
                    }
                    (false, String::new(), String::new())
                }).unwrap_or((false, String::new(), String::new()));

                let unpin = is_currently_pinned;
                let pinned_state = pinned_state_global.clone();

                tokio::spawn(async move {

                    // Send pin/unpin to group
                    if let Err(e) = service.send_pin(
                        &anon_group_id,
                        &sender_for_ui,
                        target_sent_timestamp,
                        unpin,
                    ).await {
                        tracing::error!(error = %e, "Failed to send anon group pin");
                        return;
                    }

                    let new_pinned_state = !unpin;

                    // Persist pin state to DB
                    if let Err(e) = service.set_message_pinned(&anon_group_id, target_sent_timestamp, new_pinned_state) {
                        tracing::error!(error = %e, "Failed to persist anon group pin state");
                    }

                    // Update pinned_state
                    if let Ok(msg_id) = msg_id_str.parse::<chatter_linux::domain::ids::MessageId>() {
                        if new_pinned_state {
                            pinned_state.add_pinned_message(msg_id);
                        } else {
                            pinned_state.remove_pinned_message(&msg_id);
                        }
                    }
                    let pinned_count = pinned_state.get().pinned_message_ids.len();

                    // Update UI
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            let model = w.get_messages();
                            let messages: Vec<_> = (0..model.row_count())
                                .filter_map(|i| model.row_data(i))
                                .map(|mut m| {
                                    if m.sender_id.as_str() == sender_for_ui.as_str()
                                        && m.timestamp_ns.as_str() == timestamp_for_ui.as_str()
                                    {
                                        m.is_pinned = new_pinned_state;
                                    }
                                    m
                                })
                                .collect();
                            w.set_messages(ModelRc::new(VecModel::from(messages)));
                            w.set_has_pinned_messages(pinned_count > 0);
                            if new_pinned_state {
                                w.set_pinned_message_preview(preview_text.into());
                                w.set_pinned_current_index(pinned_count as i32);
                                w.set_pinned_total_count(pinned_count as i32);
                            } else {
                                w.set_pinned_total_count(pinned_count as i32);
                                if pinned_count == 0 {
                                    w.set_pinned_message_preview("".into());
                                }
                            }

                            tracing::info!(
                                anon_group = %anon_group_id,
                                timestamp = %timestamp_for_ui,
                                pinned = new_pinned_state,
                                "Updated anon group message pin state"
                            );
                        }
                    });
                });
            }

            ActiveConversation::Dm { peer_id, .. } => {
                tokio::spawn(async move {
                    if !auth_state.is_logged_in() { return; }
                    let target_sender_id: UserId = match sender_id_str.parse() { Ok(id) => id, _ => return };
                    let target_sent_timestamp: u64 = match timestamp_ns_str.parse() { Ok(ts) => ts, _ => return };

                    if let Ok(Some(message)) = message_service.get_by_sender_timestamp(&target_sender_id, target_sent_timestamp) {
                        let msg_id = message.id;
                        let target_author_aci = message.sender_id.map(|id| id.to_string()).unwrap_or_default();
                        let unpin = message.is_pinned;
                        let content = build_pin_content(&target_author_aci, target_sent_timestamp, unpin);

                        if let Err(e) = send_dm_content(content, &peer_id, message_service.clone(), mailbox_manager).await {
                            tracing::error!(error = %e, "Failed to send DM pin");
                            return;
                        }

                        let new_pinned_state = !unpin;
                        let mut updated_message = message.clone();
                        updated_message.is_pinned = new_pinned_state;
                        let _ = message_service.update_message(&updated_message);

                        if new_pinned_state { pinned_state.add_pinned_message(msg_id); } else { pinned_state.remove_pinned_message(&msg_id); }
                        let pinned_count = pinned_state.get().pinned_message_ids.len();
                        let attachments = resolve_attachments(&message, &attachment_service);
                        let preview_text = generate_pinned_preview(&message, &attachments);

                        let sender_id_for_ui = sender_id_str.clone();
                        let timestamp_for_ui = timestamp_ns_str.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                let model = w.get_messages();
                                let messages: Vec<_> = (0..model.row_count()).filter_map(|i| model.row_data(i)).map(|mut m| {
                                    if m.sender_id.as_str() == sender_id_for_ui.as_str() && m.timestamp_ns.as_str() == timestamp_for_ui.as_str() { m.is_pinned = new_pinned_state; }
                                    m
                                }).collect();
                                w.set_messages(ModelRc::new(VecModel::from(messages)));
                                w.set_has_pinned_messages(pinned_count > 0);
                                if new_pinned_state {
                                    w.set_pinned_message_preview(preview_text.into());
                                    w.set_pinned_current_index(pinned_count as i32);
                                    w.set_pinned_total_count(pinned_count as i32);
                                }
                            }
                        });
                    }
                });
            }

            ActiveConversation::Channel(channel_id) => {
                tokio::spawn(async move {
                    if !auth_state.is_logged_in() { return; }
                    let target_sender_id: UserId = match sender_id_str.parse() { Ok(id) => id, _ => return };
                    let target_sent_timestamp: u64 = match timestamp_ns_str.parse() { Ok(ts) => ts, _ => return };

                    if let Ok(Some(message)) = message_service.get_by_sender_timestamp(&target_sender_id, target_sent_timestamp) {
                        let msg_id = message.id;
                        let target_author_aci = message.sender_id.map(|id| id.to_string()).unwrap_or_default();
                        let unpin = message.is_pinned;
                        let channel_id_str = channel_id.to_string();
                        let content = build_pin_content_with_channel(&target_author_aci, target_sent_timestamp, unpin, Some(&channel_id_str));

                        if let Err(e) = send_channel_content(content, &channel_id, message_service.clone(), group_coordinator).await {
                            tracing::error!(error = %e, "Failed to send channel pin");
                            return;
                        }

                        let new_pinned_state = !unpin;
                        let mut updated_message = message.clone();
                        updated_message.is_pinned = new_pinned_state;
                        let _ = message_service.update_message(&updated_message);

                        if new_pinned_state { pinned_state.add_pinned_message(msg_id); } else { pinned_state.remove_pinned_message(&msg_id); }
                        let pinned_count = pinned_state.get().pinned_message_ids.len();
                        let attachments = resolve_attachments(&message, &attachment_service);
                        let preview_text = generate_pinned_preview(&message, &attachments);

                        let sender_id_for_ui = sender_id_str.clone();
                        let timestamp_for_ui = timestamp_ns_str.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                let model = w.get_messages();
                                let messages: Vec<_> = (0..model.row_count()).filter_map(|i| model.row_data(i)).map(|mut m| {
                                    if m.sender_id.as_str() == sender_id_for_ui.as_str() && m.timestamp_ns.as_str() == timestamp_for_ui.as_str() { m.is_pinned = new_pinned_state; }
                                    m
                                }).collect();
                                w.set_messages(ModelRc::new(VecModel::from(messages)));
                                w.set_has_pinned_messages(pinned_count > 0);
                                if new_pinned_state {
                                    w.set_pinned_message_preview(preview_text.into());
                                    w.set_pinned_current_index(pinned_count as i32);
                                    w.set_pinned_total_count(pinned_count as i32);
                                }
                            }
                        });
                    }
                });
            }

            ActiveConversation::None => return,
        }
    });
}
