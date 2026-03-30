//! Header callbacks.
//!
//! Handles channel header actions (search, pinned, settings, load more).

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::state::ActiveConversation;

use crate::bin_ui::converters::{message_to_display_data, MessageDisplayData};
use crate::MainWindow;

/// Wire header callbacks.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_search_clicked(window);
    wire_search_callbacks(window, ui_ctx);
    wire_pinned_navigation(window, ui_ctx);
    wire_chat_settings_clicked(window, ui_ctx);
    wire_invite_member(window, ui_ctx);
    wire_members_panel(window, ui_ctx);
    wire_load_more(window, ui_ctx);
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
        // Already on UI thread - no need for invoke_from_event_loop
        if let Some(w) = weak_window.upgrade() {
            w.set_active_modal("search".into());
            tracing::debug!("Search modal opened");
        }
    });
}

fn wire_search_callbacks(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Search messages
    let weak_window = window.as_weak();
    let message_service = ui_ctx.messages().clone();
    let channel_service = ui_ctx.channels().clone();
    let user_service = ui_ctx.users().clone();
    let chat_state = ui_ctx.chat_state().clone();

    window.on_search_messages(move |query: slint::SharedString| {
        let query_str = query.to_string();
        if query_str.trim().is_empty() {
            // Already on UI thread - no need for invoke_from_event_loop
            if let Some(w) = weak_window.upgrade() {
                w.set_search_results(ModelRc::new(VecModel::default()));
                w.set_search_result_count(0);
                w.set_search_searching(false);
            }
            return;
        }

        let weak = weak_window.clone();
        let message_service = message_service.clone();
        let channel_service = channel_service.clone();
        let user_service = user_service.clone();
        let chat_state = chat_state.clone();

        // Get search scope and set searching state - already on UI thread
        let search_all = weak
            .upgrade()
            .map(|w| {
                w.set_search_searching(true);
                w.get_search_all_channels()
            })
            .unwrap_or(false);

        // Search in background thread (DB queries are sync)
        std::thread::spawn(move || {
            let channel_id = if search_all {
                None
            } else {
                chat_state.get().active_channel_id
            };

            // Perform search using message service's local search
            let results = match message_service.search_local(&query_str, channel_id.as_ref(), 50) {
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
                let msg = match message_service.get_message_local(&result.message_id) {
                    Ok(Some(m)) => m,
                    _ => continue,
                };

                // Get channel name
                let channel_name = channel_service
                    .get_channel_local(&result.conversation_id)
                    .ok()
                    .flatten()
                    .map(|c| c.name)
                    .unwrap_or_else(|| "Unknown".to_string());

                // Get sender name
                let sender_name = msg
                    .sender_id
                    .as_ref()
                    .map(|id| user_service.get_display_name(id))
                    .unwrap_or_else(|| "Unknown".to_string());

                // Format timestamp (convert UTC to local time)
                let timestamp = msg.created_at.with_timezone(&chrono::Local).format("%b %d, %H:%M").to_string();

                // Get content preview
                let content_preview = msg
                    .as_text()
                    .map(|t| chatter_core::services::gateway::shared::truncate_preview(&t))
                    .unwrap_or_else(|| "[Attachment]".to_string());

                display_results.push(crate::SearchResult {
                    message_id: result.message_id.to_string().into(),
                    channel_id: result.conversation_id.to_string().into(),
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
        // Already on UI thread - no need for invoke_from_event_loop
        if let Some(w) = weak_window.upgrade() {
            w.set_active_modal("none".into());
        }
    });

    // Toggle search scope
    let weak_window = window.as_weak();

    window.on_search_toggle_scope(move || {
        // Already on UI thread - no need for invoke_from_event_loop
        if let Some(w) = weak_window.upgrade() {
            let current = w.get_search_all_channels();
            w.set_search_all_channels(!current);
            tracing::debug!("Search scope toggled to: {}", if !current { "all channels" } else { "current channel" });
        }
    });
}

fn wire_pinned_navigation(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Previous pinned message
    let pinned_state = ui_ctx.pinned_state().clone();
    let weak_window = window.as_weak();
    let pin_service = ui_ctx.pins().clone();
    let anon_group_service = ui_ctx.anon_groups().clone();
    let chat_state = ui_ctx.chat_state().clone();

    window.on_pinned_prev_clicked(move || {
        pinned_state.navigate_prev();
        update_pinned_ui_from_loaded_messages(weak_window.clone(), pinned_state.clone(), pin_service.clone(), anon_group_service.clone(), chat_state.active_conversation());
    });

    // Next pinned message
    let pinned_state = ui_ctx.pinned_state().clone();
    let weak_window = window.as_weak();
    let pin_service = ui_ctx.pins().clone();
    let anon_group_service = ui_ctx.anon_groups().clone();
    let chat_state = ui_ctx.chat_state().clone();

    window.on_pinned_next_clicked(move || {
        pinned_state.navigate_next();
        update_pinned_ui_from_loaded_messages(weak_window.clone(), pinned_state.clone(), pin_service.clone(), anon_group_service.clone(), chat_state.active_conversation());
    });

    // Dismiss pinned bar
    let pinned_state = ui_ctx.pinned_state().clone();
    let weak_window = window.as_weak();

    window.on_pinned_dismiss_clicked(move || {
        pinned_state.dismiss();
        if let Some(w) = weak_window.upgrade() {
            w.set_has_pinned_messages(false);
        }
        tracing::debug!("Pinned bar dismissed");
    });

    // Re-show dismissed pinned bar (pin button in header)
    let pinned_state = ui_ctx.pinned_state().clone();
    let weak_window = window.as_weak();
    let pin_service = ui_ctx.pins().clone();
    let anon_group_service = ui_ctx.anon_groups().clone();
    let chat_state = ui_ctx.chat_state().clone();

    window.on_pinned_show_clicked(move || {
        let state = pinned_state.get();
        if state.pinned_message_ids.is_empty() {
            return;
        }
        // Un-dismiss and reset to first pinned message
        pinned_state.navigate_to(0);
        pinned_state.undismiss();
        update_pinned_ui_from_loaded_messages(
            weak_window.clone(),
            pinned_state.clone(),
            pin_service.clone(),
            anon_group_service.clone(),
            chat_state.active_conversation(),
        );
    });

    // Go to pinned message (scroll to it in chat)
    let pinned_state = ui_ctx.pinned_state().clone();
    let weak_window = window.as_weak();

    window.on_pinned_go_to_message(move || {
        if let Some(message_id) = pinned_state.current_message_id() {
            let message_id_str = message_id.to_string();

            // Already on UI thread - no need for invoke_from_event_loop
            if let Some(w) = weak_window.upgrade() {
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
        }
    });
}

/// Update pinned UI by reading the preview from loaded UI messages first,
/// falling back to PinService (regular channels) or AnonGroupService (private groups).
fn update_pinned_ui_from_loaded_messages(
    weak_window: slint::Weak<MainWindow>,
    pinned_state: chatter_core::state::PinnedState,
    pin_service: Arc<chatter_core::services::pin_service::PinService>,
    anon_group_service: Arc<chatter_core::services::anon_group::AnonGroupService>,
    conversation: ActiveConversation,
) {
    let state = pinned_state.get();
    let current_index = state.current_index as i32;
    let total_count = state.pinned_message_ids.len() as i32;
    let current_msg_id = pinned_state.current_message_id();

    // First try to get preview from currently loaded UI messages (works for all types)
    let msg_id_str = current_msg_id.map(|id| id.to_string()).unwrap_or_default();
    let ui_preview = weak_window.upgrade().and_then(|w| {
        let messages = w.get_messages();
        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.message_id.as_str() == msg_id_str {
                    let preview = crate::bin_ui::features::auth::inbox::utils::generate_preview_from_ui_message(&msg);
                    if !preview.is_empty() {
                        return Some(preview);
                    }
                }
            }
        }
        None
    });

    if let Some(preview) = ui_preview {
        // Already have preview from UI, update directly (already on UI thread from callback)
        if let Some(w) = weak_window.upgrade() {
            w.set_pinned_current_index(current_index + 1);
            w.set_pinned_total_count(total_count);
            w.set_pinned_message_preview(preview.into());
            w.set_has_pinned_messages(total_count > 0);
        }
        return;
    }

    // Fall back to DB lookup in background thread
    std::thread::spawn(move || {
        let preview = if let Some(msg_id) = current_msg_id {
            if let ActiveConversation::AnonGroup(group_id) = conversation {
                // Private group: look up from unified messages repo
                if let Ok(Some(msg)) = anon_group_service.get_message(&chatter_core::domain::anon_group::AnonGroupMessageId::from_string(msg_id.to_string())) {
                    msg.as_text()
                        .filter(|t| !t.is_empty())
                        .map(|t| if t.len() > 80 { format!("{}...", &t[..77]) } else { t.to_string() })
                        .unwrap_or_else(|| "[Pinned message]".to_string())
                } else {
                    let _ = group_id; // suppress unused warning
                    "[Pinned message]".to_string()
                }
            } else {
                // Regular channel/DM: use PinService
                if let Ok(Some(pinned_data)) = pin_service.get_pinned_message(&msg_id) {
                    pinned_data.preview
                } else {
                    "[Pinned message]".to_string()
                }
            }
        } else {
            String::new()
        };

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak_window.upgrade() {
                w.set_pinned_current_index(current_index + 1);
                w.set_pinned_total_count(total_count);
                w.set_pinned_message_preview(preview.into());
                w.set_has_pinned_messages(total_count > 0);
            }
        });
    });
}

fn wire_chat_settings_clicked(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let chat_state = ui_ctx.chat_state().clone();
    let channel_service = ui_ctx.channels().clone();
    let user_service = ui_ctx.users().clone();
    let anon_group_service = ui_ctx.anon_groups().clone();

    window.on_chat_settings_clicked(move || {
        let conversation = chat_state.active_conversation();

        if let ActiveConversation::AnonGroup(group_id) = conversation {
            // Private group settings - move DB queries to background thread
            let anon_group_service = anon_group_service.clone();
            let user_service = user_service.clone();
            let weak = weak_window.clone();

            std::thread::spawn(move || {

                // Get group info for settings modal (DB query in background)
                let group = match anon_group_service.get_group(&group_id) {
                    Ok(Some(g)) => g,
                    Ok(None) => {
                        tracing::error!("Private group not found: {}", group_id);
                        return;
                    }
                    Err(e) => {
                        tracing::error!("Failed to get private group: {}", e);
                        return;
                    }
                };

                let members = anon_group_service.get_members(&group_id).unwrap_or_default();
                let member_count = members.len() as i32;
                let group_name = group.name.clone();

                // Get current user ID to check if owner
                let current_user_id = match anon_group_service.current_user_id() {
                    Ok(id) => id,
                    _ => return,
                };
                let is_owner = group.owner_id == current_user_id || group.my_role == chatter_core::domain::anon_group::AnonGroupRole::Owner;
                let is_admin = group.my_role == chatter_core::domain::anon_group::AnonGroupRole::Admin;

                // Convert members to Slint data
                let mut slint_members = Vec::with_capacity(members.len());
                for member in members {
                    let display_name = if let Some(ref name) = member.display_name {
                        name.clone()
                    } else {
                        user_service.get_display_name(&member.user_id)
                    };

                    let role = match member.role {
                        chatter_core::domain::anon_group::AnonGroupRole::Owner => "owner",
                        chatter_core::domain::anon_group::AnonGroupRole::Admin => "admin",
                        chatter_core::domain::anon_group::AnonGroupRole::Member => "member",
                    };

                    slint_members.push(crate::ChannelMemberData {
                        user_id: member.user_id.to_string().into(),
                        display_name: display_name.into(),
                        username: "".into(),
                        role: role.into(),
                        joined_at: "".into(),
                    });
                }

                // Open private group settings modal
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_channel_settings_id(group_id.to_string().into());
                        w.set_channel_settings_name(group_name.into());
                        w.set_channel_settings_type("private-group".into());
                        w.set_channel_settings_member_count(member_count);
                        w.set_channel_settings_is_owner(is_owner);
                        w.set_channel_settings_can_manage_members(is_owner || is_admin);
                        w.set_channel_settings_members(ModelRc::new(VecModel::from(slint_members)));
                        w.set_active_modal("channel-settings".into());
                        tracing::debug!(
                            "Opened settings for private group {} with {} members",
                            group_id,
                            member_count
                        );
                    }
                });
            });
            return;
        }

        // Regular channel settings
        let channel_id = match conversation {
            ActiveConversation::Channel(id) | ActiveConversation::MailboxDm { channel_id: id, .. } | ActiveConversation::ServerDm { channel_id: id, .. } => id,
            _ => {
                tracing::warn!("Cannot open channel settings: no active channel");
                return;
            }
        };

        super::modals::open_channel_settings(
            weak_window.clone(),
            channel_service.clone(),
            user_service.clone(),
            channel_id,
        );
    });
}

fn wire_invite_member(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let chat_state = ui_ctx.chat_state().clone();

    window.on_invite_member(move || {
        let conversation = chat_state.active_conversation();
        let weak = weak_window.clone();

        let _ = slint::invoke_from_event_loop(move || {
            let Some(w) = weak.upgrade() else { return };

            match conversation {
                ActiveConversation::AnonGroup(group_id) => {
                    // Open private group invite modal
                    w.set_invite_private_group_id(group_id.to_string().into());
                    w.set_active_modal("invite-private-group".into());
                }
                ActiveConversation::Channel(channel_id) => {
                    // Open channel invite modal
                    w.set_invite_member_channel_id(channel_id.to_string().into());
                    w.set_active_modal("invite-member".into());
                }
                _ => {
                    tracing::debug!("Invite not available for this conversation type");
                }
            }
        });
    });
}

fn wire_load_more(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let chat_state = ui_ctx.chat_state().clone();
    let message_service = ui_ctx.messages().clone();
    let message_state = ui_ctx.message_state().clone();
    let user_service = ui_ctx.users().clone();
    let attachment_service = ui_ctx.attachments().clone();

    window.on_load_more(move || {
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();
        let message_service = message_service.clone();
        let message_state = message_state.clone();
        let user_service = user_service.clone();
        let attachment_service = attachment_service.clone();

        // Load messages in background thread (DB queries are sync)
        std::thread::spawn(move || {
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

            let older_messages = match message_service.get_messages_local(&channel_id, before.as_ref(), 50) {
                Ok(msgs) => msgs,
                Err(e) => {
                    tracing::error!("Failed to load more messages: {}", e);
                    message_state.set_loading(channel_id, false);
                    return;
                }
            };

            let has_more = older_messages.len() >= 50;
            let new_cursor = if !older_messages.is_empty() {
                chatter_core::state::PaginationCursor::new(
                    has_more,
                    older_messages.first().map(|m| m.id),
                    older_messages.first().map(|m| m.created_at),
                )
            } else {
                chatter_core::state::PaginationCursor::exhausted()
            };
            message_state.set_cursor(channel_id, new_cursor);

            // Convert older messages (come in DESC order from DB)
            let mut display_messages: Vec<MessageDisplayData> = Vec::with_capacity(older_messages.len());
            for msg in &older_messages {
                let sender_name = if let Some(sender_id) = &msg.sender_id {
                    user_service.get_display_name(sender_id)
                } else {
                    "Anonymous".to_string()
                };

                // Resolve attachments for this message
                let resolved_attachments: Vec<_> = msg
                    .attachments
                    .iter()
                    .filter_map(|att_id| attachment_service.get(att_id).ok().flatten())
                    .collect();

                display_messages.push(message_to_display_data(msg, &sender_name, &resolved_attachments));
            }

            // Reverse to get chronological order (oldest first)
            display_messages.reverse();

            // Pre-decode attachment thumbnails before UI thread
            for msg in &mut display_messages {
                msg.decode_attachment_thumbnails();
            }

            let msg_count = older_messages.len();
            // Get current user ID for is_own calculation
            let current_user_id = user_service.current_user_id().ok().map(|id| id.to_string()).unwrap_or_default();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    // Convert display data to Slint types (must be done on UI thread)
                    let slint_messages: Vec<_> = display_messages.iter().map(|d| d.to_slint_with_user(&current_user_id)).collect();

                    let current_model = w.get_messages();
                    let current_count = current_model.row_count();

                    // Collect current message IDs for deduplication
                    let current_ids: std::collections::HashSet<String> = (0..current_count)
                        .filter_map(|i| current_model.row_data(i))
                        .map(|msg| msg.message_id.to_string())
                        .collect();

                    // Prepend older messages to current (older go at top), skipping duplicates
                    let mut combined: Vec<_> = slint_messages
                        .into_iter()
                        .filter(|msg| !current_ids.contains(msg.message_id.as_str()))
                        .collect();
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

/// Wire members panel toggle — loads members when panel opens.
fn wire_members_panel(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_members_panel_toggle(move || {
        let weak = weak.clone();
        let ui_ctx = ui_ctx.clone();
        let conversation = ui_ctx.chat_state().active_conversation();

        tokio::spawn(async move {
            let members = match conversation {
                ActiveConversation::Channel(channel_id) => {
                    match ui_ctx.channels().get_channel_members(&channel_id, 200, 0).await {
                        Ok(memberships) => {
                            memberships.into_iter().map(|m| {
                                let role = format!("{:?}", m.role).to_lowercase();
                                let display_name = ui_ctx.users().get_display_name(&m.user_id);
                                let username = ui_ctx.users().get_user_local(&m.user_id)
                                    .ok().flatten()
                                    .map(|u| u.username.clone())
                                    .unwrap_or_default();
                                crate::ChannelMemberData {
                                    user_id: m.user_id.to_string().into(),
                                    display_name: display_name.into(),
                                    username: username.into(),
                                    role: role.into(),
                                    joined_at: "".into(),
                                }
                            }).collect::<Vec<_>>()
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "Failed to load channel members");
                            Vec::new()
                        }
                    }
                }
                ActiveConversation::AnonGroup(group_id) => {
                    match ui_ctx.anon_groups().get_members(&group_id) {
                        Ok(members) => {
                            members.into_iter().map(|m| {
                                let display_name = m.display_name.clone()
                                    .filter(|n| !n.is_empty())
                                    .unwrap_or_else(|| format!("User {}", &m.user_id.to_string()[..8]));
                                let role = format!("{:?}", m.role).to_lowercase();
                                crate::ChannelMemberData {
                                    user_id: m.user_id.to_string().into(),
                                    display_name: display_name.into(),
                                    username: "".into(),
                                    role: role.into(),
                                    joined_at: "".into(),
                                }
                            }).collect::<Vec<_>>()
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "Failed to load group members");
                            Vec::new()
                        }
                    }
                }
                _ => Vec::new(),
            };

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_members_panel_members(
                        ModelRc::new(VecModel::from(members))
                    );
                }
            });
        });
    });
}
