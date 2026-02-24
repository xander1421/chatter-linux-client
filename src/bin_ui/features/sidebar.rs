//! Sidebar callbacks.
//!
//! Handles channel/DM selection, settings, and profile.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_linux::app::AppContext;
use chatter_linux::domain::channel::ChannelType;
use chatter_linux::domain::ids::ChannelId;
use chatter_linux::storage::{AttachmentRepository, ChannelRepository, MessageRepository, UserRepository};

use crate::bin_ui::audio_state;
use crate::bin_ui::converters::{channel_type_to_string, message_to_display_data, MessageDisplayData};
use crate::bin_ui::features::chat::send_dm_read_receipts;
use crate::MainWindow;

/// Wire sidebar callbacks (channel/DM selection, settings, profile).
pub fn wire(window: &MainWindow, ctx: &AppContext) {
    wire_channel_selected(window, ctx);
    wire_dm_selected(window, ctx);
    wire_saved_messages(window, ctx);
    wire_settings_clicked(window, ctx);
    wire_profile_clicked(window, ctx);
    wire_create_join_channel(window);
    wire_toggle_sidebar(window, ctx);
    load_sidebar_collapsed_state(window, ctx);
}

fn wire_channel_selected(window: &MainWindow, ctx: &AppContext) {
    let chat_state = ctx.chat_state.clone();
    let channel_repo = ctx.channel_repo.clone();
    let message_repo = ctx.message_repo.clone();
    let user_repo = ctx.user_repo.clone();
    let attachment_repo = ctx.attachment_repo.clone();

    let weak_window = window.as_weak();

    window.on_channel_selected(move |channel_id: SharedString| {
        let channel_id_str = channel_id.to_string();
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();
        let channel_repo = channel_repo.clone();
        let message_repo = message_repo.clone();
        let user_repo = user_repo.clone();
        let attachment_repo = attachment_repo.clone();

        // Stop any playing audio when switching channels
        stop_audio_playback();

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
            load_channel_into_ui(weak, &channel_id, channel_repo, message_repo, user_repo, attachment_repo).await;
        });
    });
}

fn wire_dm_selected(window: &MainWindow, ctx: &AppContext) {
    let chat_state = ctx.chat_state.clone();
    let channel_repo = ctx.channel_repo.clone();
    let message_repo = ctx.message_repo.clone();
    let user_repo = ctx.user_repo.clone();
    let attachment_repo = ctx.attachment_repo.clone();
    let auth_state = ctx.auth_state.clone();
    let database = ctx.database.clone();
    let grpc_pool = ctx.grpc_pool.clone();
    let message_service = ctx.message_service();
    let mailbox_manager = ctx.mailbox_manager.clone();

    let weak_window = window.as_weak();

    window.on_dm_selected(move |dm_id: SharedString| {
        let dm_id_str = dm_id.to_string();
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();
        let channel_repo = channel_repo.clone();
        let message_repo = message_repo.clone();
        let user_repo = user_repo.clone();
        let attachment_repo = attachment_repo.clone();
        let auth_state = auth_state.clone();
        let database = database.clone();
        let _grpc_pool = grpc_pool.clone();
        let message_service = message_service.clone();
        let mailbox_manager = mailbox_manager.clone();

        // Stop any playing audio when switching channels
        stop_audio_playback();

        tokio::spawn(async move {
            // dm_id is now the channel_id directly (from DMListItemData.channel_id)
            let weak_loading = weak.clone();
            let dm_id_for_ui = dm_id.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_loading_messages(true);
                    w.set_selected_dm_id(dm_id_for_ui);
                    w.set_selected_channel_id("".into());
                }
            });

            // Parse channel ID directly
            let channel_id: ChannelId = match dm_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid DM channel ID: {}", e);
                    return;
                }
            };

            // Update chat state
            chat_state.set_active_channel(channel_id);

            // Load channel and messages
            load_channel_into_ui(weak.clone(), &channel_id, channel_repo.clone(), message_repo.clone(), user_repo, attachment_repo).await;

            // Send read receipts for unread messages from the peer
            if auth_state.is_logged_in() {
                // Get the peer's user ID from the DM channel
                if let Ok(Some(channel)) = channel_repo.get(&channel_id) {
                    if channel.channel_type == ChannelType::DirectMessage {
                        // Get our user ID
                        let my_user_id = auth_state.get().current_user_id.clone();

                        // Get recent messages to find the peer and their message timestamps
                        if let Ok(messages) = message_repo.list_for_channel(&channel_id, None, 50) {
                            // Find peer ID (sender that isn't us)
                            let peer_id = messages
                                .iter()
                                .filter_map(|m| m.sender_id.as_ref())
                                .find(|sender| my_user_id.as_ref() != Some(*sender))
                                .cloned();

                            if let Some(peer_id) = peer_id {
                                // Get sent_at timestamps (nanoseconds) of messages from the peer
                                let unread_timestamps: Vec<u64> = messages
                                    .iter()
                                    .filter(|m| m.sender_id.as_ref() == Some(&peer_id))
                                    .map(|m| m.sent_at)
                                    .collect();

                                if !unread_timestamps.is_empty() {
                                    send_dm_read_receipts(
                                        peer_id,
                                        unread_timestamps,
                                        database,
                                        message_service,
                                        mailbox_manager,
                                    );
                                }
                            }
                        }
                    }
                }
            }
        });
    });
}

fn wire_saved_messages(window: &MainWindow, ctx: &AppContext) {
    let weak_window = window.as_weak();
    let auth_state = ctx.auth_state.clone();
    let chat_state = ctx.chat_state.clone();
    let channel_repo = ctx.channel_repo.clone();
    let message_repo = ctx.message_repo.clone();
    let user_repo = ctx.user_repo.clone();
    let attachment_repo = ctx.attachment_repo.clone();

    window.on_saved_messages_clicked(move || {
        let weak = weak_window.clone();
        let auth_state = auth_state.clone();
        let chat_state = chat_state.clone();
        let channel_repo = channel_repo.clone();
        let message_repo = message_repo.clone();
        let user_repo = user_repo.clone();
        let attachment_repo = attachment_repo.clone();

        // Stop any playing audio when switching channels
        stop_audio_playback();

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
            load_channel_into_ui(weak, &channel_id, channel_repo, message_repo, user_repo, attachment_repo).await;
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

fn wire_create_join_channel(window: &MainWindow) {
    // Open create channel modal (just logs for now, modal is opened via active-modal in Slint)
    window.on_open_create_channel(|| {
        tracing::debug!("Opening create channel modal");
    });

    // Open join channel modal (just logs for now, modal is opened via active-modal in Slint)
    window.on_open_join_channel(|| {
        tracing::debug!("Opening join channel modal");
    });
}

fn wire_toggle_sidebar(window: &MainWindow, ctx: &AppContext) {
    let settings_repo = ctx.settings_repo.clone();
    let weak_window = window.as_weak();

    window.on_toggle_sidebar(move || {
        let settings_repo = settings_repo.clone();
        let weak = weak_window.clone();

        // Get the current collapsed state and persist it
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let collapsed = w.get_sidebar_collapsed();
                // Persist to settings
                if let Err(e) = settings_repo.set_bool(
                    chatter_linux::storage::settings_keys::SIDEBAR_COLLAPSED,
                    collapsed,
                ) {
                    tracing::warn!("Failed to persist sidebar collapsed state: {}", e);
                }
            }
        });
    });
}

fn load_sidebar_collapsed_state(window: &MainWindow, ctx: &AppContext) {
    // Load saved collapsed state on startup
    if let Ok(Some(collapsed)) =
        ctx.settings_repo.get::<bool>(chatter_linux::storage::settings_keys::SIDEBAR_COLLAPSED)
    {
        window.set_sidebar_collapsed(collapsed);
    }
}

/// Load channel data and messages into UI properties.
pub async fn load_channel_into_ui(
    window_weak: slint::Weak<MainWindow>,
    channel_id: &ChannelId,
    channel_repo: Arc<ChannelRepository>,
    message_repo: Arc<MessageRepository>,
    user_repo: Arc<UserRepository>,
    attachment_repo: Arc<AttachmentRepository>,
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

    // Convert messages to display data (thread-safe, can be sent to event loop)
    // Messages come from DB in DESC order (newest first), but chat UI shows oldest first
    let mut display_messages: Vec<MessageDisplayData> = Vec::with_capacity(messages.len());
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

        // Resolve attachments for this message
        let resolved_attachments: Vec<_> = msg
            .attachments
            .iter()
            .filter_map(|att_id| {
                match attachment_repo.get(att_id) {
                    Ok(Some(att)) => {
                        tracing::debug!(
                            "Resolved attachment {} for message {}: local_path={:?}",
                            att_id, msg.id, att.local_path
                        );
                        Some(att)
                    }
                    Ok(None) => {
                        tracing::warn!("Attachment {} not found in repo for message {}", att_id, msg.id);
                        None
                    }
                    Err(e) => {
                        tracing::error!("Error fetching attachment {}: {}", att_id, e);
                        None
                    }
                }
            })
            .collect();

        if !msg.attachments.is_empty() {
            tracing::debug!(
                "Message {} has {} attachment IDs, resolved {} attachments",
                msg.id, msg.attachments.len(), resolved_attachments.len()
            );
        }

        display_messages.push(message_to_display_data(msg, &sender_name, &resolved_attachments));
    }

    // Reverse to show oldest first (newest at bottom, like chat apps)
    display_messages.reverse();

    // Update UI - special handling for SavedMessages and DMs
    let is_saved_messages = channel.channel_type == ChannelType::SavedMessages;
    let is_dm = channel.channel_type == ChannelType::DirectMessage;
    let channel_id_for_lookup = channel.id;

    // For DMs, we'll look up the peer's display name in the event loop
    // where we have access to the DM conversations model
    let channel_name_from_repo = if is_saved_messages {
        Some("Saved Messages".to_string())
    } else if is_dm {
        None // Will look up from DM conversations in event loop
    } else {
        Some(channel.name.clone())
    };
    let channel_type = channel_type_to_string(channel.channel_type);
    let member_count = 0i32;
    // Don't show topic/description for SavedMessages or DMs
    let topic = if is_saved_messages || is_dm {
        String::new()
    } else {
        channel.description.clone().unwrap_or_default()
    };
    let is_archived = channel.archived;

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = window_weak.upgrade() {
            // Convert display data to Slint types (must be done on UI thread)
            let slint_messages: Vec<_> = display_messages.iter().map(|d| d.to_slint()).collect();

            // For DMs, look up the peer's display name from DM conversations
            let channel_name = channel_name_from_repo.unwrap_or_else(|| {
                let conversations = w.get_dm_conversations();
                let channel_id_str = channel_id_for_lookup.to_string();
                for i in 0..conversations.row_count() {
                    if let Some(item) = conversations.row_data(i) {
                        if item.channel_id.as_str() == channel_id_str {
                            let name = item.display_name.to_string();
                            if !name.is_empty() {
                                return name;
                            }
                        }
                    }
                }
                // Fall back to channel_id if no name found
                channel_id_str
            });

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

/// Stop any currently playing audio and clear state.
fn stop_audio_playback() {
    // Stop the audio player
    if let Ok(mut guard) = audio_state::get_audio_player().lock() {
        if let Some(ref mut player) = *guard {
            player.stop();
        }
    }
    // Clear the audio state
    audio_state::clear_state();
}
