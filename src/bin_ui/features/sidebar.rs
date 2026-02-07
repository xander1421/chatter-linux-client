//! Sidebar callbacks.
//!
//! Handles channel/DM selection, settings, and profile.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use chatter_linux::app::AppContext;
use chatter_linux::domain::ids::ChannelId;
use chatter_linux::storage::{ChannelRepository, MessageRepository, UserRepository};

use crate::bin_ui::converters::{channel_type_to_string, message_to_slint};
use crate::{MainWindow, MessageItemData};

/// Wire sidebar callbacks (channel/DM selection, settings, profile).
pub fn wire(window: &MainWindow, ctx: &AppContext) {
    wire_channel_selected(window, ctx);
    wire_dm_selected(window, ctx);
    wire_saved_messages(window, ctx);
    wire_settings_clicked(window, ctx);
    wire_profile_clicked(window, ctx);
}

fn wire_channel_selected(window: &MainWindow, ctx: &AppContext) {
    let chat_state = ctx.chat_state.clone();
    let channel_repo = ctx.channel_repo.clone();
    let message_repo = ctx.message_repo.clone();
    let user_repo = ctx.user_repo.clone();

    let weak_window = window.as_weak();

    window.on_channel_selected(move |channel_id: SharedString| {
        let channel_id_str = channel_id.to_string();
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();
        let channel_repo = channel_repo.clone();
        let message_repo = message_repo.clone();
        let user_repo = user_repo.clone();

        tokio::spawn(async move {
            // Set loading state
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_loading_messages(true);
                    w.set_selected_channel_id(channel_id.clone());
                    w.set_selected_dm_id("".into());
                }
            });

            // Parse channel ID
            let channel_id: ChannelId = match channel_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid channel ID: {}", e);
                    return;
                }
            };

            // Update chat state
            chat_state.set_active_channel(channel_id);

            // Load channel and messages
            load_channel_into_ui(weak, &channel_id, channel_repo, message_repo, user_repo).await;
        });
    });
}

fn wire_dm_selected(window: &MainWindow, ctx: &AppContext) {
    let chat_state = ctx.chat_state.clone();
    let channel_repo = ctx.channel_repo.clone();
    let message_repo = ctx.message_repo.clone();
    let user_repo = ctx.user_repo.clone();

    let weak_window = window.as_weak();

    window.on_dm_selected(move |dm_id: SharedString| {
        let dm_id_str = dm_id.to_string();
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();
        let channel_repo = channel_repo.clone();
        let message_repo = message_repo.clone();
        let user_repo = user_repo.clone();

        tokio::spawn(async move {
            // Set loading state
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_loading_messages(true);
                    w.set_selected_dm_id(dm_id.clone());
                    w.set_selected_channel_id("".into());
                }
            });

            // Parse channel ID (DMs are still channels)
            let channel_id: ChannelId = match dm_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid DM ID: {}", e);
                    return;
                }
            };

            // Update chat state
            chat_state.set_active_channel(channel_id);

            // Load channel and messages
            load_channel_into_ui(weak, &channel_id, channel_repo, message_repo, user_repo).await;
        });
    });
}

fn wire_saved_messages(window: &MainWindow, _ctx: &AppContext) {
    let weak_window = window.as_weak();

    window.on_saved_messages_clicked(move || {
        let weak = weak_window.clone();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_channel_name("Saved Messages".into());
                w.set_channel_type("saved".into());
                w.set_selected_channel_id("".into());
                w.set_selected_dm_id("".into());
                let empty_messages: Vec<MessageItemData> = Vec::new();
                w.set_messages(ModelRc::new(VecModel::from(empty_messages)));
            }
        });
    });
}

fn wire_settings_clicked(window: &MainWindow, ctx: &AppContext) {
    let auth_state = ctx.auth_state.clone();
    let settings_repo = ctx.settings_repo.clone();

    let weak_window = window.as_weak();

    window.on_settings_clicked(move || {
        let weak = weak_window.clone();
        let auth_state = auth_state.clone();
        let settings_repo = settings_repo.clone();

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

                // Load privacy settings
                if let Ok(Some(read_receipts)) =
                    settings_repo.get::<bool>(chatter_linux::storage::settings_keys::SEND_READ_RECEIPTS)
                {
                    w.set_settings_read_receipts(read_receipts);
                }
                if let Ok(Some(typing_indicators)) =
                    settings_repo.get::<bool>(chatter_linux::storage::settings_keys::SHOW_TYPING_INDICATORS)
                {
                    w.set_settings_typing_indicators(typing_indicators);
                }

                w.set_active_modal("settings".into());
            }
        });
    });
}

fn wire_profile_clicked(window: &MainWindow, ctx: &AppContext) {
    let auth_state = ctx.auth_state.clone();

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

/// Load channel data and messages into UI properties.
pub async fn load_channel_into_ui(
    window_weak: slint::Weak<MainWindow>,
    channel_id: &ChannelId,
    channel_repo: Arc<ChannelRepository>,
    message_repo: Arc<MessageRepository>,
    user_repo: Arc<UserRepository>,
) {
    // Get channel info from local repo
    let channel = match channel_repo.get(channel_id) {
        Ok(Some(ch)) => ch,
        Ok(None) => {
            tracing::warn!("Channel {} not found in local repo", channel_id);
            let weak = window_weak;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_loading_messages(false);
                }
            });
            return;
        }
        Err(e) => {
            tracing::error!("Failed to get channel {}: {}", channel_id, e);
            let weak = window_weak;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_loading_messages(false);
                }
            });
            return;
        }
    };

    // Fetch recent messages (limit 50)
    let messages = match message_repo.list_for_channel(channel_id, None, 50) {
        Ok(msgs) => msgs,
        Err(e) => {
            tracing::error!("Failed to get messages for channel {}: {}", channel_id, e);
            Vec::new()
        }
    };

    // Convert messages to Slint model
    let mut slint_messages: Vec<MessageItemData> = Vec::with_capacity(messages.len());
    for msg in &messages {
        let sender_name = if let Some(sender_id) = &msg.sender_id {
            user_repo
                .get_profile(sender_id)
                .ok()
                .flatten()
                .map(|p| p.display_name)
                .or_else(|| user_repo.get(sender_id).ok().flatten().map(|u| u.username))
                .unwrap_or_else(|| "Unknown".to_string())
        } else {
            "Anonymous".to_string()
        };
        slint_messages.push(message_to_slint(msg, &sender_name));
    }

    // Update UI
    let channel_name = channel.name.clone();
    let channel_type = channel_type_to_string(channel.channel_type);
    let member_count = 0i32;
    let topic = channel.description.clone().unwrap_or_default();
    let is_archived = channel.archived;

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = window_weak.upgrade() {
            w.set_channel_name(channel_name.into());
            w.set_channel_type(channel_type.into());
            w.set_member_count(member_count);
            w.set_topic(topic.into());
            w.set_is_archived(is_archived);
            w.set_messages(ModelRc::new(VecModel::from(slint_messages)));
            w.set_loading_messages(false);
        }
    });
}
