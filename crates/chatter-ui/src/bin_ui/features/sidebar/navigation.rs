//! Navigation callbacks: saved messages, settings, profile, create/join channel, toggle sidebar.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::ChannelId;

use crate::MainWindow;

use super::channel::load_channel_into_ui_owned;
use super::stop_audio_playback;

pub(crate) fn wire_saved_messages(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx_clone = ui_ctx.clone();

    window.on_saved_messages_clicked(move || {
        let weak = weak_window.clone();
        super::show_chat_pane(&weak);
        let ui_ctx = ui_ctx_clone.clone();
        let auth_state = ui_ctx.auth_state().clone();
        let chat_state = ui_ctx.chat_state().clone();

        // Stop any playing audio when switching channels
        stop_audio_playback();

        // Clear inline GIF animations to free memory
        crate::bin_ui::gif_state::clear_inline_gifs();

        // Saved Messages is the channel whose id is the user's own id.
        let channel_id = match auth_state.get().current_user_id {
            Some(id) => ChannelId::from(id),
            None => {
                tracing::warn!("Cannot open saved messages: not logged in");
                return;
            }
        };

        // Commit active-conversation state SYNCHRONOUSLY on the UI thread
        // (same race fix as dm.rs — see comment there).
        chat_state.set_active_channel(channel_id);
        chat_state
            .set_active_conversation(chatter_core::state::ActiveConversation::Channel(channel_id));
        if let Some(w) = weak.upgrade() {
            w.invoke_refresh_screen_protection();
        }

        if let Some(w) = weak.upgrade() {
            crate::bin_ui::features::composer::sync_composer_draft(
                &w,
                &ui_ctx,
                ui_ctx.chat_state().get().attachment_revision,
            );
        }

        let owner = crate::bin_ui::features::history_owner::HistoryOwner::begin(
            &ui_ctx,
            chatter_core::state::ActiveConversation::Channel(channel_id),
        );
        crate::run_history_ui!(weak, owner, |w| {
            w.set_loading_messages(true);

            // Clear pinned bar immediately
            w.set_has_pinned_messages(false);
            w.set_pinned_message_preview("".into());
            w.set_pinned_total_count(0);
        });

        tokio::spawn(async move {
            if !owner.is_current() {
                return;
            }
            // Active conversation already committed pre-spawn.
            let ui_ctx_for_sel = ui_ctx.clone();
            crate::run_history_ui!(weak, owner, |w| super::refresh_selection(
                &w,
                &ui_ctx_for_sel
            ));

            // Ensure the saved messages channel exists locally
            if ui_ctx.get_channel_local(&channel_id).is_none() {
                if let Err(e) = ui_ctx.get_channel(&channel_id).await {
                    tracing::error!("Failed to fetch saved messages channel from server: {}", e);
                    crate::run_history_ui!(weak, owner, |w| w.set_loading_messages(false));
                    return;
                }
            }

            // Load the saved messages channel and its messages
            // Saved Messages is your own notepad: nothing in it is ever
            // "unread", so it always opens at the newest entry.
            load_channel_into_ui_owned(
                weak,
                channel_id,
                ui_ctx,
                Default::default(),
                None,
                owner,
                None,
            )
            .await;
        });
    });
}

pub(crate) fn wire_settings_clicked(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let auth_state = ui_ctx.auth_state().clone();
    let ui_ctx_fp = ui_ctx.clone();
    let ui_ctx_ai = ui_ctx.clone();

    let weak_window = window.as_weak();

    window.on_settings_clicked(move || {
        let weak = weak_window.clone();
        let auth_state = auth_state.clone();
        let ui_ctx_fp = ui_ctx_fp.clone();
        let ui_ctx_settings = ui_ctx_ai.clone();

        // The Active Sessions badge; otherwise it reads 0 until "View All".
        let ctx = ui_ctx_ai.clone();
        let weak_count = weak.clone();
        tokio::spawn(async move {
            if let Ok(sessions) = ctx.list_sessions().await {
                let n = sessions.len() as i32;
                let weak_count = weak_count.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_count.upgrade() {
                        w.set_settings_session_count(n);
                    }
                });
            }
            // Key Backup card status; "unknown" only if the check itself fails.
            let status = match ctx.has_key_backup().await {
                Ok(true) => "backed_up",
                Ok(false) => "not_backed_up",
                Err(_) => "unknown",
            };
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_count.upgrade() {
                    w.set_settings_backup_status(status.into());
                }
            });
        });

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let state = auth_state.get();
                w.set_settings_display_name(state.display_name.unwrap_or_default().into());
                w.set_settings_username(ui_ctx_settings.full_username().unwrap_or_default().into());

                // Read from the same settings the toggles write.
                w.set_settings_read_receipts(
                    ui_ctx_settings.read_receipts_enabled().unwrap_or(true),
                );
                w.set_settings_typing_indicators(
                    ui_ctx_settings.typing_indicators_enabled().unwrap_or(true),
                );
                w.set_settings_notification_detail(
                    ui_ctx_settings.notification_detail_level().as_str().into(),
                );
                w.set_settings_profile_visibility(
                    ui_ctx_settings.profile_visibility().as_str().into(),
                );
                w.set_settings_profile_visibility_pending("".into());
                w.set_settings_profile_visibility_error("".into());

                // Cloud AI: reflect what is actually saved, so the panel never
                // shows a preference the core would not honour.
                use chatter_core::ai_policy::channel_ai::SiloPreference;
                let cloud = ui_ctx_settings.cloud_ai_settings();
                w.set_settings_ai_prefer_cloud(
                    ui_ctx_settings.ai_silo_preference() == SiloPreference::PreferCloud,
                );
                w.set_settings_cloud_ai_mode(cloud.mode.as_str().into());
                w.set_settings_cloud_ai_configured(cloud.is_available());
                // Never echo the stored key back into the field.
                w.set_settings_cloud_ai_api_key("".into());
            }
        });

        // Fetch identity fingerprint asynchronously and set it on the UI
        let weak_fp = weak_window.clone();
        let ui_ctx_policy = ui_ctx_fp.clone();
        let weak_policy = weak_window.clone();
        let ui_ctx_discover = ui_ctx_fp.clone();
        let weak_discover = weak_window.clone();
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

        // Fetch the sealed-sender RECEIVING policy from the server so the toggle
        // reflects users.unrestricted_unidentified_access (not a stale local bool).
        tokio::spawn(async move {
            match ui_ctx_policy.get_sealed_sender_policy().await {
                Ok(allow_from_anyone) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_policy.upgrade() {
                            w.set_settings_sealed_sender_unrestricted(allow_from_anyone);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to load sealed sender policy for settings: {}", e);
                }
            }
        });

        // Fetch "Who can find me by my username" from the server — it is
        // stored there (user_profiles.username_discoverable), not locally.
        tokio::spawn(async move {
            match ui_ctx_discover.get_username_discoverability().await {
                Ok(discoverable) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_discover.upgrade() {
                            w.set_settings_username_discoverable(discoverable);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to load username discoverability for settings: {}",
                        e
                    );
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
                        &w,
                        &user_id,
                        &auth_state,
                        &user_state,
                        Some(&ui_ctx),
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
                        let peer_name = dm
                            .peer_display_name
                            .clone()
                            .unwrap_or_else(|| ui_ctx.fallback_display_name(&dm.peer_id));

                        crate::FriendRequestData {
                            user_id: dm.id.to_string().into(),
                            display_name: peer_name.into(),
                            username: "Private".into(),
                            avatar_source: slint::Image::default(),
                            has_avatar: false,
                            sent_at: dm
                                .created_at
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
                w.set_friends_private_requests(slint::ModelRc::new(slint::VecModel::from(
                    private_requests,
                )));
            }
        });
    });
}

/// Load pending message requests count and list into the sidebar.
pub(crate) fn load_pending_message_requests_count(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
) {
    let requests = ui_ctx.list_anon_dm_message_requests();
    {
        let count = requests.len() as i32;

        // Build UI data for private requests
        let private_requests: Vec<crate::FriendRequestData> = requests
            .iter()
            .map(|dm| {
                // Use stored display name, not user_service (strangers)
                let peer_name = dm
                    .peer_display_name
                    .clone()
                    .unwrap_or_else(|| ui_ctx.fallback_display_name(&dm.peer_id));

                crate::FriendRequestData {
                    user_id: dm.id.to_string().into(),
                    display_name: peer_name.into(),
                    username: "Private".into(),
                    avatar_source: slint::Image::default(),
                    has_avatar: false,
                    sent_at: dm
                        .created_at
                        .with_timezone(&chrono::Local)
                        .format("%b %d, %H:%M")
                        .to_string()
                        .into(),
                    note: "".into(),
                }
            })
            .collect();

        tracing::info!(count = count, "Loaded {} pending message requests", count);

        window.set_pending_message_requests(count);
        window.set_friends_private_requests(slint::ModelRc::new(slint::VecModel::from(
            private_requests,
        )));
    }
}

/// Wire group invites click handler.
///
/// Opens the friends modal with the "requests" tab and "groups" sub-tab selected.
/// Refreshes both the badge count and the group invites list in the modal.
pub(crate) fn wire_group_invites_clicked(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_group_invites_clicked(move || {
        crate::bin_ui::features::modals::refresh_group_invites(&weak_window, &ui_ctx);
    });
}

/// Load pending group invites count into the sidebar.
pub(crate) fn load_pending_group_invites_count(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    crate::bin_ui::features::modals::refresh_group_invites(&window.as_weak(), ui_ctx);
}
