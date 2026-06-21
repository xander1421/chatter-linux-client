//! Header callbacks.
//!
//! Handles channel header actions (search, pinned, settings, load more).

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::state::ActiveConversation;

use crate::bin_ui::converters::messages_to_display_batch;
use crate::MainWindow;

/// Wire header callbacks.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
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

fn wire_pinned_navigation(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Previous pinned message
    let chat_state_prev_nav = ui_ctx.chat_state().clone();
    let weak_window = window.as_weak();
    let ui_ctx_prev = ui_ctx.clone();
    let chat_state = ui_ctx.chat_state().clone();

    window.on_pinned_prev_clicked(move || {
        chat_state_prev_nav.pinned_navigate_prev();
        update_pinned_ui_from_loaded_messages(weak_window.clone(), chat_state_prev_nav.clone(), ui_ctx_prev.clone(), chat_state.active_conversation());
    });

    // Next pinned message
    let chat_state_next_nav = ui_ctx.chat_state().clone();
    let weak_window = window.as_weak();
    let ui_ctx_next = ui_ctx.clone();
    let chat_state = ui_ctx.chat_state().clone();

    window.on_pinned_next_clicked(move || {
        chat_state_next_nav.pinned_navigate_next();
        update_pinned_ui_from_loaded_messages(weak_window.clone(), chat_state_next_nav.clone(), ui_ctx_next.clone(), chat_state.active_conversation());
    });

    // Dismiss pinned bar
    let chat_state_dismiss = ui_ctx.chat_state().clone();
    let weak_window = window.as_weak();

    window.on_pinned_dismiss_clicked(move || {
        chat_state_dismiss.pinned_dismiss();
        if let Some(w) = weak_window.upgrade() {
            w.set_has_pinned_messages(false);
        }
        tracing::debug!("Pinned bar dismissed");
    });

    // Re-show dismissed pinned bar (pin button in header)
    let chat_state_show = ui_ctx.chat_state().clone();
    let weak_window = window.as_weak();
    let ui_ctx_show = ui_ctx.clone();
    let chat_state = ui_ctx.chat_state().clone();

    window.on_pinned_show_clicked(move || {
        if chat_state_show.pinned_total_count() == 0 {
            return;
        }
        // Un-dismiss and reset to first pinned message
        chat_state_show.pinned_navigate_to(0);
        chat_state_show.pinned_undismiss();
        update_pinned_ui_from_loaded_messages(
            weak_window.clone(),
            chat_state_show.clone(),
            ui_ctx_show.clone(),
            chat_state.active_conversation(),
        );
    });

    // Go to pinned message (scroll to it in chat)
    let chat_state_goto = ui_ctx.chat_state().clone();
    let weak_window = window.as_weak();

    window.on_pinned_go_to_message(move || {
        if let Some(message_id) = chat_state_goto.pinned_current_message_id() {
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
    chat_state: chatter_core::state::ChatState,
    ui_ctx: Arc<ChatterClient>,
    conversation: ActiveConversation,
) {
    let state = chat_state.get();
    let current_index = state.pinned.current_index as i32;
    let total_count = state.pinned.pinned_message_ids.len() as i32;
    let current_msg_id = chat_state.pinned_current_message_id();

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
            if let ActiveConversation::AnonGroup(_group_id) = conversation {
                // Private group: look up from unified messages repo
                let anon_id = chatter_core::domain::anon_group::AnonGroupMessageId::from_string(msg_id.to_string());
                ui_ctx.get_anon_group_message(&anon_id)
                    .and_then(|msg| msg.as_text().filter(|t| !t.is_empty()).map(|t| {
                        if t.len() > 80 { format!("{}...", &t[..77]) } else { t.to_string() }
                    }))
                    .unwrap_or_else(|| "[Pinned message]".to_string())
            } else {
                // Regular channel/DM: use PinService
                ui_ctx
                    .get_pinned_message(&msg_id)
                    .map(|d| d.preview)
                    .unwrap_or_else(|| "[Pinned message]".to_string())
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
    let ui_ctx = ui_ctx.clone();

    window.on_chat_settings_clicked(move || {
        let conversation = chat_state.active_conversation();

        if let ActiveConversation::AnonGroup(group_id) = conversation {
            // Private group settings - move DB queries to background thread
            let ui_ctx = ui_ctx.clone();
            let weak = weak_window.clone();

            std::thread::spawn(move || {

                // Get group info for settings modal (DB query in background)
                let group = match ui_ctx.get_anon_group(&group_id) {
                    Some(g) => g,
                    None => {
                        tracing::error!("Private group not found: {}", group_id);
                        return;
                    }
                };

                let members = ui_ctx.get_anon_group_members(&group_id);
                let member_count = members.len() as i32;
                let group_name = group.name.clone();

                // Get current user ID to check if owner
                let current_user_id = match ui_ctx.session_current_user_id() {
                    Some(id) => id,
                    None => return,
                };
                let is_owner = group.is_current_user_owner(&current_user_id);
                let is_admin = group.is_current_user_admin(&current_user_id);

                // Convert members to Slint data
                let mut slint_members = Vec::with_capacity(members.len());
                for member in members {
                    let display_name = if let Some(ref name) = member.display_name {
                        name.clone()
                    } else {
                        ui_ctx.display_name(&member.user_id)
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
            ui_ctx.clone(),
            channel_id,
        );
    });
}

fn wire_invite_member(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let chat_state = ui_ctx.chat_state().clone();
    let ui_ctx = ui_ctx.clone();

    window.on_invite_member(move || {
        let conversation = chat_state.active_conversation();
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();

        match conversation {
            ActiveConversation::AnonGroup(group_id) => {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_invite_private_group_id(group_id.to_string().into());
                        w.set_active_modal("invite-private-group".into());
                    }
                });
            }
            ActiveConversation::Channel(channel_id) => {
                crate::bin_ui::features::modals::open_invite_member_modal(
                    weak, ui_ctx, channel_id,
                );
            }
            _ => {
                tracing::debug!("Invite not available for this conversation type");
            }
        }
    });
}

fn wire_load_more(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let chat_state = ui_ctx.chat_state().clone();
    let ui_ctx_load = ui_ctx.clone();
    let message_state = ui_ctx.message_state().clone();

    window.on_load_more(move || {
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();
        let ui_ctx = ui_ctx_load.clone();
        let message_state = message_state.clone();

        // Load messages in background thread (DB queries are sync)
        std::thread::spawn(move || {
            let state = chat_state.get();
            let channel_id = match state.active_channel_id {
                Some(id) => id,
                None => {
                    tracing::warn!("Cannot load more: no active channel");
                    return;
                }
            };
            // Resolve conversation type via client helper so we pick up
            // the correct variant for encrypted-vs-non-encrypted channels
            // (cheap channel_repo lookup inside).
            let conversation_type = match ui_ctx.active_conversation_type() {
                Some(ct) => ct,
                None => {
                    tracing::warn!("Cannot load more: no active conversation type");
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

            let older_messages = match ui_ctx.get_messages_local_asc(&channel_id, &conversation_type, before.as_ref(), 50) {
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

            // Convert older messages to display data
            let display_messages = messages_to_display_batch(
                &older_messages,
                |uid| ui_ctx.display_name(uid),
                |att_id| ui_ctx.get_attachment(att_id),
            );

            let msg_count = older_messages.len();
            // Get current user ID for is_own calculation
            let current_user_id = ui_ctx.session_current_user_id().map(|id| id.to_string()).unwrap_or_default();

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

        // Close if already open — Rust owns visibility (mirrors ThreadPanel).
        if let Some(w) = weak.upgrade() {
            if w.get_members_panel_visible() {
                w.set_members_panel_visible(false);
                return;
            }
        }

        let conversation = ui_ctx.chat_state().active_conversation();

        tokio::spawn(async move {
            let members = match conversation {
                ActiveConversation::Channel(channel_id) => {
                    match ui_ctx.get_channel_members(&channel_id, 200, 0).await {
                        Ok(memberships) => {
                            memberships.into_iter().map(|m| {
                                let role = format!("{:?}", m.role).to_lowercase();
                                let display_name = ui_ctx.display_name(&m.user_id);
                                let username = ui_ctx.get_user_local(&m.user_id)
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
                    ui_ctx.get_anon_group_members(&group_id).into_iter().map(|m| {
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
                _ => Vec::new(),
            };

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_members_panel_members(
                        ModelRc::new(VecModel::from(members))
                    );
                    w.set_members_panel_visible(true);
                }
            });
        });
    });
}
