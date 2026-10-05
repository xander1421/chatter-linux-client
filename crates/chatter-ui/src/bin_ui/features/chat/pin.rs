//! Pin message callback for DM and channel conversations.

use std::sync::Arc;

use slint::{ComponentHandle, Model, SharedString};

use chatter_core::client::ChatterClient;
use chatter_core::domain::{Attachment, FileCategory, Message};
use chatter_core::state::ActiveConversation;

use crate::{MainWindow, MessageItemData};

/// Generate a preview text for a pinned message.
fn generate_pinned_preview(message: &Message, attachments: &[Attachment]) -> String {
    if let Some(text) = message.as_text() {
        if !text.is_empty() {
            return chatter_core::services::gateway::shared::truncate_preview(text);
        }
    }
    if !attachments.is_empty() {
        if attachments.len() == 1 {
            let cat = attachments[0].metadata.category();
            let emoji = match cat {
                FileCategory::Image => "📷",
                FileCategory::Video => "🎬",
                FileCategory::Audio => "🎵",
                FileCategory::Document => "📄",
                FileCategory::Code => "📝",
                FileCategory::Archive => "📦",
                FileCategory::Other => "📎",
            };
            let name = match cat {
                FileCategory::Image => "Photo",
                FileCategory::Video => "Video",
                FileCategory::Audio => "Voice message",
                FileCategory::Document => "Document",
                FileCategory::Code => "Code",
                FileCategory::Archive => "Archive",
                FileCategory::Other => "File",
            };
            return format!("{} {}", emoji, name);
        }
        return format!("📎 {} attachments", attachments.len());
    }
    if !message.attachments.is_empty() {
        if message.attachments.len() == 1 {
            return "📎 Attachment".to_string();
        }
        return format!("📎 {} attachments", message.attachments.len());
    }
    String::new()
}

fn resolve_attachments(message: &Message, ui_ctx: &ChatterClient) -> Vec<Attachment> {
    message
        .attachments
        .iter()
        .filter_map(|id| ui_ctx.get_attachment(id))
        .collect()
}

/// Generate preview from Slint UI message model (used for anon DM/group sender-side).
fn generate_pinned_preview_from_ui(msg: &MessageItemData) -> String {
    crate::bin_ui::features::auth::inbox::utils::generate_preview_from_ui_message(msg)
}

pub fn wire_pin_message(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();

    window.on_pin_message(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();

        let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
            Ok(ts) => ts,
            Err(_) => return,
        };

        let conversation = ui_ctx.chat_state().active_conversation();
        if matches!(conversation, ActiveConversation::None) {
            return;
        }

        // Get pin state and preview on UI thread BEFORE spawning
        let (is_currently_pinned, preview_text, msg_id_str) = weak_window
            .upgrade()
            .map(|w| {
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
            })
            .unwrap_or((false, String::new(), String::new()));

        let unpin = is_currently_pinned;
        let handle = ui_ctx.conversation(conversation);
        let ui_ctx = ui_ctx.clone();
        let weak = weak_window.clone();

        tokio::spawn(async move {
            let target_author: chatter_core::domain::UserId = match sender_id_str.parse() {
                Ok(id) => id,
                Err(_) => {
                    tracing::error!(sender_id = %sender_id_str, "invalid sender id for pin");
                    return;
                }
            };
            // Send pin/unpin via ConversationHandle (dispatches to correct service)
            if let Err(e) = handle
                .send_pin(&target_author, target_sent_timestamp, unpin)
                .await
            {
                tracing::error!(error = %e, "Failed to send pin");
                return;
            }

            // Persistence is handled by ConversationHandle::send_pin()

            let new_pinned_state = !unpin;

            // Update pinned-bar substate
            if let Ok(msg_id) = msg_id_str.parse::<chatter_core::domain::ids::MessageId>() {
                if new_pinned_state {
                    ui_ctx.chat_state().add_pinned_message(msg_id);
                } else {
                    ui_ctx.chat_state().remove_pinned_message(&msg_id);
                }
            }
            let pinned_count = ui_ctx.chat_state().pinned_total_count();

            // For DM/Channel, generate preview from stored message + attachments
            let preview_text = match handle.conversation() {
                ActiveConversation::ServerDm { .. } | ActiveConversation::Channel(_) => {
                    if let Ok(sender_id) = sender_id_str.parse::<chatter_core::domain::UserId>() {
                        if let Some(message) = ui_ctx
                            .get_message_by_sender_timestamp(&sender_id, target_sent_timestamp)
                        {
                            let attachments = resolve_attachments(&message, &ui_ctx);
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
                    super::edit_messages(&w, |model| {
                        for i in 0..model.row_count() {
                            let Some(mut m) = model.row_data(i) else {
                                continue;
                            };
                            if m.sender_id.as_str() == sender_id_str.as_str()
                                && m.timestamp_ns.as_str() == timestamp_ns_str.as_str()
                            {
                                m.is_pinned = new_pinned_state;
                                model.set_row_data(i, m);
                            }
                        }
                    });
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
