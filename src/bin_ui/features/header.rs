//! Header callbacks.
//!
//! Handles channel header actions (search, pinned, settings, load more).

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use chatter_linux::app::AppContext;
use chatter_linux::domain::{Attachment, FileCategory, Message};
use chatter_linux::storage::AttachmentRepository;

use crate::bin_ui::converters::{message_to_display_data, MessageDisplayData};
use crate::MainWindow;

/// Generate a preview text for a pinned message (with attachment support).
fn generate_pinned_preview(message: &Message, attachments: &[Attachment]) -> String {
    // First try to get text content
    if let Some(text) = message.as_text() {
        if !text.is_empty() {
            return if text.len() > 80 {
                format!("{}...", &text[..77])
            } else {
                text.to_string()
            };
        }
    }

    // If no text, describe attachments
    if !attachments.is_empty() {
        if attachments.len() == 1 {
            let att = &attachments[0];
            let category = att.metadata.category();
            let (emoji, type_name) = match category {
                FileCategory::Image => ("📷", "Photo"),
                FileCategory::Video => ("🎬", "Video"),
                FileCategory::Audio => ("🎵", "Voice message"),
                FileCategory::Document => ("📄", "Document"),
                FileCategory::Code => ("📝", "Code"),
                FileCategory::Archive => ("📦", "Archive"),
                FileCategory::Other => ("📎", "File"),
            };
            return format!("{} {}", emoji, type_name);
        } else {
            return format!("📎 {} attachments", attachments.len());
        }
    }

    // Fallback for unresolved attachment IDs
    if !message.attachments.is_empty() {
        if message.attachments.len() == 1 {
            "📎 Attachment".to_string()
        } else {
            format!("📎 {} attachments", message.attachments.len())
        }
    } else {
        "[Pinned message]".to_string()
    }
}

/// Wire header callbacks.
pub fn wire(window: &MainWindow, ctx: &AppContext) {
    wire_search_clicked(window);
    wire_search_callbacks(window, ctx);
    wire_pinned_navigation(window, ctx);
    wire_chat_settings_clicked(window, ctx);
    wire_load_more(window, ctx);
    wire_scroll_to_bottom(window);
}

fn wire_scroll_to_bottom(window: &MainWindow) {
    window.on_scroll_to_bottom_clicked(|| {
        tracing::debug!("Scroll-to-bottom button clicked");
        // Scrolling is handled in Slint
    });
}

fn wire_search_clicked(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_search_clicked(move || {
        let weak = weak_window.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_active_modal("search".into());
                tracing::debug!("Search modal opened");
            }
        });
    });
}

fn wire_search_callbacks(window: &MainWindow, ctx: &AppContext) {
    // Search messages
    let weak_window = window.as_weak();
    let search_index = ctx.search_index.clone();
    let message_repo = ctx.message_repo.clone();
    let channel_repo = ctx.channel_repo.clone();
    let user_repo = ctx.user_repo.clone();
    let chat_state = ctx.chat_state.clone();

    window.on_search_messages(move |query: slint::SharedString| {
        let query_str = query.to_string();
        if query_str.trim().is_empty() {
            let weak = weak_window.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_search_results(ModelRc::new(VecModel::default()));
                    w.set_search_result_count(0);
                    w.set_search_searching(false);
                }
            });
            return;
        }

        let weak = weak_window.clone();
        let search_index = search_index.clone();
        let message_repo = message_repo.clone();
        let channel_repo = channel_repo.clone();
        let user_repo = user_repo.clone();
        let chat_state = chat_state.clone();

        // Set searching state
        {
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_search_searching(true);
                }
            });
        }

        tokio::spawn(async move {
            // Get search scope
            let search_all = weak
                .upgrade()
                .map(|w| w.get_search_all_channels())
                .unwrap_or(false);

            let channel_id = if search_all {
                None
            } else {
                chat_state.get().active_channel_id
            };

            // Perform search
            let results = match search_index.search(&query_str, channel_id.as_ref(), 50) {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("Search failed: {}", e);
                    let weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_search_searching(false);
                        }
                    });
                    return;
                }
            };

            // Convert to display format
            let mut display_results: Vec<crate::SearchResult> = Vec::with_capacity(results.len());
            for result in &results {
                // Get message details
                let msg = match message_repo.get(&result.message_id) {
                    Ok(Some(m)) => m,
                    _ => continue,
                };

                // Get channel name
                let channel_name = channel_repo
                    .get(&result.channel_id)
                    .ok()
                    .flatten()
                    .map(|c| c.name)
                    .unwrap_or_else(|| "Unknown".to_string());

                // Get sender name
                let sender_name = msg
                    .sender_id
                    .as_ref()
                    .and_then(|id| {
                        user_repo
                            .get_profile(id)
                            .ok()
                            .flatten()
                            .map(|p| p.display_name)
                            .or_else(|| user_repo.get(id).ok().flatten().map(|u| u.username))
                    })
                    .unwrap_or_else(|| "Unknown".to_string());

                // Format timestamp
                let timestamp = msg.created_at.format("%b %d, %H:%M").to_string();

                // Get content preview
                let content_preview = msg
                    .as_text()
                    .map(|t| {
                        if t.len() > 100 {
                            format!("{}...", &t[..97])
                        } else {
                            t.to_string()
                        }
                    })
                    .unwrap_or_else(|| "[Attachment]".to_string());

                display_results.push(crate::SearchResult {
                    message_id: result.message_id.to_string().into(),
                    channel_id: result.channel_id.to_string().into(),
                    channel_name: channel_name.into(),
                    sender_name: sender_name.into(),
                    content_preview: content_preview.into(),
                    timestamp: timestamp.into(),
                    highlight_start: 0,
                    highlight_end: 0,
                });
            }

            let result_count = display_results.len() as i32;

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_search_results(ModelRc::new(VecModel::from(display_results)));
                    w.set_search_result_count(result_count);
                    w.set_search_searching(false);
                    tracing::debug!("Search completed with {} results", result_count);
                }
            });
        });
    });

    // Search result clicked - navigate to message
    let weak_window = window.as_weak();

    window.on_search_result_clicked(move |message_id: slint::SharedString| {
        let message_id_str = message_id.to_string();
        tracing::info!("Search result clicked: {}", message_id_str);
        // TODO: Navigate to message in chat
        // This would require:
        // 1. Switch to the message's channel
        // 2. Load messages around that message
        // 3. Scroll to the message
        let weak = weak_window.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_active_modal("none".into());
            }
        });
    });

    // Toggle search scope
    let weak_window = window.as_weak();

    window.on_search_toggle_scope(move || {
        let weak = weak_window.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let current = w.get_search_all_channels();
                w.set_search_all_channels(!current);
                tracing::debug!("Search scope toggled to: {}", if !current { "all channels" } else { "current channel" });
            }
        });
    });
}

fn wire_pinned_navigation(window: &MainWindow, ctx: &AppContext) {
    // Previous pinned message
    let pinned_state = ctx.pinned_state.clone();
    let weak_window = window.as_weak();
    let message_repo = ctx.message_repo.clone();
    let attachment_repo = ctx.attachment_repo.clone();

    window.on_pinned_prev_clicked(move || {
        pinned_state.navigate_prev();
        update_pinned_ui(&weak_window, &pinned_state, &message_repo, &attachment_repo);
    });

    // Next pinned message
    let pinned_state = ctx.pinned_state.clone();
    let weak_window = window.as_weak();
    let message_repo = ctx.message_repo.clone();
    let attachment_repo = ctx.attachment_repo.clone();

    window.on_pinned_next_clicked(move || {
        pinned_state.navigate_next();
        update_pinned_ui(&weak_window, &pinned_state, &message_repo, &attachment_repo);
    });

    // Dismiss pinned bar
    let pinned_state = ctx.pinned_state.clone();
    let weak_window = window.as_weak();

    window.on_pinned_dismiss_clicked(move || {
        pinned_state.dismiss();
        let weak = weak_window.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_has_pinned_messages(false);
            }
        });
        tracing::debug!("Pinned bar dismissed");
    });

    // Go to pinned message (scroll to it in chat)
    let pinned_state = ctx.pinned_state.clone();
    let weak_window = window.as_weak();

    window.on_pinned_go_to_message(move || {
        if let Some(message_id) = pinned_state.current_message_id() {
            let message_id_str = message_id.to_string();
            let weak = weak_window.clone();

            // Find the message index in the current message list and scroll to it
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    let messages = w.get_messages();
                    let message_count = messages.row_count();

                    // Find the index of the pinned message
                    for i in 0..message_count {
                        if let Some(msg) = messages.row_data(i) {
                            if msg.message_id.as_str() == message_id_str.as_str() {
                                // Calculate scroll position: negative offset from top
                                // Estimated message height: 80px, plus some padding
                                let estimated_height = 80.0f32;
                                let padding = 16.0f32; // Spacing.md
                                let scroll_pos = -((i as f32 * estimated_height) + padding);

                                // Set scroll position directly
                                w.set_message_scroll_position(scroll_pos);
                                // Mark as not at bottom since we're scrolling to a specific message
                                w.set_is_at_bottom(false);

                                tracing::info!(
                                    message_id = %message_id_str,
                                    index = i,
                                    scroll_position = scroll_pos,
                                    "Scrolling to pinned message"
                                );
                                return;
                            }
                        }
                    }

                    tracing::warn!(
                        message_id = %message_id_str,
                        "Pinned message not found in current message list"
                    );
                }
            });
        }
    });
}

fn update_pinned_ui(
    weak_window: &slint::Weak<MainWindow>,
    pinned_state: &chatter_linux::state::PinnedState,
    message_repo: &std::sync::Arc<chatter_linux::storage::MessageRepository>,
    attachment_repo: &std::sync::Arc<AttachmentRepository>,
) {
    let state = pinned_state.get();
    let current_index = state.current_index as i32;
    let total_count = state.pinned_message_ids.len() as i32;

    // Get preview text for current pinned message (with attachment support)
    let preview = if let Some(msg_id) = pinned_state.current_message_id() {
        if let Ok(Some(msg)) = message_repo.get(&msg_id) {
            let attachments: Vec<Attachment> = msg
                .attachments
                .iter()
                .filter_map(|att_id| attachment_repo.get(att_id).ok().flatten())
                .collect();
            generate_pinned_preview(&msg, &attachments)
        } else {
            "[Pinned message]".to_string()
        }
    } else {
        String::new()
    };

    let weak = weak_window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            w.set_pinned_current_index(current_index + 1); // 1-based for display
            w.set_pinned_total_count(total_count);
            w.set_pinned_message_preview(preview.into());
            w.set_has_pinned_messages(total_count > 0);
        }
    });
}

fn wire_chat_settings_clicked(window: &MainWindow, ctx: &AppContext) {
    let chat_state = ctx.chat_state.clone();
    let channel_repo = ctx.channel_repo.clone();

    window.on_chat_settings_clicked(move || {
        let chat_state = chat_state.clone();
        let channel_repo = channel_repo.clone();

        tokio::spawn(async move {
            let channel_id = match chat_state.get().active_channel_id {
                Some(id) => id,
                None => {
                    tracing::warn!("Cannot open channel settings: no active channel");
                    return;
                }
            };

            // Get channel info for logging
            let channel_info = channel_repo
                .get(&channel_id)
                .ok()
                .flatten()
                .map(|c| c.name)
                .unwrap_or_else(|| "Unknown".to_string());

            // TODO: Implement channel settings modal UI in Slint
            // Required: ui/modals/channel_settings.slint with:
            // - channel-name, channel-description properties
            // - member list with role management
            // - notification settings
            // - leave/archive channel actions
            tracing::info!(
                "Channel settings requested for: {} ({})",
                channel_info,
                channel_id
            );
            tracing::info!("Channel settings modal UI not yet implemented");
        });
    });
}

fn wire_load_more(window: &MainWindow, ctx: &AppContext) {
    let weak_window = window.as_weak();
    let chat_state = ctx.chat_state.clone();
    let message_repo = ctx.message_repo.clone();
    let message_state = ctx.message_state.clone();
    let user_repo = ctx.user_repo.clone();
    let attachment_repo = ctx.attachment_repo.clone();

    window.on_load_more(move || {
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();
        let message_repo = message_repo.clone();
        let message_state = message_state.clone();
        let user_repo = user_repo.clone();
        let attachment_repo = attachment_repo.clone();

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

            // Convert older messages (come in DESC order from DB)
            let mut display_messages: Vec<MessageDisplayData> = Vec::with_capacity(older_messages.len());
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

                // Resolve attachments for this message
                let resolved_attachments: Vec<_> = msg
                    .attachments
                    .iter()
                    .filter_map(|att_id| attachment_repo.get(att_id).ok().flatten())
                    .collect();

                display_messages.push(message_to_display_data(msg, &sender_name, &resolved_attachments));
            }

            // Reverse to get chronological order (oldest first)
            display_messages.reverse();

            let msg_count = older_messages.len();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    // Convert display data to Slint types (must be done on UI thread)
                    let slint_messages: Vec<_> = display_messages.iter().map(|d| d.to_slint()).collect();

                    let current_model = w.get_messages();
                    let current_count = current_model.row_count();

                    // Prepend older messages to current (older go at top)
                    let mut combined = slint_messages;
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
