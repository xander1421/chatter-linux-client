//! Header callbacks.
//!
//! Handles channel header actions (search, pinned, settings, load more).

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use chatter_linux::app::AppContext;

use crate::bin_ui::converters::message_to_slint;
use crate::{MainWindow, MessageItemData};

/// Wire header callbacks.
pub fn wire(window: &MainWindow, ctx: &AppContext) {
    wire_search_clicked(window);
    wire_pinned_clicked(window, ctx);
    wire_chat_settings_clicked(window, ctx);
    wire_load_more(window, ctx);
}

fn wire_search_clicked(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_search_clicked(move || {
        let weak = weak_window.clone();
        tracing::debug!("Search clicked");

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(_w) = weak.upgrade() {
                tracing::info!("Search modal requested (not yet implemented)");
            }
        });
    });
}

fn wire_pinned_clicked(window: &MainWindow, ctx: &AppContext) {
    let weak_window = window.as_weak();
    let chat_state = ctx.chat_state.clone();

    window.on_pinned_clicked(move || {
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();

        tokio::spawn(async move {
            let channel_id = match chat_state.get().active_channel_id {
                Some(id) => id,
                None => {
                    tracing::warn!("Cannot show pinned messages: no active channel");
                    return;
                }
            };

            tracing::debug!("Pinned messages clicked for channel: {}", channel_id);

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(_w) = weak.upgrade() {
                    tracing::info!("Pinned messages bar toggled for channel: {}", channel_id);
                }
            });
        });
    });
}

fn wire_chat_settings_clicked(window: &MainWindow, ctx: &AppContext) {
    let weak_window = window.as_weak();
    let chat_state = ctx.chat_state.clone();

    window.on_chat_settings_clicked(move || {
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();

        let channel_id = chat_state.get().active_channel_id;

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(_w) = weak.upgrade() {
                if let Some(channel_id) = channel_id {
                    tracing::info!("Channel settings requested for: {}", channel_id);
                } else {
                    tracing::warn!("Cannot open channel settings: no active channel");
                }
            }
        });
    });
}

fn wire_load_more(window: &MainWindow, ctx: &AppContext) {
    let weak_window = window.as_weak();
    let chat_state = ctx.chat_state.clone();
    let message_repo = ctx.message_repo.clone();
    let message_state = ctx.message_state.clone();
    let user_repo = ctx.user_repo.clone();

    window.on_load_more(move || {
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();
        let message_repo = message_repo.clone();
        let message_state = message_state.clone();
        let user_repo = user_repo.clone();

        tokio::spawn(async move {
            let channel_id = match chat_state.get().active_channel_id {
                Some(id) => id,
                None => {
                    tracing::warn!("Cannot load more: no active channel");
                    return;
                }
            };

            if message_state.is_loading(&channel_id) {
                tracing::trace!("Already loading messages for channel: {}", channel_id);
                return;
            }

            let cursor = message_state.get_cursor(&channel_id);
            if let Some(ref c) = cursor {
                if !c.has_more {
                    tracing::trace!("No more messages to load for channel: {}", channel_id);
                    return;
                }
            }

            message_state.set_loading(channel_id, true);

            let before = cursor.and_then(|c| c.oldest_message_id);

            let older_messages = match message_repo.list_for_channel(&channel_id, before.as_ref(), 50) {
                Ok(msgs) => msgs,
                Err(e) => {
                    tracing::error!("Failed to load more messages: {}", e);
                    message_state.set_loading(channel_id, false);
                    return;
                }
            };

            let has_more = older_messages.len() >= 50;
            let new_cursor = if !older_messages.is_empty() {
                chatter_linux::state::PaginationCursor::new(
                    has_more,
                    older_messages.first().map(|m| m.id),
                    older_messages.first().map(|m| m.created_at),
                )
            } else {
                chatter_linux::state::PaginationCursor::exhausted()
            };
            message_state.set_cursor(channel_id, new_cursor);

            let mut slint_messages: Vec<MessageItemData> = Vec::with_capacity(older_messages.len());
            for msg in &older_messages {
                let sender_name = if let Some(sender_id) = &msg.sender_id {
                    user_repo
                        .get_profile(sender_id)
                        .ok()
                        .flatten()
                        .map(|p| p.display_name)
                        .or_else(|| {
                            user_repo
                                .get(sender_id)
                                .ok()
                                .flatten()
                                .map(|u| u.username)
                        })
                        .unwrap_or_else(|| "Unknown".to_string())
                } else {
                    "Anonymous".to_string()
                };
                slint_messages.push(message_to_slint(msg, &sender_name));
            }

            let msg_count = older_messages.len();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    let current_model = w.get_messages();
                    let current_count = current_model.row_count();

                    let mut combined: Vec<MessageItemData> = slint_messages;
                    for i in 0..current_count {
                        if let Some(msg) = current_model.row_data(i) {
                            combined.push(msg);
                        }
                    }

                    w.set_messages(ModelRc::new(VecModel::from(combined)));
                    tracing::debug!("Loaded {} older messages for channel", msg_count);
                }
            });

            message_state.set_loading(channel_id, false);
        });
    });
}
