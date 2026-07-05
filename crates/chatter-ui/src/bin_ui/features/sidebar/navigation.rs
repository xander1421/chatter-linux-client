//! Navigation callbacks: saved messages, settings, profile, create/join channel, toggle sidebar.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::ChannelId;

use crate::{MainWindow, run_ui};

use super::channel::load_channel_into_ui;
use super::stop_audio_playback;

pub(crate) fn wire_saved_messages(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx_clone = ui_ctx.clone();

    window.on_saved_messages_clicked(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_clone.clone();
        let auth_state = ui_ctx.auth_state().clone();
        let chat_state = ui_ctx.chat_state().clone();

        // Stop any playing audio when switching channels
        stop_audio_playback();

        // Clear inline GIF animations to free memory
        crate::bin_ui::gif_state::clear_inline_gifs();

        let channel_id_str = match auth_state.get().current_user_id {
            Some(id) => id.to_string(),
            None => {
                tracing::warn!("Cannot open saved messages: not logged in");
                return;
            }
        };

        // Commit active-conversation state SYNCHRONOUSLY on the UI thread
        // (same race fix as dm.rs — see comment there).
        if let Ok(parsed) = channel_id_str.parse::<ChannelId>() {
            chat_state.set_active_channel(parsed);
            chat_state.set_active_conversation(chatter_core::state::ActiveConversation::Channel(parsed));
        }

        let _ = channel_id_str.clone();
        run_ui!(weak, |w| {
            w.set_loading_messages(true);
            // Clear composer reply/edit state to prevent stale context
            w.set_composer_is_replying(false);
            w.set_composer_reply_to_sender("".into());
            w.set_composer_reply_to_sender_name("".into());
            w.set_composer_reply_to_timestamp("".into());
            w.set_composer_reply_preview("".into());
            w.set_composer_reply_has_thumbnail(false);
            w.set_composer_reply_thumbnail(slint::Image::default());
            w.set_composer_is_editing(false);
            w.set_composer_edit_message_id("".into());
            // Clear pinned bar immediately
            w.set_has_pinned_messages(false);
            w.set_pinned_message_preview("".into());
            w.set_pinned_total_count(0);
        });

        tokio::spawn(async move {
            let channel_id: ChannelId = match channel_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid user ID for saved messages: {}", e);
                    return;
                }
            };

            // Active conversation already committed pre-spawn.
            let ui_ctx_for_sel = ui_ctx.clone();
            run_ui!(weak, |w| super::refresh_selection(&w, &ui_ctx_for_sel));

            // Ensure the saved messages channel exists locally
            if ui_ctx.get_channel_local(&channel_id).is_none() {
                if let Err(e) = ui_ctx.get_channel(&channel_id).await {
                    tracing::error!("Failed to fetch saved messages channel from server: {}", e);
                    run_ui!(weak, |w| w.set_loading_messages(false));
                    return;
                }
            }

            // Load the saved messages channel and its messages
            load_channel_into_ui(
                weak,
                channel_id,
                ui_ctx,
            ).await;
        });
    });
}

pub(crate) fn wire_settings_clicked(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let auth_state = ui_ctx.auth_state().clone();
    let privacy_state = ui_ctx.privacy_state().clone();
    let ui_ctx_fp = ui_ctx.clone();

    let weak_window = window.as_weak();

    window.on_settings_clicked(move || {
        let weak = weak_window.clone();
        let auth_state = auth_state.clone();
        let privacy_state = privacy_state.clone();
        let ui_ctx_fp = ui_ctx_fp.clone();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let state = auth_state.get();
                w.set_settings_display_name(state.display_name.unwrap_or_default().into());
                w.set_settings_username(
                    state
                        .current_user_id
                        .map(|id| id.to_string())
                        .unwrap_or_default()
                        .into(),
                );

                // Load privacy settings from state
                let privacy = privacy_state.get();
                w.set_settings_read_receipts(privacy.global_read_receipts);
                w.set_settings_typing_indicators(privacy.global_typing_indicators);

                w.set_active_modal("settings".into());
            }
        });

        // Fetch identity fingerprint asynchronously and set it on the UI
        let weak_fp = weak_window.clone();
        tokio::spawn(async move {
            match ui_ctx_fp.my_identity_fingerprint().await {
                Ok(fingerprint) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_fp.upgrade() {
                            w.set_settings_identity_fingerprint(fingerprint.into());
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to load identity fingerprint for settings: {}", e);
                }
            }
        });
    });
}

pub(crate) fn wire_profile_clicked(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let auth_state = ui_ctx.auth_state().clone();
    let user_state = ui_ctx.user_state().clone();
    let ui_ctx_profile = ui_ctx.clone();

    let weak_window = window.as_weak();

    window.on_profile_clicked(move || {
        let weak = weak_window.clone();
        let auth_state = auth_state.clone();
        let user_state = user_state.clone();
        let ui_ctx = ui_ctx_profile.clone();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                if let Some(user_id) = auth_state.get().current_user_id {
                    crate::bin_ui::features::modals::open_user_profile(
                        &w, &user_id, &auth_state, &user_state, Some(&ui_ctx),
                    );
                }
            }
        });
    });
}

pub(crate) fn wire_create_join_channel(window: &MainWindow) {
    // Open create channel modal (just logs for now, modal is opened via active-modal in Slint)
    window.on_open_create_channel(|| {
        tracing::debug!("Opening create channel modal");
    });

    // Open join channel modal (just logs for now, modal is opened via active-modal in Slint)
    window.on_open_join_channel(|| {
        tracing::debug!("Opening join channel modal");
    });
}

pub(crate) fn wire_toggle_sidebar(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();

    window.on_toggle_sidebar(move || {
        let ui_ctx = ui_ctx.clone();
        let weak = weak_window.clone();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                ui_ctx.set_sidebar_collapsed(w.get_sidebar_collapsed());
            }
        });
    });
}

pub(crate) fn load_sidebar_collapsed_state(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    window.set_sidebar_collapsed(ui_ctx.sidebar_collapsed());
}

/// Wire message requests click handler.
///
/// Opens the friends modal with the "requests" tab and "private" sub-tab selected.
/// Refreshes both the badge count and the private requests list in the modal.
pub(crate) fn wire_message_requests_clicked(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();

    window.on_message_requests_clicked(move || {
        let ui_ctx = ui_ctx.clone();
        let weak = weak_window.clone();

        // Refresh the pending requests list and count when clicked
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let requests = ui_ctx.list_anon_dm_message_requests();
                let count = requests.len() as i32;

                // Build UI data for private requests
                let private_requests: Vec<crate::FriendRequestData> = requests
                    .iter()
                    .map(|dm| {
                        // Use stored display name, not user_service (strangers)
                        let peer_name = dm.peer_display_name.clone()
                            .unwrap_or_else(|| ui_ctx.fallback_display_name(&dm.peer_id));

                        crate::FriendRequestData {
                            user_id: dm.id.to_string().into(),
                            display_name: peer_name.into(),
                            username: "Private".into(),
                            avatar_source: slint::Image::default(),
                            has_avatar: false,
                            sent_at: dm.created_at
                                .with_timezone(&chrono::Local)
                                .format("%b %d, %H:%M")
                                .to_string()
                                .into(),
                            note: "".into(),
                        }
                    })
                    .collect();

                tracing::info!(
                    count = count,
                    "Message requests clicked - refreshed {} requests",
                    count
                );

                // Update both the badge count and the requests list
                w.set_pending_message_requests(count);
                w.set_friends_private_requests(slint::ModelRc::new(slint::VecModel::from(private_requests)));
            }
        });
    });
}

/// Load pending message requests count and list into the sidebar.
pub(crate) fn load_pending_message_requests_count(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let requests = ui_ctx.list_anon_dm_message_requests();
    {
            let count = requests.len() as i32;

            // Build UI data for private requests
            let private_requests: Vec<crate::FriendRequestData> = requests
                .iter()
                .map(|dm| {
                    // Use stored display name, not user_service (strangers)
                    let peer_name = dm.peer_display_name.clone()
                        .unwrap_or_else(|| ui_ctx.fallback_display_name(&dm.peer_id));

                    crate::FriendRequestData {
                        user_id: dm.id.to_string().into(),
                        display_name: peer_name.into(),
                        username: "Private".into(),
                        avatar_source: slint::Image::default(),
                        has_avatar: false,
                        sent_at: dm.created_at
                            .with_timezone(&chrono::Local)
                            .format("%b %d, %H:%M")
                            .to_string()
                            .into(),
                        note: "".into(),
                    }
                })
                .collect();

            tracing::info!(
                count = count,
                "Loaded {} pending message requests",
                count
            );

            window.set_pending_message_requests(count);
            window.set_friends_private_requests(slint::ModelRc::new(slint::VecModel::from(private_requests)));
    }
}

/// Wire group invites click handler.
///
/// Opens the friends modal with the "requests" tab and "groups" sub-tab selected.
/// Refreshes both the badge count and the group invites list in the modal.
pub(crate) fn wire_group_invites_clicked(window: &MainWindow, _ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();

    window.on_group_invites_clicked(move || {
        let weak = weak_window.clone();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                // Refresh count from the pending invites store
                let count = crate::bin_ui::features::modals::pending_group_invite_count();
                tracing::info!(count = count, "Group invites clicked - {} pending", count);
                w.set_pending_group_invites(count);
            }
        });
    });
}

/// Load pending group invites count into the sidebar.
pub(crate) fn load_pending_group_invites_count(window: &MainWindow, _ui_ctx: &Arc<ChatterClient>) {
    let count = crate::bin_ui::features::modals::pending_group_invite_count();
    tracing::info!(count = count, "Loaded {} pending group invites", count);
    window.set_pending_group_invites(count);
}
