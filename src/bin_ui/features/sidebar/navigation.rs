//! Navigation callbacks: saved messages, settings, profile, create/join channel, toggle sidebar.

use std::sync::Arc;

use slint::{ComponentHandle, SharedString};

use chatter_linux::app::UIContext;
use chatter_linux::domain::ids::ChannelId;

use crate::MainWindow;

use super::channel::load_channel_into_ui;
use super::stop_audio_playback;

pub(crate) fn wire_saved_messages(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let weak_window = window.as_weak();
    let auth_state = ui_ctx.auth_state.clone();
    let chat_state = ui_ctx.chat_state.clone();
    let channel_service = ui_ctx.channel_service.clone();
    let message_service = ui_ctx.message_service.clone();
    let user_service = ui_ctx.user_service.clone();
    let attachment_service = ui_ctx.attachment_service.clone();

    window.on_saved_messages_clicked(move || {
        let weak = weak_window.clone();
        let auth_state = auth_state.clone();
        let chat_state = chat_state.clone();
        let channel_service = channel_service.clone();
        let message_service = message_service.clone();
        let user_service = user_service.clone();
        let attachment_service = attachment_service.clone();

        // Stop any playing audio when switching channels
        stop_audio_playback();

        // Clear inline GIF animations to free memory
        crate::bin_ui::gif_state::clear_inline_gifs();

        tokio::spawn(async move {
            // Get current user ID - saved messages channel_id = user_id
            let user_id = match auth_state.get().current_user_id {
                Some(id) => id,
                None => {
                    tracing::warn!("Cannot open saved messages: not logged in");
                    return;
                }
            };

            // The saved messages channel ID is the same as the user ID
            let channel_id: ChannelId = user_id.into();

            // Set loading state
            let weak_loading = weak.clone();
            let channel_id_str: SharedString = channel_id.to_string().into();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_loading_messages(true);
                    w.set_selected_channel_id(channel_id_str);
                    w.set_selected_dm_id("".into());
                }
            });

            // Update chat state with active channel
            chat_state.set_active_channel(channel_id);

            // Load the saved messages channel and its messages
            let current_user_id = Some(user_id.to_string());
            load_channel_into_ui(
                weak,
                &channel_id,
                channel_service,
                message_service,
                user_service,
                attachment_service,
                current_user_id,
            ).await;
        });
    });
}

pub(crate) fn wire_settings_clicked(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let auth_state = ui_ctx.auth_state.clone();
    let privacy_state = ui_ctx.privacy_state.clone();

    let weak_window = window.as_weak();

    window.on_settings_clicked(move || {
        let weak = weak_window.clone();
        let auth_state = auth_state.clone();
        let privacy_state = privacy_state.clone();

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
    });
}

pub(crate) fn wire_profile_clicked(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let auth_state = ui_ctx.auth_state.clone();

    let weak_window = window.as_weak();

    window.on_profile_clicked(move || {
        let weak = weak_window.clone();
        let auth_state = auth_state.clone();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let state = auth_state.get();

                w.set_profile_user_id(
                    state
                        .current_user_id
                        .map(|id| id.to_string())
                        .unwrap_or_default()
                        .into(),
                );
                w.set_profile_display_name(state.display_name.clone().unwrap_or_default().into());
                w.set_profile_username(
                    state
                        .current_user_id
                        .map(|id| id.to_string())
                        .unwrap_or_default()
                        .into(),
                );
                w.set_profile_is_self(true);
                w.set_profile_is_friend(false);
                w.set_profile_is_blocked(false);
                w.set_profile_presence("online".into());

                w.set_active_modal("user-profile".into());
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

pub(crate) fn wire_toggle_sidebar(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let ui_state = ui_ctx.ui_state.clone();
    let weak_window = window.as_weak();

    window.on_toggle_sidebar(move || {
        let ui_state = ui_state.clone();
        let weak = weak_window.clone();

        // Get the current collapsed state and persist it via UI state
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let collapsed = w.get_sidebar_collapsed();
                // Persist via UI state (which handles storage internally)
                ui_state.set_sidebar_collapsed(collapsed);
            }
        });
    });
}

pub(crate) fn load_sidebar_collapsed_state(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    // Load saved collapsed state from UI state on startup
    let collapsed = ui_ctx.ui_state.get().sidebar_collapsed;
    window.set_sidebar_collapsed(collapsed);
}

/// Wire message requests click handler.
///
/// Opens the friends modal with the "requests" tab and "private" sub-tab selected.
/// Refreshes both the badge count and the private requests list in the modal.
pub(crate) fn wire_message_requests_clicked(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let anon_dm_service = ui_ctx.anon_dm_service.clone();
    let weak_window = window.as_weak();

    window.on_message_requests_clicked(move || {
        let anon_dm_service = anon_dm_service.clone();
        let weak = weak_window.clone();

        // Refresh the pending requests list and count when clicked
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                // Get fresh requests from service
                match anon_dm_service.list_message_requests() {
                    Ok(requests) => {
                        let count = requests.len() as i32;

                        // Build UI data for private requests
                        let private_requests: Vec<crate::FriendRequestData> = requests
                            .iter()
                            .map(|dm| {
                                // Use stored display name, not user_service (strangers)
                                let peer_name = dm.peer_display_name.clone()
                                    .unwrap_or_else(|| format!("User {}", &dm.peer_id.to_string()[..8]));

                                crate::FriendRequestData {
                                    user_id: dm.id.to_string().into(),
                                    display_name: peer_name.into(),
                                    username: "Private".into(),
                                    avatar_url: "".into(),
                                    avatar_source: slint::Image::default(),
                                    has_avatar: false,
                                    sent_at: dm.created_at
                                        .with_timezone(&chrono::Local)
                                        .format("%b %d, %H:%M")
                                        .to_string()
                                        .into(),
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
                    Err(e) => {
                        tracing::warn!("Failed to refresh message requests: {}", e);
                        w.set_pending_message_requests(0);
                    }
                }
            }
        });
    });
}

/// Load pending message requests count and list into the sidebar.
pub(crate) fn load_pending_message_requests_count(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    match ui_ctx.anon_dm_service.list_message_requests() {
        Ok(requests) => {
            let count = requests.len() as i32;

            // Build UI data for private requests
            let private_requests: Vec<crate::FriendRequestData> = requests
                .iter()
                .map(|dm| {
                    // Use stored display name, not user_service (strangers)
                    let peer_name = dm.peer_display_name.clone()
                        .unwrap_or_else(|| format!("User {}", &dm.peer_id.to_string()[..8]));

                    crate::FriendRequestData {
                        user_id: dm.id.to_string().into(),
                        display_name: peer_name.into(),
                        username: "Private".into(),
                        avatar_url: "".into(),
                        avatar_source: slint::Image::default(),
                        has_avatar: false,
                        sent_at: dm.created_at
                            .with_timezone(&chrono::Local)
                            .format("%b %d, %H:%M")
                            .to_string()
                            .into(),
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
        Err(e) => {
            tracing::warn!("Failed to load message requests: {}", e);
            window.set_pending_message_requests(0);
        }
    }
}
