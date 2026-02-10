//! Authentication feature callbacks.
//!
//! Handles login, register, and logout UI flows.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_linux::app::AppContext;
use chatter_linux::domain::channel::ChannelType;
use chatter_linux::inbox::provider::InboxEvent;
use chatter_linux::network::grpc::auth_client::AuthClient;
use chatter_linux::network::grpc::channel_client::ChannelClient;
use chatter_linux::network::grpc::client_pool::GrpcClientPool;
use chatter_linux::services::auth_service::AuthService;
use chatter_linux::services::channel_service::ChannelService;
use chatter_linux::state::AuthState;
use chatter_linux::storage::{AttachmentRepository, ChannelRepository, MessageRepository, SettingsRepository, UserRepository};

use crate::bin_ui::converters::{channel_to_slint, dm_to_slint, get_initials};
use crate::bin_ui::features::sidebar::load_channel_into_ui;
use crate::{ChannelListItemData, DMListItemData, MainWindow, MessageItemData};

/// Update a reaction summary string when a reaction is added or removed.
///
/// The summary format is "👍 3  ❤️ 2" (emoji space count, double-space separated).
fn update_reaction_summary(current: &str, emoji: &str, remove: bool) -> String {
    use std::collections::HashMap;

    // Parse current summary into map of emoji -> count
    let mut reactions: HashMap<String, i32> = HashMap::new();

    for part in current.split("  ") {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        // Format is "emoji count" or just "emoji" (count=1)
        let mut chars = part.chars();
        let mut emoji_chars = String::new();
        let mut count_str = String::new();

        // Collect emoji (may be multiple chars for complex emoji)
        for c in chars.by_ref() {
            if c.is_ascii_digit() || c == ' ' {
                if c.is_ascii_digit() {
                    count_str.push(c);
                }
                break;
            }
            emoji_chars.push(c);
        }
        // Collect remaining digits
        for c in chars {
            if c.is_ascii_digit() {
                count_str.push(c);
            }
        }

        let count = count_str.parse::<i32>().unwrap_or(1);
        if !emoji_chars.is_empty() {
            reactions.insert(emoji_chars, count);
        }
    }

    // Update the count
    if remove {
        if let Some(count) = reactions.get_mut(emoji) {
            *count -= 1;
            if *count <= 0 {
                reactions.remove(emoji);
            }
        }
    } else {
        *reactions.entry(emoji.to_string()).or_insert(0) += 1;
    }

    // Rebuild the summary string
    if reactions.is_empty() {
        return String::new();
    }

    reactions
        .iter()
        .map(|(e, c)| {
            if *c == 1 {
                e.clone()
            } else {
                format!("{} {}", e, c)
            }
        })
        .collect::<Vec<_>>()
        .join("  ")
}

/// Wire authentication callbacks (login, register, logout).
pub fn wire(window: &MainWindow, ctx: &AppContext) {
    wire_login(window, ctx);
    wire_register(window, ctx);
    wire_logout(window, ctx);
}

fn wire_login(window: &MainWindow, ctx: &AppContext) {
    let auth_state = ctx.auth_state.clone();
    let grpc_pool = ctx.grpc_pool.clone();
    let identity_manager = ctx.identity_manager.clone();
    let prekey_manager = ctx.prekey_manager.clone();
    let user_repo = ctx.user_repo.clone();
    let settings_repo = ctx.settings_repo.clone();
    let key_store = ctx.key_store.clone();
    let auth_interceptor = ctx.auth_interceptor.clone();
    let inbox_provider = ctx.get_device_inbox_provider();
    let channel_repo = ctx.channel_repo.clone();
    let message_repo = ctx.message_repo.clone();
    let attachment_repo = ctx.attachment_repo.clone();
    let signal_service = ctx.signal_service.clone();

    let weak_window = window.as_weak();

    window.on_login(move |username: SharedString, password: SharedString| {
        let auth_state = auth_state.clone();
        let grpc_pool = grpc_pool.clone();
        let identity_manager = identity_manager.clone();
        let prekey_manager = prekey_manager.clone();
        let user_repo = user_repo.clone();
        let settings_repo = settings_repo.clone();
        let key_store = key_store.clone();
        let auth_interceptor = auth_interceptor.clone();
        let inbox_provider = inbox_provider.clone();
        let channel_repo = channel_repo.clone();
        let message_repo = message_repo.clone();
        let attachment_repo = attachment_repo.clone();
        let signal_service = signal_service.clone();
        let weak = weak_window.clone();
        let username = username.to_string();
        let password = password.to_string();

        tokio::spawn(async move {
            // Set loading state
            let weak_for_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_for_loading.upgrade() {
                    w.set_login_loading(true);
                    w.set_login_error("".into());
                }
            });

            // Create auth service and client
            let auth_service = AuthService::new(
                identity_manager,
                prekey_manager,
                user_repo.clone(),
                settings_repo.clone(),
                key_store,
                auth_interceptor,
            );

            let mut auth_client = AuthClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            // Attempt login
            match auth_service
                .login(&mut auth_client, &username, &password, "linux-client")
                .await
            {
                Ok(result) => {
                    // Update auth state
                    auth_state.set_logged_in(
                        result.user_id,
                        result.device_id,
                        result.display_name.clone(),
                    );

                    // Start inbox provider if available
                    if let Some(provider) = &inbox_provider {
                        if let Err(e) = provider.start().await {
                            tracing::error!("Failed to start inbox provider: {}", e);
                        } else {
                            // Subscribe to inbox events for DM handling
                            let mut event_rx = provider.subscribe();
                            let weak_for_events = weak.clone();
                            let user_repo_for_events = user_repo.clone();

                            tokio::spawn(async move {
                                while let Ok(event) = event_rx.recv().await {
                                    handle_inbox_event(event, &weak_for_events, &user_repo_for_events);
                                }
                            });
                        }
                    } else {
                        tracing::warn!("Inbox provider not available (offline mode)");
                    }

                    // Switch to main view
                    let weak_for_view = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_for_view.upgrade() {
                            w.set_login_loading(false);
                            w.set_current_view("main".into());
                        }
                    });

                    // Load initial data (sync from server first)
                    load_initial_data(
                        weak,
                        grpc_pool,
                        channel_repo,
                        message_repo,
                        user_repo,
                        attachment_repo,
                        settings_repo,
                        signal_service,
                        auth_state,
                    )
                    .await;
                }
                Err(e) => {
                    let error_msg = e.to_string();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_login_loading(false);
                            w.set_login_error(error_msg.into());
                        }
                    });
                }
            }
        });
    });
}

fn wire_register(window: &MainWindow, ctx: &AppContext) {
    let auth_state = ctx.auth_state.clone();
    let grpc_pool = ctx.grpc_pool.clone();
    let identity_manager = ctx.identity_manager.clone();
    let prekey_manager = ctx.prekey_manager.clone();
    let user_repo = ctx.user_repo.clone();
    let settings_repo = ctx.settings_repo.clone();
    let key_store = ctx.key_store.clone();
    let auth_interceptor = ctx.auth_interceptor.clone();
    let inbox_provider = ctx.get_device_inbox_provider();
    let channel_repo = ctx.channel_repo.clone();
    let message_repo = ctx.message_repo.clone();
    let attachment_repo = ctx.attachment_repo.clone();
    let signal_service = ctx.signal_service.clone();

    let weak_window = window.as_weak();

    window.on_register(
        move |username: SharedString, password: SharedString, display_name: SharedString| {
            let auth_state = auth_state.clone();
            let grpc_pool = grpc_pool.clone();
            let identity_manager = identity_manager.clone();
            let prekey_manager = prekey_manager.clone();
            let user_repo = user_repo.clone();
            let settings_repo = settings_repo.clone();
            let key_store = key_store.clone();
            let auth_interceptor = auth_interceptor.clone();
            let inbox_provider = inbox_provider.clone();
            let channel_repo = channel_repo.clone();
            let message_repo = message_repo.clone();
            let attachment_repo = attachment_repo.clone();
            let signal_service = signal_service.clone();
            let weak = weak_window.clone();
            let username = username.to_string();
            let password = password.to_string();
            let display_name = display_name.to_string();

            tokio::spawn(async move {
                // Set loading state
                let weak_for_loading = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_for_loading.upgrade() {
                        w.set_register_loading(true);
                        w.set_register_error("".into());
                    }
                });

                // Create auth service and client
                let auth_service = AuthService::new(
                    identity_manager,
                    prekey_manager,
                    user_repo.clone(),
                    settings_repo.clone(),
                    key_store,
                    auth_interceptor,
                );

                let mut auth_client = AuthClient::new(
                    grpc_pool.channel(),
                    grpc_pool.auth_clone(),
                );

                // Attempt registration
                match auth_service
                    .register(
                        &mut auth_client,
                        &username,
                        &password,
                        &display_name,
                        "linux-client",
                    )
                    .await
                {
                    Ok(result) => {
                        // Update auth state
                        auth_state.set_logged_in(
                            result.user_id,
                            result.device_id,
                            result.display_name.clone(),
                        );

                        // Start inbox provider if available
                        if let Some(provider) = &inbox_provider {
                            if let Err(e) = provider.start().await {
                                tracing::error!("Failed to start inbox provider: {}", e);
                            } else {
                                // Subscribe to inbox events for DM handling
                                let mut event_rx = provider.subscribe();
                                let weak_for_events = weak.clone();
                                let user_repo_for_events = user_repo.clone();

                                tokio::spawn(async move {
                                    while let Ok(event) = event_rx.recv().await {
                                        handle_inbox_event(event, &weak_for_events, &user_repo_for_events);
                                    }
                                });
                            }
                        } else {
                            tracing::warn!("Inbox provider not available (offline mode)");
                        }

                        // Switch to main view
                        let weak_for_view = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak_for_view.upgrade() {
                                w.set_register_loading(false);
                                w.set_current_view("main".into());
                            }
                        });

                        // Load initial data (sync from server first)
                        load_initial_data(
                            weak,
                            grpc_pool,
                            channel_repo,
                            message_repo,
                            user_repo,
                            attachment_repo,
                            settings_repo,
                            signal_service,
                            auth_state,
                        )
                        .await;
                    }
                    Err(e) => {
                        let error_msg = e.to_string();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_register_loading(false);
                                w.set_register_error(error_msg.into());
                            }
                        });
                    }
                }
            });
        },
    );
}

fn wire_logout(window: &MainWindow, ctx: &AppContext) {
    let auth_state = ctx.auth_state.clone();
    let grpc_pool = ctx.grpc_pool.clone();
    let identity_manager = ctx.identity_manager.clone();
    let prekey_manager = ctx.prekey_manager.clone();
    let user_repo = ctx.user_repo.clone();
    let settings_repo = ctx.settings_repo.clone();
    let key_store = ctx.key_store.clone();
    let auth_interceptor = ctx.auth_interceptor.clone();
    let inbox_provider = ctx.get_device_inbox_provider();

    let weak_window = window.as_weak();

    window.on_settings_logout(move || {
        let auth_state = auth_state.clone();
        let grpc_pool = grpc_pool.clone();
        let identity_manager = identity_manager.clone();
        let prekey_manager = prekey_manager.clone();
        let user_repo = user_repo.clone();
        let settings_repo = settings_repo.clone();
        let key_store = key_store.clone();
        let auth_interceptor = auth_interceptor.clone();
        let inbox_provider = inbox_provider.clone();
        let weak = weak_window.clone();

        tokio::spawn(async move {
            // Stop inbox provider if available
            if let Some(provider) = &inbox_provider {
                if let Err(e) = provider.stop().await {
                    tracing::error!("Failed to stop inbox provider: {}", e);
                }
            }

            // Create auth service and client
            let auth_service = AuthService::new(
                identity_manager,
                prekey_manager,
                user_repo,
                settings_repo,
                key_store,
                auth_interceptor,
            );

            let mut auth_client = AuthClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            // Attempt logout (ignore errors - we logout locally regardless)
            let _ = auth_service.logout(&mut auth_client).await;

            // Update auth state
            auth_state.set_logged_out();

            // Switch to login view
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_current_view("login".into());
                    w.set_active_modal("none".into());
                    w.set_login_username("".into());
                    w.set_login_password("".into());
                    w.set_login_error("".into());
                }
            });
        });
    });
}

/// Load initial data after login or on startup.
pub async fn load_initial_data(
    window_weak: slint::Weak<MainWindow>,
    grpc_pool: Arc<GrpcClientPool>,
    channel_repo: Arc<ChannelRepository>,
    message_repo: Arc<MessageRepository>,
    user_repo: Arc<UserRepository>,
    attachment_repo: Arc<AttachmentRepository>,
    settings_repo: Arc<SettingsRepository>,
    signal_service: Arc<chatter_linux::services::signal_service::SignalService>,
    auth_state: AuthState,
) {
    tracing::info!("Loading initial data...");

    // Create channel service and client
    let channel_service = ChannelService::new(settings_repo.clone(), channel_repo.clone(), signal_service);
    let mut channel_client = ChannelClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

    // Sync channels from server first (fetches and upserts to local storage)
    // This ensures saved messages channel and all other channels are available locally
    match channel_service
        .list_channels(&mut channel_client, 100, 0, None)
        .await
    {
        Ok(synced_channels) => {
            tracing::info!("Synced {} channels from server", synced_channels.len());
        }
        Err(e) => {
            tracing::warn!("Failed to sync channels from server: {}", e);
        }
    }

    // Now get channels from local storage (which has been synced)
    let channels = match channel_service.get_channels_for_user() {
        Ok(ch) => ch,
        Err(e) => {
            tracing::warn!("Failed to load channels from local storage: {}", e);
            Vec::new()
        }
    };

    // Separate channels and DMs
    let mut channel_list: Vec<ChannelListItemData> = Vec::new();
    let mut dm_list: Vec<DMListItemData> = Vec::new();

    for channel in &channels {
        match channel.channel_type {
            ChannelType::DirectMessage => {
                let other_user_name = channel.name.clone();
                let other_user_id = channel.owner_id.to_string();
                dm_list.push(dm_to_slint(channel, &other_user_name, &other_user_id));
            }
            ChannelType::Public | ChannelType::Private => {
                channel_list.push(channel_to_slint(channel));
            }
            ChannelType::SavedMessages => {
                // Saved messages has its own sidebar button, not listed with channels
            }
        }
    }

    // Get user profile info
    let user_display_name = auth_state.get().display_name.clone().unwrap_or_default();
    let user_initials = get_initials(&user_display_name);

    // Determine first channel to select (exclude DMs and SavedMessages)
    let first_channel_id = channels
        .iter()
        .find(|c| {
            !matches!(
                c.channel_type,
                ChannelType::DirectMessage | ChannelType::SavedMessages
            )
        })
        .map(|c| c.id);

    // Clone for loading
    let first_channel_id_for_load = first_channel_id;
    let channel_repo_for_load = channel_repo.clone();
    let message_repo_for_load = message_repo.clone();
    let user_repo_for_load = user_repo.clone();
    let attachment_repo_for_load = attachment_repo.clone();

    // Update UI with channels and DMs
    let window_weak_ui = window_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = window_weak_ui.upgrade() {
            w.set_channels(ModelRc::new(VecModel::from(channel_list)));
            w.set_dm_conversations(ModelRc::new(VecModel::from(dm_list)));
            w.set_user_display_name(user_display_name.into());
            w.set_user_initials(user_initials.into());
            w.set_user_presence_status("online".into());
        }
    });

    // If there's a channel, select it and load messages
    if let Some(channel_id) = first_channel_id_for_load {
        let channel_id_str = channel_id.to_string();

        let window_weak_select = window_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = window_weak_select.upgrade() {
                w.set_selected_channel_id(channel_id_str.into());
            }
        });

        load_channel_into_ui(
            window_weak,
            &channel_id,
            channel_repo_for_load,
            message_repo_for_load,
            user_repo_for_load,
            attachment_repo_for_load,
        )
        .await;
    }

    tracing::info!("Initial data load complete");
}

/// Handle inbox events and update UI accordingly.
fn handle_inbox_event(
    event: InboxEvent,
    weak_window: &slint::Weak<MainWindow>,
    user_repo: &UserRepository,
) {
    match event {
        InboxEvent::DmReceived {
            peer_id,
            text,
            timestamp,
            message_id,
        } => {
            tracing::info!("DM received from {}: {} chars", peer_id, text.len());

            // Get sender name from user repo
            let sender_name = user_repo
                .get(&peer_id)
                .ok()
                .flatten()
                .map(|u| u.username.clone())
                .unwrap_or_else(|| peer_id.to_string());

            let weak = weak_window.clone();
            let peer_id_str = peer_id.to_string();

            let _ = slint::invoke_from_event_loop(move || {
                let Some(window) = weak.upgrade() else {
                    return;
                };

                // Check if this DM conversation is currently selected
                let selected_dm = window.get_selected_dm_id().to_string();

                if selected_dm == peer_id_str {
                    // Add message to current view
                    let current_model = window.get_messages();
                    let mut messages: Vec<_> = (0..current_model.row_count())
                        .filter_map(|i| current_model.row_data(i))
                        .collect();

                    // Create message display data
                    let initials = get_initials(&sender_name);
                    let timestamp_str = format_timestamp(timestamp);

                    // Create message preview for sidebar (before text is moved)
                    let preview = if text.len() > 50 {
                        format!("{}...", &text[..47])
                    } else {
                        text.clone()
                    };

                    let msg_item = MessageItemData {
                        message_id: message_id.into(),
                        sender_id: peer_id_str.clone().into(),
                        sender_name: sender_name.into(),
                        sender_initials: initials.into(),
                        sender_avatar_url: "".into(),
                        content: text.into(),
                        timestamp: timestamp_str.clone().into(),
                        timestamp_ms: timestamp as i32,
                        is_own: false,
                        is_edited: false,
                        is_deleted: false,
                        is_system: false,
                        is_ai: false,
                        has_attachments: false,
                        attachment_count: 0,
                        attachments: ModelRc::new(VecModel::from(Vec::new())),
                        has_reactions: false,
                        reaction_summary: "".into(),
                        reply_count: 0,
                        reply_count_text: "".into(),
                        is_thread_reply: false,
                        show_sender: true,
                        delivery_status: "".into(), // Not own message
                    };

                    messages.push(msg_item);
                    window.set_messages(ModelRc::new(VecModel::from(messages)));

                    tracing::debug!("Added DM message to current view");

                    // Update sidebar with last message preview
                    let dms = window.get_dm_conversations();
                    for i in 0..dms.row_count() {
                        if let Some(dm) = dms.row_data(i) {
                            if dm.user_id.as_str() == peer_id_str {
                                let mut updated_dm = dm.clone();
                                updated_dm.last_message = preview.into();
                                updated_dm.timestamp = timestamp_str.into();

                                if let Some(model) = dms.as_any().downcast_ref::<VecModel<DMListItemData>>() {
                                    model.set_row_data(i, updated_dm);
                                }
                                break;
                            }
                        }
                    }
                } else {
                    // DM is not currently selected - still update sidebar
                    let dms = window.get_dm_conversations();
                    let timestamp_str = format_timestamp(timestamp);
                    let preview = if text.len() > 50 {
                        format!("{}...", &text[..47])
                    } else {
                        text
                    };

                    for i in 0..dms.row_count() {
                        if let Some(dm) = dms.row_data(i) {
                            if dm.user_id.as_str() == peer_id_str {
                                let mut updated_dm = dm.clone();
                                updated_dm.last_message = preview.into();
                                updated_dm.timestamp = timestamp_str.into();

                                if let Some(model) = dms.as_any().downcast_ref::<VecModel<DMListItemData>>() {
                                    model.set_row_data(i, updated_dm);
                                }
                                break;
                            }
                        }
                    }
                }
            });
        }
        InboxEvent::DmDecryptionFailed { peer_id, error } => {
            tracing::error!("DM decryption failed from {}: {}", peer_id, error);
        }
        InboxEvent::DmPollCompleted { total_messages } => {
            if total_messages > 0 {
                tracing::debug!("DM poll completed: {} messages", total_messages);
            }
        }
        // Regular inbox events - logged but not displayed
        InboxEvent::MessageReceived { message_id, .. } => {
            tracing::trace!("Inbox message received: {}", message_id);
        }
        InboxEvent::MessageProcessed { message_id } => {
            tracing::trace!("Inbox message processed: {}", message_id);
        }
        InboxEvent::MessageFailed { message_id, error } => {
            tracing::warn!("Inbox message failed: {} - {}", message_id, error);
        }
        InboxEvent::PollCompleted { count, .. } => {
            if count > 0 {
                tracing::trace!("Inbox poll completed: {} messages", count);
            }
        }
        InboxEvent::PollFailed { error } => {
            tracing::warn!("Inbox poll failed: {}", error);
        }
        InboxEvent::AcknowledgeFailed { message_ids, error } => {
            tracing::warn!("Inbox ack failed for {:?}: {}", message_ids, error);
        }
        InboxEvent::DmUnreadCountUpdated { channel_id, peer_id, unread_count } => {
            tracing::debug!(
                "DM unread count updated: {} (peer {}) = {}",
                channel_id, peer_id, unread_count
            );

            // Update sidebar DM item with new unread count
            if let Some(window) = weak_window.upgrade() {
                let peer_id_str = peer_id.to_string();
                let dms = window.get_dm_conversations();

                for i in 0..dms.row_count() {
                    if let Some(dm) = dms.row_data(i) {
                        if dm.user_id.as_str() == peer_id_str {
                            // Clone the DM data and update unread count
                            let mut updated_dm = dm.clone();
                            updated_dm.unread_count = unread_count as i32;

                            // Update in the model
                            if let Some(model) = dms.as_any().downcast_ref::<VecModel<DMListItemData>>() {
                                model.set_row_data(i, updated_dm);
                            }
                            break;
                        }
                    }
                }
            }
        }
        InboxEvent::DmTyping { peer_id, action } => {
            tracing::debug!(peer_id = %peer_id, action, "Received typing indicator");
            // action: 1=Started, 2=Stopped

            // Get peer name for display
            let peer_name = user_repo
                .get(&peer_id)
                .ok()
                .flatten()
                .map(|u| u.username.clone())
                .unwrap_or_else(|| "Someone".to_string());

            let peer_id_str = peer_id.to_string();
            let weak = weak_window.clone();

            let _ = slint::invoke_from_event_loop(move || {
                let Some(window) = weak.upgrade() else {
                    return;
                };

                // Only show typing for currently selected DM
                let selected_dm = window.get_selected_dm_id().to_string();
                if selected_dm != peer_id_str {
                    return;
                }

                // Update typing indicator based on action
                if action == 1 {
                    // Started typing
                    window.set_typing_indicator(format!("{} is typing...", peer_name).into());
                } else {
                    // Stopped typing
                    window.set_typing_indicator("".into());
                }
            });
        }
        InboxEvent::DmReceipt { peer_id, receipt_type, timestamps } => {
            tracing::debug!(
                peer_id = %peer_id,
                receipt_type,
                count = timestamps.len(),
                "Received read/delivery receipt"
            );
            // receipt_type: 1=Delivery, 2=Read, 3=Viewed
            let status = match receipt_type {
                1 => "delivered",
                2 | 3 => "read",
                _ => "sent",
            };

            let peer_id_str = peer_id.to_string();
            let weak = weak_window.clone();

            let _ = slint::invoke_from_event_loop(move || {
                let Some(window) = weak.upgrade() else {
                    return;
                };

                // Only update if this DM is currently selected
                let selected_dm = window.get_selected_dm_id().to_string();
                if selected_dm != peer_id_str {
                    return;
                }

                // Update delivery status for messages with matching timestamps
                let messages = window.get_messages();

                for i in 0..messages.row_count() {
                    if let Some(msg) = messages.row_data(i) {
                        // Only update own messages
                        if !msg.is_own {
                            continue;
                        }

                        let msg_ts = msg.timestamp_ms as u64;
                        if timestamps.contains(&msg_ts) {
                            let mut updated_msg = msg.clone();
                            updated_msg.delivery_status = status.into();

                            if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>() {
                                model.set_row_data(i, updated_msg);
                            }

                            tracing::debug!(
                                "Updated delivery status to '{}' for message at index {}",
                                status, i
                            );
                        }
                    }
                }
            });
        }
        InboxEvent::DmReaction { peer_id, emoji, remove, target_author_aci: _, target_sent_timestamp } => {
            tracing::debug!(
                peer_id = %peer_id,
                emoji = %emoji,
                remove,
                target = target_sent_timestamp,
                "Received reaction"
            );

            let peer_id_str = peer_id.to_string();
            let weak = weak_window.clone();

            let _ = slint::invoke_from_event_loop(move || {
                let Some(window) = weak.upgrade() else {
                    return;
                };

                // Only update if this DM is currently selected
                let selected_dm = window.get_selected_dm_id().to_string();
                if selected_dm != peer_id_str {
                    return;
                }

                // Find message by timestamp_ms and update reactions
                let messages = window.get_messages();
                let target_ts = target_sent_timestamp as i32;

                for i in 0..messages.row_count() {
                    if let Some(msg) = messages.row_data(i) {
                        if msg.timestamp_ms == target_ts {
                            let mut updated_msg = msg.clone();

                            // Parse and update reaction summary
                            let new_summary = update_reaction_summary(
                                &msg.reaction_summary.to_string(),
                                &emoji,
                                remove,
                            );

                            updated_msg.reaction_summary = new_summary.into();
                            updated_msg.has_reactions = !updated_msg.reaction_summary.is_empty();

                            if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>() {
                                model.set_row_data(i, updated_msg);
                            }

                            tracing::debug!("Updated DM message reactions at index {}", i);
                            return;
                        }
                    }
                }

                tracing::debug!("DM reaction received but message not found (ts={})", target_sent_timestamp);
            });
        }
        InboxEvent::DmPin { peer_id, target_sent_timestamp, unpin, .. } => {
            tracing::debug!(
                peer_id = %peer_id,
                target = target_sent_timestamp,
                unpin,
                "Received pin/unpin"
            );

            let peer_id_str = peer_id.to_string();
            let weak = weak_window.clone();

            let _ = slint::invoke_from_event_loop(move || {
                let Some(window) = weak.upgrade() else {
                    return;
                };

                // Only update if this DM is currently selected
                let selected_dm = window.get_selected_dm_id().to_string();
                if selected_dm != peer_id_str {
                    return;
                }

                // Find the message content by timestamp to use as preview
                let messages = window.get_messages();
                let target_ts = target_sent_timestamp as i32;
                let mut preview = String::new();

                for i in 0..messages.row_count() {
                    if let Some(msg) = messages.row_data(i) {
                        if msg.timestamp_ms == target_ts {
                            preview = msg.content.to_string();
                            if preview.len() > 100 {
                                preview = format!("{}...", &preview[..97]);
                            }
                            break;
                        }
                    }
                }

                // Update pinned bar state
                let current_count = window.get_pinned_total_count();
                if unpin {
                    // Unpinning
                    let new_count = (current_count - 1).max(0);
                    window.set_pinned_total_count(new_count);
                    window.set_has_pinned_messages(new_count > 0);
                    if new_count == 0 {
                        window.set_pinned_message_preview("".into());
                    }
                } else {
                    // Pinning
                    let new_count = current_count + 1;
                    window.set_pinned_total_count(new_count);
                    window.set_has_pinned_messages(true);
                    if !preview.is_empty() {
                        window.set_pinned_message_preview(preview.into());
                        window.set_pinned_current_index(new_count); // Show newest pin
                    }
                }

                tracing::debug!(
                    "Updated pinned bar: {} -> {}",
                    if unpin { "unpinned" } else { "pinned" },
                    window.get_pinned_total_count()
                );
            });
        }
        InboxEvent::DmEdit { peer_id, target_sent_timestamp, new_body } => {
            tracing::debug!(
                peer_id = %peer_id,
                target = target_sent_timestamp,
                new_len = new_body.len(),
                "Received message edit"
            );

            let peer_id_str = peer_id.to_string();
            let weak = weak_window.clone();

            let _ = slint::invoke_from_event_loop(move || {
                let Some(window) = weak.upgrade() else {
                    return;
                };

                // Only update if this DM is currently selected
                let selected_dm = window.get_selected_dm_id().to_string();
                if selected_dm != peer_id_str {
                    return;
                }

                // Find message by timestamp_ms and update content
                let messages = window.get_messages();
                let target_ts = target_sent_timestamp as i32;

                for i in 0..messages.row_count() {
                    if let Some(msg) = messages.row_data(i) {
                        if msg.timestamp_ms == target_ts {
                            // Found the message - update content and mark as edited
                            let mut updated_msg = msg.clone();
                            updated_msg.content = new_body.clone().into();
                            updated_msg.is_edited = true;

                            if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>() {
                                model.set_row_data(i, updated_msg);
                            }

                            tracing::debug!("Updated DM message at index {} with edit", i);
                            return;
                        }
                    }
                }

                tracing::debug!("DM edit received but message not found in current view (ts={})", target_sent_timestamp);
            });
        }
        InboxEvent::DmDeleteForMe { conversation_id, target_sent_timestamp, .. } => {
            tracing::debug!(
                conversation_id = %conversation_id,
                target = target_sent_timestamp,
                "Received delete-for-me sync"
            );

            let conversation_id_str = conversation_id.clone();
            let weak = weak_window.clone();

            let _ = slint::invoke_from_event_loop(move || {
                let Some(window) = weak.upgrade() else {
                    return;
                };

                // Only update if this DM is currently selected
                let selected_dm = window.get_selected_dm_id().to_string();
                if selected_dm != conversation_id_str {
                    return;
                }

                // Find message by timestamp_ms and mark as deleted (local only)
                let messages = window.get_messages();
                let target_ts = target_sent_timestamp as i32;

                for i in 0..messages.row_count() {
                    if let Some(msg) = messages.row_data(i) {
                        if msg.timestamp_ms == target_ts {
                            // Found the message - mark as deleted
                            let mut updated_msg = msg.clone();
                            updated_msg.is_deleted = true;
                            updated_msg.content = "Message deleted".into();

                            if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>() {
                                model.set_row_data(i, updated_msg);
                            }

                            tracing::debug!("Marked DM message at index {} as deleted (for-me)", i);
                            return;
                        }
                    }
                }

                tracing::debug!("DM delete-for-me received but message not found (ts={})", target_sent_timestamp);
            });
        }
        InboxEvent::DmDeleteForEveryone { peer_id, target_sent_timestamp } => {
            tracing::debug!(
                peer_id = %peer_id,
                target = target_sent_timestamp,
                "Received delete-for-everyone from peer"
            );

            let peer_id_str = peer_id.to_string();
            let weak = weak_window.clone();

            let _ = slint::invoke_from_event_loop(move || {
                let Some(window) = weak.upgrade() else {
                    return;
                };

                // Only update if this DM is currently selected
                let selected_dm = window.get_selected_dm_id().to_string();
                if selected_dm != peer_id_str {
                    return;
                }

                // Find message by timestamp_ms and mark as deleted
                let messages = window.get_messages();
                let target_ts = target_sent_timestamp as i32;

                for i in 0..messages.row_count() {
                    if let Some(msg) = messages.row_data(i) {
                        if msg.timestamp_ms == target_ts {
                            // Found the message - mark as deleted and update content
                            let mut updated_msg = msg.clone();
                            updated_msg.is_deleted = true;
                            updated_msg.content = "This message was deleted".into();

                            if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>() {
                                model.set_row_data(i, updated_msg);
                            }

                            tracing::debug!("Marked DM message at index {} as deleted", i);
                            return;
                        }
                    }
                }

                tracing::debug!("DM delete-for-everyone received but message not found (ts={})", target_sent_timestamp);
            });
        }
        // Channel control events - received via sender key encryption
        InboxEvent::ChannelTyping { channel_id, sender_id, action } => {
            tracing::debug!(
                channel_id = %channel_id,
                sender_id = %sender_id,
                action,
                "Received channel typing indicator"
            );

            // Get sender name for display
            let sender_name = user_repo
                .get(&sender_id)
                .ok()
                .flatten()
                .map(|u| u.username.clone())
                .unwrap_or_else(|| "Someone".to_string());

            let channel_id_str = channel_id.to_string();
            let weak = weak_window.clone();

            let _ = slint::invoke_from_event_loop(move || {
                let Some(window) = weak.upgrade() else {
                    return;
                };

                // Only show typing for currently selected channel
                let selected_channel = window.get_selected_channel_id().to_string();
                if selected_channel != channel_id_str {
                    return;
                }

                // Update typing indicator based on action
                if action == 1 {
                    // Started typing
                    window.set_typing_indicator(format!("{} is typing...", sender_name).into());
                } else {
                    // Stopped typing
                    window.set_typing_indicator("".into());
                }
            });
        }
        InboxEvent::ChannelReaction { channel_id, sender_id, emoji, remove, target_sent_timestamp, .. } => {
            tracing::debug!(
                channel_id = %channel_id,
                sender_id = %sender_id,
                emoji = %emoji,
                remove,
                target = target_sent_timestamp,
                "Received channel reaction"
            );

            let channel_id_str = channel_id.to_string();
            let weak = weak_window.clone();

            let _ = slint::invoke_from_event_loop(move || {
                let Some(window) = weak.upgrade() else {
                    return;
                };

                // Only update if this channel is currently selected
                let selected_channel = window.get_selected_channel_id().to_string();
                if selected_channel != channel_id_str {
                    return;
                }

                // Find message by timestamp_ms and update reactions
                let messages = window.get_messages();
                let target_ts = target_sent_timestamp as i32;

                for i in 0..messages.row_count() {
                    if let Some(msg) = messages.row_data(i) {
                        if msg.timestamp_ms == target_ts {
                            let mut updated_msg = msg.clone();

                            // Parse and update reaction summary
                            let new_summary = update_reaction_summary(
                                &msg.reaction_summary.to_string(),
                                &emoji,
                                remove,
                            );

                            updated_msg.reaction_summary = new_summary.into();
                            updated_msg.has_reactions = !updated_msg.reaction_summary.is_empty();

                            if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>() {
                                model.set_row_data(i, updated_msg);
                            }

                            tracing::debug!("Updated channel message reactions at index {}", i);
                            return;
                        }
                    }
                }

                tracing::debug!("Channel reaction received but message not found (ts={})", target_sent_timestamp);
            });
        }
        InboxEvent::ChannelPin { channel_id, sender_id, target_sent_timestamp, unpin, .. } => {
            tracing::debug!(
                channel_id = %channel_id,
                sender_id = %sender_id,
                target = target_sent_timestamp,
                unpin,
                "Received channel pin/unpin"
            );

            let channel_id_str = channel_id.to_string();
            let weak = weak_window.clone();

            let _ = slint::invoke_from_event_loop(move || {
                let Some(window) = weak.upgrade() else {
                    return;
                };

                // Only update if this channel is currently selected
                let selected_channel = window.get_selected_channel_id().to_string();
                if selected_channel != channel_id_str {
                    return;
                }

                // Find the message content by timestamp to use as preview
                let messages = window.get_messages();
                let target_ts = target_sent_timestamp as i32;
                let mut preview = String::new();

                for i in 0..messages.row_count() {
                    if let Some(msg) = messages.row_data(i) {
                        if msg.timestamp_ms == target_ts {
                            preview = msg.content.to_string();
                            if preview.len() > 100 {
                                preview = format!("{}...", &preview[..97]);
                            }
                            break;
                        }
                    }
                }

                // Update pinned bar state
                let current_count = window.get_pinned_total_count();
                if unpin {
                    // Unpinning
                    let new_count = (current_count - 1).max(0);
                    window.set_pinned_total_count(new_count);
                    window.set_has_pinned_messages(new_count > 0);
                    if new_count == 0 {
                        window.set_pinned_message_preview("".into());
                    }
                } else {
                    // Pinning
                    let new_count = current_count + 1;
                    window.set_pinned_total_count(new_count);
                    window.set_has_pinned_messages(true);
                    if !preview.is_empty() {
                        window.set_pinned_message_preview(preview.into());
                        window.set_pinned_current_index(new_count); // Show newest pin
                    }
                }

                tracing::debug!(
                    "Updated channel pinned bar: {} -> {}",
                    if unpin { "unpinned" } else { "pinned" },
                    window.get_pinned_total_count()
                );
            });
        }
        InboxEvent::ChannelEdit { channel_id, sender_id, target_sent_timestamp, new_body } => {
            tracing::debug!(
                channel_id = %channel_id,
                sender_id = %sender_id,
                target = target_sent_timestamp,
                new_len = new_body.len(),
                "Received channel message edit"
            );

            let channel_id_str = channel_id.to_string();
            let weak = weak_window.clone();

            let _ = slint::invoke_from_event_loop(move || {
                let Some(window) = weak.upgrade() else {
                    return;
                };

                // Only update if this channel is currently selected
                let selected_channel = window.get_selected_channel_id().to_string();
                if selected_channel != channel_id_str {
                    return;
                }

                // Find message by timestamp_ms and update content
                let messages = window.get_messages();
                let target_ts = target_sent_timestamp as i32;

                for i in 0..messages.row_count() {
                    if let Some(msg) = messages.row_data(i) {
                        if msg.timestamp_ms == target_ts {
                            // Found the message - update content and mark as edited
                            let mut updated_msg = msg.clone();
                            updated_msg.content = new_body.clone().into();
                            updated_msg.is_edited = true;

                            if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>() {
                                model.set_row_data(i, updated_msg);
                            }

                            tracing::debug!("Updated channel message at index {} with edit", i);
                            return;
                        }
                    }
                }

                tracing::debug!("Channel edit received but message not found (ts={})", target_sent_timestamp);
            });
        }
        InboxEvent::ChannelDeleteForEveryone { channel_id, sender_id, target_sent_timestamp } => {
            tracing::debug!(
                channel_id = %channel_id,
                sender_id = %sender_id,
                target = target_sent_timestamp,
                "Received channel delete-for-everyone"
            );

            let channel_id_str = channel_id.to_string();
            let weak = weak_window.clone();

            let _ = slint::invoke_from_event_loop(move || {
                let Some(window) = weak.upgrade() else {
                    return;
                };

                // Only update if this channel is currently selected
                let selected_channel = window.get_selected_channel_id().to_string();
                if selected_channel != channel_id_str {
                    return;
                }

                // Find message by timestamp_ms and mark as deleted
                let messages = window.get_messages();
                let target_ts = target_sent_timestamp as i32;

                for i in 0..messages.row_count() {
                    if let Some(msg) = messages.row_data(i) {
                        if msg.timestamp_ms == target_ts {
                            // Found the message - mark as deleted and update content
                            let mut updated_msg = msg.clone();
                            updated_msg.is_deleted = true;
                            updated_msg.content = "This message was deleted".into();

                            if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>() {
                                model.set_row_data(i, updated_msg);
                            }

                            tracing::debug!("Marked channel message at index {} as deleted", i);
                            return;
                        }
                    }
                }

                tracing::debug!("Channel delete-for-everyone received but message not found (ts={})", target_sent_timestamp);
            });
        }
        InboxEvent::ChannelDeleteForMe { conversation_id, target_sent_timestamp, .. } => {
            tracing::debug!(
                conversation_id = %conversation_id,
                target = target_sent_timestamp,
                "Received channel delete-for-me sync"
            );

            let conversation_id_str = conversation_id.clone();
            let weak = weak_window.clone();

            let _ = slint::invoke_from_event_loop(move || {
                let Some(window) = weak.upgrade() else {
                    return;
                };

                // Only update if this channel is currently selected
                let selected_channel = window.get_selected_channel_id().to_string();
                if selected_channel != conversation_id_str {
                    return;
                }

                // Find message by timestamp_ms and mark as deleted (local only)
                let messages = window.get_messages();
                let target_ts = target_sent_timestamp as i32;

                for i in 0..messages.row_count() {
                    if let Some(msg) = messages.row_data(i) {
                        if msg.timestamp_ms == target_ts {
                            // Found the message - mark as deleted
                            let mut updated_msg = msg.clone();
                            updated_msg.is_deleted = true;
                            updated_msg.content = "Message deleted".into();

                            if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>() {
                                model.set_row_data(i, updated_msg);
                            }

                            tracing::debug!("Marked channel message at index {} as deleted (for-me)", i);
                            return;
                        }
                    }
                }

                tracing::debug!("Channel delete-for-me received but message not found (ts={})", target_sent_timestamp);
            });
        }

        InboxEvent::GroupMailboxPollCompleted { total_groups, total_messages } => {
            tracing::trace!(
                total_groups,
                total_messages,
                "Group mailbox poll completed"
            );
            // Status event - no UI update needed
        }

        InboxEvent::GroupMailboxAnnouncement { channel_id, member_id, .. } => {
            tracing::debug!(
                channel_id = %channel_id,
                member_id = %member_id,
                "Group mailbox announcement received"
            );
            // TODO: Could show notification or update member list
        }
    }
}

/// Format a millisecond timestamp for display.
fn format_timestamp(timestamp_ms: u64) -> String {
    use chrono::{Local, TimeZone};

    if timestamp_ms == 0 {
        return "".to_string();
    }

    let secs = (timestamp_ms / 1000) as i64;
    let nsecs = ((timestamp_ms % 1000) * 1_000_000) as u32;

    match Local.timestamp_opt(secs, nsecs).single() {
        Some(dt) => {
            let now = Local::now();
            if dt.date_naive() == now.date_naive() {
                dt.format("%H:%M").to_string()
            } else {
                dt.format("%b %d, %H:%M").to_string()
            }
        }
        None => "".to_string(),
    }
}
