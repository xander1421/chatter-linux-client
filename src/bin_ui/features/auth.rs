//! Authentication feature callbacks.
//!
//! Handles login, register, and logout UI flows.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use chatter_linux::app::AppContext;
use chatter_linux::domain::channel::ChannelType;
use chatter_linux::network::grpc::auth_client::AuthClient;
use chatter_linux::services::auth_service::AuthService;
use chatter_linux::services::channel_service::ChannelService;
use chatter_linux::state::AuthState;
use chatter_linux::storage::{ChannelRepository, MessageRepository, SettingsRepository, UserRepository};

use crate::bin_ui::converters::{channel_to_slint, dm_to_slint, get_initials};
use crate::bin_ui::features::sidebar::load_channel_into_ui;
use crate::{ChannelListItemData, DMListItemData, MainWindow};

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
    let inbox_provider = ctx.device_inbox_provider.clone();
    let channel_repo = ctx.channel_repo.clone();
    let message_repo = ctx.message_repo.clone();

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

                    // Start inbox provider
                    if let Err(e) = inbox_provider.start().await {
                        tracing::error!("Failed to start inbox provider: {}", e);
                    }

                    // Switch to main view
                    let weak_for_view = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_for_view.upgrade() {
                            w.set_login_loading(false);
                            w.set_current_view("main".into());
                        }
                    });

                    // Load initial data
                    load_initial_data(
                        weak,
                        channel_repo,
                        message_repo,
                        user_repo,
                        settings_repo,
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
    let inbox_provider = ctx.device_inbox_provider.clone();
    let channel_repo = ctx.channel_repo.clone();
    let message_repo = ctx.message_repo.clone();

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

                        // Start inbox provider
                        if let Err(e) = inbox_provider.start().await {
                            tracing::error!("Failed to start inbox provider: {}", e);
                        }

                        // Switch to main view
                        let weak_for_view = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak_for_view.upgrade() {
                                w.set_register_loading(false);
                                w.set_current_view("main".into());
                            }
                        });

                        // Load initial data
                        load_initial_data(
                            weak,
                            channel_repo,
                            message_repo,
                            user_repo,
                            settings_repo,
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
    let inbox_provider = ctx.device_inbox_provider.clone();

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
            // Stop inbox provider
            if let Err(e) = inbox_provider.stop().await {
                tracing::error!("Failed to stop inbox provider: {}", e);
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
    channel_repo: Arc<ChannelRepository>,
    message_repo: Arc<MessageRepository>,
    user_repo: Arc<UserRepository>,
    settings_repo: Arc<SettingsRepository>,
    auth_state: AuthState,
) {
    tracing::info!("Loading initial data...");

    // Get channels from local storage
    let channel_service = ChannelService::new(settings_repo.clone(), channel_repo.clone());
    let channels = match channel_service.get_channels_for_user() {
        Ok(ch) => ch,
        Err(e) => {
            tracing::warn!("Failed to load channels: {}", e);
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
        }
    }

    // Get user profile info
    let user_display_name = auth_state.get().display_name.clone().unwrap_or_default();
    let user_initials = get_initials(&user_display_name);

    // Determine first channel to select
    let first_channel_id = channels
        .iter()
        .find(|c| c.channel_type != ChannelType::DirectMessage)
        .map(|c| c.id);

    // Clone for loading
    let first_channel_id_for_load = first_channel_id;
    let channel_repo_for_load = channel_repo.clone();
    let message_repo_for_load = message_repo.clone();
    let user_repo_for_load = user_repo.clone();

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
        )
        .await;
    }

    tracing::info!("Initial data load complete");
}
