//! Pin message callback for DM and channel conversations.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_linux::app::UIContext;
use chatter_linux::domain::{Attachment, FileCategory, Message};
use chatter_linux::services::attachment::AttachmentService;
use chatter_linux::services::conversation::ConversationHandle;
use chatter_linux::state::ActiveConversation;

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
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();

    window.on_pin_message(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();

        let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
            Ok(ts) => ts,
            Err(_) => return,
        };

        let conversation = ui_ctx.chat_state.active_conversation();
        if matches!(conversation, ActiveConversation::None) {
            return;
        }

        // Get pin state and preview on UI thread BEFORE spawning
        let (is_currently_pinned, preview_text, msg_id_str) = weak_window.upgrade().map(|w| {
            let model = w.get_messages();
            for i in 0..model.row_count() {
                if let Some(m) = model.row_data(i) {
                    if m.sender_id.as_str() == sender_id_str.as_str()
                        && m.timestamp_ns.as_str() == timestamp_ns_str.as_str()
                    {
                        let preview = generate_pinned_preview_from_ui(&m);
                        return (m.is_pinned, preview, m.message_id.to_string());
                    }
                }
            }
            (false, String::new(), String::new())
        }).unwrap_or((false, String::new(), String::new()));

        let unpin = is_currently_pinned;
        let handle = ConversationHandle::new(conversation.clone(), ui_ctx.clone());
        let ui_ctx = ui_ctx.clone();
        let weak = weak_window.clone();

        tokio::spawn(async move {
            // Send pin/unpin via ConversationHandle (dispatches to correct service)
            if let Err(e) = handle.send_pin(
                &sender_id_str, target_sent_timestamp, unpin,
            ).await {
                tracing::error!(error = %e, "Failed to send pin");
                return;
            }

            let new_pinned_state = !unpin;

            // Persist pin state based on conversation type
            match handle.conversation() {
                ActiveConversation::AnonDm(id) => {
                    if let Err(e) = ui_ctx.anon_dm_service.set_message_pinned(id, target_sent_timestamp, new_pinned_state) {
                        tracing::error!(error = %e, "Failed to persist anon DM pin state");
                    }
                }
                ActiveConversation::AnonGroup(id) => {
                    if let Err(e) = ui_ctx.anon_group_service.set_message_pinned(id, target_sent_timestamp, new_pinned_state) {
                        tracing::error!(error = %e, "Failed to persist anon group pin state");
                    }
                }
                ActiveConversation::Dm { .. } | ActiveConversation::Channel(_) => {
                    // For DM/Channel, look up the message and update is_pinned
                    if let Ok(sender_id) = sender_id_str.parse::<chatter_linux::domain::UserId>() {
                        if let Ok(Some(message)) = ui_ctx.message_service.get_by_sender_timestamp(&sender_id, target_sent_timestamp) {
                            let mut updated = message.clone();
                            updated.is_pinned = new_pinned_state;
                            let _ = ui_ctx.message_service.update_message(&updated);
                        }
                    }
                }
                ActiveConversation::None => {}
            }

            // Update pinned_state
            if let Ok(msg_id) = msg_id_str.parse::<chatter_linux::domain::ids::MessageId>() {
                if new_pinned_state {
                    ui_ctx.pinned_state.add_pinned_message(msg_id);
                } else {
                    ui_ctx.pinned_state.remove_pinned_message(&msg_id);
                }
            }
            let pinned_count = ui_ctx.pinned_state.get().pinned_message_ids.len();

            // For DM/Channel, generate preview from stored message + attachments
            let preview_text = match handle.conversation() {
                ActiveConversation::Dm { .. } | ActiveConversation::Channel(_) => {
                    if let Ok(sender_id) = sender_id_str.parse::<chatter_linux::domain::UserId>() {
                        if let Ok(Some(message)) = ui_ctx.message_service.get_by_sender_timestamp(&sender_id, target_sent_timestamp) {
                            let attachments = resolve_attachments(&message, &ui_ctx.attachment_service);
                            generate_pinned_preview(&message, &attachments)
                        } else {
                            preview_text
                        }
                    } else {
                        preview_text
                    }
                }
                _ => preview_text,
            };

            // Update UI (unified across all conversation types)
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    let model = w.get_messages();
                    let messages: Vec<_> = (0..model.row_count())
                        .filter_map(|i| model.row_data(i))
                        .map(|mut m| {
                            if m.sender_id.as_str() == sender_id_str.as_str()
                                && m.timestamp_ns.as_str() == timestamp_ns_str.as_str()
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
                }
            });
        });
    });
}
