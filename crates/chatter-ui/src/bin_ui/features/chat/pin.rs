//! Pin message callback for DM and channel conversations.

use std::sync::Arc;

use slint::{ComponentHandle, Model, SharedString};

use chatter_core::client::ChatterClient;
use chatter_core::domain::{Attachment, FileCategory, Message, SharedCategory};
use chatter_core::state::ActiveConversation;

use crate::{MainWindow, MessageItemData};

/// Generate a preview text for a pinned message.
fn generate_pinned_preview(message: &Message, attachments: &[Attachment]) -> String {
    if let Some(text) = message.as_text() {
        if !text.is_empty() {
            return crate::bin_ui::converters::pin_preview::text_preview(text);
        }
    }
    if !attachments.is_empty() {
        if attachments.len() == 1 {
            let meta = &attachments[0].metadata;
            let cat = meta.category();
            // Only an in-app recording is a voice note; picked audio is music.
            let voice = SharedCategory::of(meta) == SharedCategory::Voice;
            let emoji = match cat {
                FileCategory::Image => "📷",
                FileCategory::Video => "🎬",
                FileCategory::Audio if voice => "🎤",
                FileCategory::Audio => "🎵",
                FileCategory::Document => "📄",
                FileCategory::Code => "📝",
                FileCategory::Archive => "📦",
                FileCategory::Other => "📎",
            };
            let name = match cat {
                FileCategory::Image => "Photo",
                FileCategory::Video => "Video",
                FileCategory::Audio if voice => "Voice message",
                FileCategory::Audio => "Music",
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

    window.global::<crate::ChatPane>().on_pin_message(
        move |sender_id: SharedString, timestamp_ns: SharedString| {
            let sender_id_str = sender_id.to_string();
            let timestamp_ns_str = timestamp_ns.to_string();

            let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                Ok(ts) => ts,
                Err(_) => return,
            };

            let conversation = ui_ctx.active_conversation();
            if matches!(conversation, ActiveConversation::None) {
                return;
            }

            // Get pin state and preview on UI thread BEFORE spawning
            let (is_currently_pinned, preview_text, msg_id_str) = weak_window
                .upgrade()
                .map(|w| {
                    let model = w.global::<crate::ChatPane>().get_messages();
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
            // The pin is for this session, conversation and history epoch: a
            // send that finishes after a sign-out, another conversation opened
            // or a reload must move neither the pinned substate nor the rows
            // (`HistoryOwner`, checked before every current-state change and
            // again inside the UI closure).
            let owner = crate::bin_ui::features::history_owner::HistoryOwner::capture(
                &ui_ctx,
                conversation,
            );
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
                // The storage kind the menu was opened against; the publication
                // below requires the open view to still resolve to it.
                let Ok(kind) = handle.storage_kind() else {
                    return;
                };
                if !owner.is_current() {
                    return;
                }
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

                // For DM/Channel, generate preview from stored message + attachments.
                // A Saved row keeps the UI model's preview: the lookup below is
                // keyed by author and clock alone, not by the Saved scope.
                let preview_text = match handle.conversation() {
                    ActiveConversation::ServerDm { .. } | ActiveConversation::Channel(_)
                        if kind != chatter_core::domain::ConversationType::SavedMessages =>
                    {
                        if let Some(message) = ui_ctx
                            .get_message_by_sender_timestamp(&target_author, target_sent_timestamp)
                        {
                            let attachments = resolve_attachments(&message, &ui_ctx);
                            generate_pinned_preview(&message, &attachments)
                        } else {
                            preview_text
                        }
                    }
                    _ => preview_text,
                };

                // Update UI (unified across all conversation types). The pinned
                // substate is moved here, on the UI thread, for the view that is
                // still current and still stored under the menu's kind. A Saved
                // pin is durable before this runs, so the stored row as of now
                // decides the flag and the preview — not the tap's intent — and
                // it must be the tapped row (real id) at this account's Saved
                // key: foreign, deleted or missing paints nothing.
                crate::run_history_ui!(weak, owner, |w| {
                    let handle = ui_ctx.conversation(conversation);
                    if handle.storage_kind().ok() != Some(kind.clone()) {
                        return;
                    }
                    let saved = kind == chatter_core::domain::ConversationType::SavedMessages;
                    let (new_pinned_state, preview_text) = if saved {
                        let row = match handle
                            .stored_message_scoped(&target_author, target_sent_timestamp)
                        {
                            Ok(row) if row.id.to_string() == msg_id_str => row,
                            _ => return,
                        };
                        let preview = if row.is_pinned {
                            ui_ctx.pinned_message_preview(&conversation, &row.id)
                        } else {
                            String::new()
                        };
                        (row.is_pinned, preview)
                    } else {
                        (new_pinned_state, preview_text)
                    };
                    if let Ok(msg_id) = msg_id_str.parse::<chatter_core::domain::ids::MessageId>() {
                        if new_pinned_state {
                            ui_ctx.add_pinned_message(msg_id);
                        } else {
                            ui_ctx.remove_pinned_message(&msg_id);
                        }
                    }
                    let pinned_count = ui_ctx.pinned_total_count();
                    super::edit_messages(&w, |model| {
                        for i in 0..model.row_count() {
                            let Some(mut m) = model.row_data(i) else {
                                continue;
                            };
                            let hit = if saved {
                                m.message_id.as_str() == msg_id_str.as_str()
                            } else {
                                m.sender_id.as_str() == sender_id_str.as_str()
                                    && m.timestamp_ns.as_str() == timestamp_ns_str.as_str()
                            };
                            if hit {
                                m.is_pinned = new_pinned_state;
                                model.set_row_data(i, m);
                            }
                        }
                    });
                    w.global::<crate::ChatPane>()
                        .set_has_pinned_messages(pinned_count > 0);
                    if new_pinned_state {
                        w.global::<crate::ChatPane>()
                            .set_pinned_message_preview(preview_text.into());
                        w.global::<crate::ChatPane>()
                            .set_pinned_current_index(pinned_count as i32);
                        w.global::<crate::ChatPane>()
                            .set_pinned_total_count(pinned_count as i32);
                    } else {
                        w.global::<crate::ChatPane>()
                            .set_pinned_total_count(pinned_count as i32);
                        if pinned_count == 0 {
                            w.global::<crate::ChatPane>()
                                .set_pinned_message_preview("".into());
                        }
                    }
                    crate::bin_ui::features::header::refresh_pinned_ui(&w, &ui_ctx);
                });
            });
        },
    );
}
