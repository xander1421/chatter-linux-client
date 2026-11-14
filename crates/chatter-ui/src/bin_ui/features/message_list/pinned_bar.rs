//! The pinned-message bar above the list: navigation, previews, refresh.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::MessageId;
use chatter_core::state::ActiveConversation;

use crate::MainWindow;
use super::jump::jump_to_message;

pub(super) fn wire_pinned_navigation(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Previous pinned message
    let weak_window = window.as_weak();
    let ui_ctx_prev = ui_ctx.clone();

    window
        .global::<crate::ChatPane>()
        .on_pinned_prev_clicked(move || {
            ui_ctx_prev.pinned_navigate_prev();
            update_pinned_ui_from_loaded_messages(
                weak_window.clone(),
                ui_ctx_prev.clone(),
                ui_ctx_prev.active_conversation(),
            );
        });

    // Next pinned message
    let weak_window = window.as_weak();
    let ui_ctx_next = ui_ctx.clone();

    window
        .global::<crate::ChatPane>()
        .on_pinned_next_clicked(move || {
            ui_ctx_next.pinned_navigate_next();
            update_pinned_ui_from_loaded_messages(
                weak_window.clone(),
                ui_ctx_next.clone(),
                ui_ctx_next.active_conversation(),
            );
        });

    // Header menu opens the complete core-ordered pin list, not just loaded rows.
    let weak_window = window.as_weak();
    let ui_ctx_show = ui_ctx.clone();
    window
        .global::<crate::ChatPane>()
        .on_pinned_show_clicked(move || {
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx_show.clone();
            let conversation = ui_ctx.active_conversation();
            let revision = ui_ctx.auth_snapshot().session_revision;
            tokio::spawn(async move {
                let mut items = ui_ctx.pinned_display_items(conversation).await;
                for item in &mut items {
                    item.preview = pinned_plaintext_preview(
                        &ui_ctx,
                        conversation,
                        &item.message_id,
                        &item.preview,
                    );
                }
                #[cfg(target_os = "android")]
                let items = {
                    let client = ui_ctx.clone();
                    tokio::task::spawn_blocking(move || {
                        for item in items.iter_mut().filter(|i| i.thumbnail.is_empty()).take(4) {
                            item.thumbnail =
                                pinned_video_thumbnail(&client, conversation, &item.message_id);
                        }
                        items
                    })
                    .await
                    .unwrap_or_default()
                };
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(w) = weak.upgrade() else {
                        return;
                    };
                    if ui_ctx.auth_snapshot().session_revision != revision {
                        return;
                    }
                    if ui_ctx.active_conversation() != conversation {
                        return;
                    }
                    let rows: Vec<crate::PinnedItem> = items
                        .into_iter()
                        .map(|item| crate::PinnedItem {
                            message_id: item.message_id.to_string().into(),
                            preview: item.preview.into(),
                            sender_name: item.sender_name.into(),
                            sent_time: item.sent_time.into(),
                            thumbnail: crate::bin_ui::converters::avatar_bytes_to_image(
                                &item.thumbnail,
                            )
                            .unwrap_or_default(),
                        })
                        .collect();
                    w.global::<crate::ChatPane>()
                        .set_pinned_items(ModelRc::new(slint::VecModel::from(rows)));
                    crate::bin_ui::features::back::push(&w, "pinned-messages");
                });
            });
        });
    let weak_window = window.as_weak();
    let ui_ctx_selected = ui_ctx.clone();
    window
        .global::<crate::ChatPane>()
        .on_pinned_item_selected(move |message_id| {
            let Some(w) = weak_window.upgrade() else {
                return;
            };
            crate::bin_ui::features::back::close(&w, "pinned-messages");
            jump_to_message(&w, &ui_ctx_selected, &message_id);
        });

    // Go to pinned message (scroll to it in chat)
    let weak_window = window.as_weak();
    let ui_ctx_goto = ui_ctx.clone();

    window
        .global::<crate::ChatPane>()
        .on_pinned_go_to_message(move || {
            if let (Some(message_id), Some(w)) = (
                ui_ctx_goto.pinned_current_message_id(),
                weak_window.upgrade(),
            ) {
                jump_to_message(&w, &ui_ctx_goto, &message_id.to_string());
            }
        });
}

/// Resolve the full text before stripping/truncating; core previews can already
/// end halfway through a formatting marker. Non-text captions remain literal.
fn pinned_plaintext_preview(
    ui_ctx: &ChatterClient,
    conversation: ActiveConversation,
    message_id: &MessageId,
    caption: &str,
) -> String {
    let message = match conversation {
        ActiveConversation::AnonGroup(_) => ui_ctx.get_anon_group_message(
            &chatter_core::domain::anon_group::AnonGroupMessageId::from_string(
                message_id.to_string(),
            ),
        ),
        _ => ui_ctx.get_message_local(message_id),
    };
    chatter_core::ui::pin_preview::preview_or_caption(
        message.as_ref().and_then(|message| message.as_text()),
        caption,
    )
}

/// Missing old video pins can derive a local frame before history has been loaded.
#[cfg(target_os = "android")]
fn pinned_video_thumbnail(
    client: &ChatterClient,
    conversation: ActiveConversation,
    id: &MessageId,
) -> Vec<u8> {
    let message = match conversation {
        ActiveConversation::AnonGroup(_) => client.get_anon_group_message(
            &chatter_core::domain::anon_group::AnonGroupMessageId::from_string(id.to_string()),
        ),
        _ => client.get_message_local(id),
    };
    let Some(message) = message else {
        return Vec::new();
    };
    for id in message.attachments.iter().take(16) {
        let Some(attachment) = client.get_attachment_metadata(id) else {
            continue;
        };
        if !attachment.metadata.is_video() {
            continue;
        }
        if let Some(bytes) = attachment.metadata.thumbnail.filter(|b| !b.is_empty()) {
            return bytes;
        }
        return client
            .decrypt_and_extract_video_thumbnail(id)
            .map(|result| result.image_bytes)
            .unwrap_or_default();
    }
    Vec::new()
}

pub(crate) fn refresh_pinned_ui(w: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    update_pinned_ui_from_loaded_messages(
        w.as_weak(),
        ui_ctx.clone(),
        ui_ctx.active_conversation(),
    );
}

/// Update pinned UI by reading the preview from loaded UI messages first,
/// falling back to PinService (regular channels) or AnonGroupService (private groups).
fn update_pinned_ui_from_loaded_messages(
    weak_window: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    conversation: ActiveConversation,
) {
    let state = ui_ctx.chat_snapshot();
    let current_index = state.pinned.current_index as i32;
    let total_count = state.pinned.pinned_message_ids.len() as i32;
    let current_msg_id = ui_ctx.pinned_current_message_id();

    // First try to get preview from currently loaded UI messages (works for all types)
    let msg_id_str = current_msg_id.map(|id| id.to_string()).unwrap_or_default();
    let ui_preview = weak_window.upgrade().and_then(|w| {
        let messages = w.global::<crate::ChatPane>().get_messages();
        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.message_id.as_str() == msg_id_str {
                    let preview = crate::bin_ui::features::auth::inbox::utils::generate_preview_from_ui_message(&msg);
                    if !preview.is_empty() {
                        let thumbnail = msg.attachments.iter()
                            .find(|a| (a.is_image || a.is_video) && a.has_thumbnail)
                            .map(|a| a.thumbnail).unwrap_or_default();
                        let missing_video = msg.attachments.iter().any(|a| a.is_video && !a.has_thumbnail);
                        return Some((preview, thumbnail, missing_video));
                    }
                }
            }
        }
        None
    });

    if let Some((preview, thumbnail, missing_video)) = ui_preview {
        let derive_video =
            cfg!(target_os = "android") && missing_video && thumbnail.size().width == 0;
        // Already have preview from UI, update directly (already on UI thread from callback)
        if let Some(w) = weak_window.upgrade() {
            w.global::<crate::ChatPane>()
                .set_pinned_current_index(current_index + 1);
            w.global::<crate::ChatPane>()
                .set_pinned_total_count(total_count);
            w.global::<crate::ChatPane>()
                .set_pinned_message_preview(preview.into());
            w.global::<crate::ChatPane>()
                .set_pinned_thumbnail(thumbnail);
            w.global::<crate::ChatPane>()
                .set_has_pinned_messages(total_count > 0);
        }
        if !derive_video {
            return;
        }
    }

    // Fall back to DB lookup in background thread
    let revision = ui_ctx.auth_snapshot().session_revision;
    std::thread::spawn(move || {
        let preview = match current_msg_id {
            Some(msg_id) => pinned_plaintext_preview(
                &ui_ctx,
                conversation,
                &msg_id,
                &ui_ctx.pinned_message_preview(&conversation, &msg_id),
            ),
            None => String::new(),
        };

        let thumbnail = current_msg_id
            .and_then(|id| ui_ctx.get_pinned_message(&id))
            .map(|pin| pin.thumbnail)
            .unwrap_or_default();
        #[cfg(target_os = "android")]
        let thumbnail = if thumbnail.is_empty() {
            current_msg_id
                .map(|id| pinned_video_thumbnail(&ui_ctx, conversation, &id))
                .unwrap_or_default()
        } else {
            thumbnail
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak_window.upgrade() {
                if ui_ctx.active_conversation() != conversation
                    || ui_ctx.pinned_current_message_id() != current_msg_id
                    || ui_ctx.auth_snapshot().session_revision != revision
                {
                    return;
                }
                w.global::<crate::ChatPane>().set_pinned_thumbnail(
                    crate::bin_ui::converters::avatar_bytes_to_image(&thumbnail)
                        .unwrap_or_default(),
                );
                w.global::<crate::ChatPane>()
                    .set_pinned_current_index(current_index + 1);
                w.global::<crate::ChatPane>()
                    .set_pinned_total_count(total_count);
                w.global::<crate::ChatPane>()
                    .set_pinned_message_preview(preview.into());
                w.global::<crate::ChatPane>()
                    .set_has_pinned_messages(total_count > 0);
            }
        });
    });
}
