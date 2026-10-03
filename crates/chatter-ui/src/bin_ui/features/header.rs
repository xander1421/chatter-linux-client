//! Header callbacks.
//!
//! Handles channel header actions (search, pinned, settings, load more).

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::MessageId;
use chatter_core::state::ActiveConversation;

use crate::bin_ui::converters::messages_to_display_batch;
use crate::MainWindow;

/// Wire header callbacks.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_pinned_navigation(window, ui_ctx);
    wire_chat_settings_clicked(window, ui_ctx);
    wire_invite_member(window, ui_ctx);
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
    let ui_ctx_goto = ui_ctx.clone();

    window.on_pinned_go_to_message(move || {
        if let (Some(message_id), Some(w)) =
            (chat_state_goto.pinned_current_message_id(), weak_window.upgrade())
        {
            jump_to_message(&w, &ui_ctx_goto, &message_id.to_string());
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
        let preview = match current_msg_id {
            Some(msg_id) => ui_ctx.pinned_message_preview(&conversation, &msg_id),
            None => String::new(),
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
                // Shared with the live refresh on membership events
                // (`refresh_members_if_open`), so both read the same roster.
                let Some(roster) = super::modals::load_private_group_roster(&ui_ctx, group_id) else {
                    return;
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_channel_settings_id(group_id.to_string().into());
                        w.set_channel_settings_name(roster.name.as_str().into());
                        w.set_channel_settings_type("private-group".into());
                        super::modals::apply_private_group_roster(&w, roster);
                        // Clear a previous channel's ban list.
                        w.set_channel_settings_can_view_bans(false);
                        w.set_channel_settings_banned_members(ModelRc::default());
                        crate::bin_ui::features::back::push(&w, "channel-settings");
                        tracing::debug!("Opened settings for private group {}", group_id);
                    }
                });
            });
            return;
        }

        // A DM's settings screen is the peer's profile — block, verify, safety
        // number, remove friend all live there, the same way Telegram hangs a
        // DM's settings off the contact page. The kebab used to route to
        // channel settings instead, which for a channel-free (Signal-style)
        // server DM missed in `get_channel_local` and silently did nothing:
        // the button was on screen in every DM and had never worked.
        if let ActiveConversation::ServerDm { peer_id } = conversation {
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx.clone();
            let auth_state = ui_ctx.auth_state().clone();
            let user_state = ui_ctx.user_state().clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    super::modals::open_user_profile(
                        &w, &peer_id, &auth_state, &user_state, Some(&ui_ctx),
                    );
                    crate::bin_ui::features::back::push(&w, "user-profile");
                }
            });
            return;
        }

        // Regular channel settings
        let channel_id = match conversation {
            ActiveConversation::Channel(id) => id,
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
                        crate::bin_ui::features::back::push(&w, "invite-private-group");
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
    let ui_ctx = ui_ctx.clone();

    window.on_load_more(move || {
        if let Some(w) = weak_window.upgrade() {
            load_older(&w, &ui_ctx, None);
        }
    });
}

/// Row of the open list holding `message_id`.
fn row_of(w: &MainWindow, message_id: &str) -> Option<usize> {
    let messages = w.get_messages();
    (0..messages.row_count())
        .find(|&i| messages.row_data(i).is_some_and(|m| m.message_id.as_str() == message_id))
}

/// Scroll the open conversation to `message_id` and flash it: a quote's
/// parent, a search hit, the pinned message. One that is older than what is
/// loaded has the history back to it loaded first. UI thread.
pub(crate) fn jump_to_message(w: &MainWindow, ui_ctx: &Arc<ChatterClient>, message_id: &str) {
    let Some(row) = row_of(w, message_id) else {
        match message_id.parse() {
            Ok(id) => load_older(w, ui_ctx, Some(id)),
            Err(_) => tracing::debug!(message_id, "Jump target is not a stored message"),
        }
        return;
    };
    w.set_scroll_to_message_index(row as i32);
    w.set_highlighted_message_id(message_id.into());

    // A brief pulse, not a permanent highlight. The index goes back with it:
    // the list acts on it changing, so left in place a second jump to the
    // same row would do nothing.
    let weak = w.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(1500), move || {
        if let Some(w) = weak.upgrade() {
            w.set_highlighted_message_id(slint::SharedString::new());
            w.set_scroll_to_message_index(-1);
        }
    });
}

/// Load history older than the oldest row of the open list and insert it
/// above: one page, or with `until` every page back to that message, which
/// is then jumped to. The oldest loaded row is the cursor — the list itself,
/// not a copy of where it was last time. UI thread.
fn load_older(w: &MainWindow, ui_ctx: &Arc<ChatterClient>, until: Option<MessageId>) {
    let Some(before) = w
        .get_messages()
        .row_data(0)
        .and_then(|m| m.message_id.parse::<MessageId>().ok())
    else {
        return;
    };
    let weak = w.as_weak();
    let ui_ctx = ui_ctx.clone();

    // Load messages in background thread (DB queries are sync)
    std::thread::spawn(move || {
        let Some(channel_id) = ui_ctx.chat_state().get().active_channel_id else {
            tracing::warn!("Cannot load more: no active channel");
            return;
        };
        // Resolve conversation type via client helper so we pick up
        // the correct variant for encrypted-vs-non-encrypted channels
        // (cheap channel_repo lookup inside).
        let Some(conversation_type) = ui_ctx.active_conversation_type() else {
            tracing::warn!("Cannot load more: no active conversation type");
            return;
        };

        let message_state = ui_ctx.message_state();
        if message_state.is_loading(&channel_id) {
            tracing::trace!("Already loading messages for channel: {}", channel_id);
            return;
        }
        message_state.set_loading(channel_id, true);
        let older_messages = ui_ctx.get_older_messages_local_asc(
            &channel_id,
            &conversation_type,
            &before,
            until.as_ref(),
        );
        message_state.set_loading(channel_id, false);
        let older_messages = match older_messages {
            Ok(msgs) => msgs,
            Err(e) => {
                tracing::error!("Failed to load more messages: {}", e);
                return;
            }
        };
        if older_messages.is_empty() {
            tracing::debug!("No older messages for channel: {}", channel_id);
            return;
        }

        // Convert older messages to display data
        let mut display_messages = messages_to_display_batch(
            &older_messages,
            |m| ui_ctx.message_sender_display_name(m),
            |m| ui_ctx.message_sender_avatar(m),
            |att_id| ui_ctx.get_attachment(att_id),
            |m| ui_ctx.reply_author_display_name(m),
            |m| ui_ctx.reply_parent_missing_for(m),
            |m| ui_ctx.reply_parent_media(m),
        );
        crate::bin_ui::converters::apply_reply_counts(
            &mut display_messages,
            &ui_ctx.thread_reply_counts(&channel_id, &conversation_type),
        );

        // Get current user ID for is_own calculation
        let current_user_id = ui_ctx.session_current_user_id().map(|id| id.to_string()).unwrap_or_default();

        let _ = slint::invoke_from_event_loop(move || {
            let Some(w) = weak.upgrade() else { return };
            // The conversation was switched while this loaded.
            if ui_ctx.chat_state().get().active_channel_id != Some(channel_id) {
                return;
            }
            // Convert display data to Slint types (must be done on UI thread)
            let slint_messages: Vec<_> = display_messages.iter().map(|d| d.to_slint_with_user(&current_user_id)).collect();

            let current_model = w.get_messages();
            let current_ids: std::collections::HashSet<String> = (0..current_model.row_count())
                .filter_map(|i| current_model.row_data(i))
                .map(|msg| msg.message_id.to_string())
                .collect();

            // Prepend older messages (older go at top), skipping
            // duplicates. Inserted in place: rows added above the
            // first visible one shift the list's index without
            // moving what is on screen, so history loads without
            // a jump.
            let older: Vec<_> = slint_messages
                .into_iter()
                .filter(|msg| !current_ids.contains(msg.message_id.as_str()))
                .collect();
            tracing::debug!("Loaded {} older messages for channel", older.len());
            crate::bin_ui::features::chat::edit_messages(&w, |model| {
                for (i, msg) in older.into_iter().enumerate() {
                    model.insert(i, msg);
                }
            });
            if let Some(target) = until.map(|id| id.to_string()) {
                if row_of(&w, &target).is_some() {
                    jump_to_message(&w, &ui_ctx, &target);
                }
            }
        });
    });
}
