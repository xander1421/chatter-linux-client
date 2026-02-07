//! Chatter Linux Client
//!
//! Native Linux desktop client for Chatter secure messaging.

use std::sync::Arc;
use slint::{Model, ModelRc, SharedString, VecModel};

use chatter_linux::app::AppContext;
use chatter_linux::domain::channel::{Channel, ChannelType};
use chatter_linux::domain::ids::ChannelId;
use chatter_linux::domain::message::Message;
use chatter_linux::services::auth_service::AuthService;
use chatter_linux::storage::ChannelRepository;
use chatter_linux::storage::MessageRepository;

// Include Slint-generated code in the binary
slint::include_modules!();

use chatter_linux::network::grpc::message_client::MessageClient;

#[tokio::main]
async fn main() {
    // Initialize tracing for logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    tracing::info!("Chatter Linux Client v0.1.0");

    // Initialize application context (all subsystems)
    let ctx = match AppContext::new() {
        Ok(ctx) => ctx,
        Err(e) => {
            tracing::error!("Failed to initialize application: {}", e);
            eprintln!("Failed to initialize: {}", e);
            std::process::exit(1);
        }
    };

    // Connect to server (continue in offline mode if fails)
    if let Err(e) = ctx.grpc_pool.connect().await {
        tracing::warn!("Could not connect to server: {} (offline mode)", e);
    }

    // Create main window
    let window = MainWindow::new().expect("Failed to create window");

    // Set initial view based on login state
    let auth_service = ctx.auth_service();
    if auth_service.is_logged_in() {
        window.set_current_view("main".into());
        // Populate display name from auth state if available
        if let Some((_, _)) = auth_service.current_user() {
            let display_name = ctx.auth_state.get().display_name.unwrap_or_default();
            window.set_user_display_name(display_name.into());
        }
        
        // Start inbox provider
        let inbox_provider = ctx.device_inbox_provider.clone();
        tokio::spawn(async move {
            if let Err(e) = inbox_provider.start().await {
                tracing::error!("Failed to start inbox provider on startup: {}", e);
            }
        });
    } else {
        window.set_current_view("login".into());
    }

    // Wire callbacks
    wire_auth_callbacks(&window, &ctx);
    wire_navigation_callbacks(&window);
    wire_chat_callbacks(&window, &ctx);
    wire_sidebar_callbacks(&window, &ctx);
    wire_chat_action_callbacks(&window, &ctx);
    wire_header_callbacks(&window, &ctx);
    wire_composer_callbacks(&window, &ctx);

    // Run Slint event loop (blocks until window closes)
    window.run().expect("Failed to run application");

    tracing::info!("Application closed");
}

/// Wire authentication callbacks (login, register, logout).
fn wire_auth_callbacks(window: &MainWindow, ctx: &AppContext) {
    // Clone what we need for callbacks
    let auth_state = ctx.auth_state.clone();
    let grpc_pool = ctx.grpc_pool.clone();
    let identity_manager = ctx.identity_manager.clone();
    let prekey_manager = ctx.prekey_manager.clone();
    let user_repo = ctx.user_repo.clone();
    let settings_repo = ctx.settings_repo.clone();
    let key_store = ctx.key_store.clone();
    let auth_interceptor = ctx.auth_interceptor.clone();
    let device_inbox_provider = ctx.device_inbox_provider.clone();

    // Login callback
    let weak_window = window.as_weak();
    let auth_state_login = auth_state.clone();
    let grpc_pool_login = grpc_pool.clone();
    let identity_manager_login = identity_manager.clone();
    let prekey_manager_login = prekey_manager.clone();
    let user_repo_login = user_repo.clone();
    let settings_repo_login = settings_repo.clone();
    let key_store_login = key_store.clone();
    let auth_interceptor_login = auth_interceptor.clone();
    let inbox_provider_login = device_inbox_provider.clone();

    window.on_login(move |username: SharedString, password: SharedString| {
        let auth_state = auth_state_login.clone();
        let grpc_pool = grpc_pool_login.clone();
        let identity_manager = identity_manager_login.clone();
        let prekey_manager = prekey_manager_login.clone();
        let user_repo = user_repo_login.clone();
        let settings_repo = settings_repo_login.clone();
        let key_store = key_store_login.clone();
        let auth_interceptor = auth_interceptor_login.clone();
        let inbox_provider = inbox_provider_login.clone();
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
                user_repo,
                settings_repo,
                key_store,
                auth_interceptor,
            );

            // Need mutable client for login
            let mut auth_client = chatter_linux::network::grpc::auth_client::AuthClient::new(
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
                    let display_name = result.display_name;
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_login_loading(false);
                            w.set_current_view("main".into());
                            w.set_user_display_name(display_name.into());
                        }
                    });
                }
                Err(e) => {
                    // Show error
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

    // Register callback
    let weak_window = window.as_weak();
    let auth_state_register = auth_state.clone();
    let grpc_pool_register = grpc_pool.clone();
    let identity_manager_register = identity_manager.clone();
    let prekey_manager_register = prekey_manager.clone();
    let user_repo_register = user_repo.clone();
    let settings_repo_register = settings_repo.clone();
    let key_store_register = key_store.clone();
    let auth_interceptor_register = auth_interceptor.clone();
    let inbox_provider_register = device_inbox_provider.clone();

    window.on_register(
        move |username: SharedString, password: SharedString, display_name: SharedString| {
            let auth_state = auth_state_register.clone();
            let grpc_pool = grpc_pool_register.clone();
            let identity_manager = identity_manager_register.clone();
            let prekey_manager = prekey_manager_register.clone();
            let user_repo = user_repo_register.clone();
            let settings_repo = settings_repo_register.clone();
            let key_store = key_store_register.clone();
            let auth_interceptor = auth_interceptor_register.clone();
            let inbox_provider = inbox_provider_register.clone();
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
                    user_repo,
                    settings_repo,
                    key_store,
                    auth_interceptor,
                );

                let mut auth_client = chatter_linux::network::grpc::auth_client::AuthClient::new(
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
                        let display_name = result.display_name;
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_register_loading(false);
                                w.set_current_view("main".into());
                                w.set_user_display_name(display_name.into());
                            }
                        });
                    }
                    Err(e) => {
                        // Show error
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

    // Logout callback (via settings)
    let weak_window = window.as_weak();
    let auth_state_logout = auth_state;
    let grpc_pool_logout = grpc_pool;
    let identity_manager_logout = identity_manager;
    let prekey_manager_logout = prekey_manager;
    let user_repo_logout = user_repo;
    let settings_repo_logout = settings_repo;
    let key_store_logout = key_store;
    let auth_interceptor_logout = auth_interceptor;
    let inbox_provider_logout = device_inbox_provider;

    window.on_settings_logout(move || {
        let auth_state = auth_state_logout.clone();
        let grpc_pool = grpc_pool_logout.clone();
        let identity_manager = identity_manager_logout.clone();
        let prekey_manager = prekey_manager_logout.clone();
        let user_repo = user_repo_logout.clone();
        let settings_repo = settings_repo_logout.clone();
        let key_store = key_store_logout.clone();
        let auth_interceptor = auth_interceptor_logout.clone();
        let inbox_provider = inbox_provider_logout.clone();
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

            let mut auth_client = chatter_linux::network::grpc::auth_client::AuthClient::new(
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
                    // Clear login form
                    w.set_login_username("".into());
                    w.set_login_password("".into());
                    w.set_login_error("".into());
                }
            });
        });
    });
}

/// Wire navigation callbacks (switch between auth screens).
fn wire_navigation_callbacks(window: &MainWindow) {
    // Go to register page
    let weak_window = window.as_weak();
    window.on_go_to_register(move || {
        if let Some(w) = weak_window.upgrade() {
            w.set_current_view("register".into());
            // Clear register form
            w.set_register_display_name("".into());
            w.set_register_username("".into());
            w.set_register_password("".into());
            w.set_register_confirm_password("".into());
            w.set_register_error("".into());
        }
    });

    // Go to login page
    let weak_window = window.as_weak();
    window.on_go_to_login(move || {
        if let Some(w) = weak_window.upgrade() {
            w.set_current_view("login".into());
            // Clear login form
            w.set_login_username("".into());
            w.set_login_password("".into());
            w.set_login_error("".into());
        }
    });
}

/// Wire chat callbacks (send message, etc).
fn wire_chat_callbacks(window: &MainWindow, ctx: &AppContext) {
    let message_service = ctx.message_service();
    let chat_state = ctx.chat_state.clone();
    let auth_state = ctx.auth_state.clone();
    let grpc_pool = ctx.grpc_pool.clone();
    let weak_window = window.as_weak();

    window.on_send(move |text: SharedString| {
        let text = text.to_string();
        if text.trim().is_empty() {
            return;
        }

        let message_service = message_service.clone();
        let chat_state = chat_state.clone();
        let auth_state = auth_state.clone();
        let grpc_pool = grpc_pool.clone();
        let weak = weak_window.clone();

        // Use spawn_blocking with inner runtime to avoid Send requirement
        // for libsignal's trait objects (sealed sender uses &mut dyn SessionStore)
        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create runtime for message send");

            rt.block_on(async {
                // Get active channel
                let channel_id = match chat_state.get().active_channel_id {
                    Some(id) => id,
                    None => {
                        tracing::warn!("Cannot send message: no active channel");
                        return;
                    }
                };

                // Check if logged in
                if !auth_state.is_logged_in() {
                    tracing::warn!("Cannot send message: not logged in");
                    return;
                }

                // Create clients
                let mut message_client = MessageClient::new(
                    grpc_pool.channel(),
                    grpc_pool.auth_clone(),
                );

                let mut auth_client = chatter_linux::network::grpc::auth_client::AuthClient::new(
                     grpc_pool.channel(),
                     grpc_pool.auth_clone(),
                );

                // Send message
                match message_service.send_message(
                    &mut message_client,
                    &mut auth_client,
                    &channel_id,
                    &text,
                    vec![], // No attachments for now
                    None,   // No thread ID for now (TODO: support threads)
                    vec![], // No mentions
                    false,  // Not AI bot target
                ).await {
                    Ok(_) => {
                        // clear composer on success
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_composer_text("".into());
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!("Failed to send message: {}", e);
                        // TODO: Show error in UI
                    }
                }
            });
        });
    });
}

/// Wire sidebar callbacks (channel/DM selection, settings, profile).
fn wire_sidebar_callbacks(window: &MainWindow, ctx: &AppContext) {
    // Clone dependencies for callbacks
    let chat_state = ctx.chat_state.clone();
    let auth_state = ctx.auth_state.clone();
    let channel_repo = ctx.channel_repo.clone();
    let message_repo = ctx.message_repo.clone();
    let user_repo = ctx.user_repo.clone();

    // ========== channel-selected callback ==========
    let weak_window = window.as_weak();
    let chat_state_channel = chat_state.clone();
    let channel_repo_channel = channel_repo.clone();
    let message_repo_channel = message_repo.clone();
    let user_repo_channel = user_repo.clone();

    window.on_channel_selected(move |channel_id: SharedString| {
        let channel_id_str = channel_id.to_string();
        let weak = weak_window.clone();
        let chat_state = chat_state_channel.clone();
        let channel_repo = channel_repo_channel.clone();
        let message_repo = message_repo_channel.clone();
        let user_repo = user_repo_channel.clone();

        tokio::spawn(async move {
            // Set loading state
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_loading_messages(true);
                    w.set_selected_channel_id(channel_id.clone());
                    w.set_selected_dm_id("".into()); // Deselect DM
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
            load_channel_into_ui(
                weak,
                &channel_id,
                channel_repo,
                message_repo,
                user_repo,
            )
            .await;
        });
    });

    // ========== dm-selected callback ==========
    let weak_window = window.as_weak();
    let chat_state_dm = chat_state.clone();
    let channel_repo_dm = channel_repo.clone();
    let message_repo_dm = message_repo.clone();
    let user_repo_dm = user_repo.clone();

    window.on_dm_selected(move |dm_id: SharedString| {
        let dm_id_str = dm_id.to_string();
        let weak = weak_window.clone();
        let chat_state = chat_state_dm.clone();
        let channel_repo = channel_repo_dm.clone();
        let message_repo = message_repo_dm.clone();
        let user_repo = user_repo_dm.clone();

        tokio::spawn(async move {
            // Set loading state
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_loading_messages(true);
                    w.set_selected_dm_id(dm_id.clone());
                    w.set_selected_channel_id("".into()); // Deselect channel
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
            load_channel_into_ui(
                weak,
                &channel_id,
                channel_repo,
                message_repo,
                user_repo,
            )
            .await;
        });
    });

    // ========== saved-messages-clicked callback ==========
    let weak_window = window.as_weak();
    let chat_state_saved = chat_state.clone();

    window.on_saved_messages_clicked(move || {
        let weak = weak_window.clone();
        let _chat_state = chat_state_saved.clone();

        // Clear active channel when viewing saved messages
        // TODO: Implement proper saved messages fetching

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_channel_name("Saved Messages".into());
                w.set_channel_type("saved".into());
                w.set_selected_channel_id("".into());
                w.set_selected_dm_id("".into());
                // Clear messages for now - TODO: load saved messages
                let empty_messages: Vec<MessageItemData> = Vec::new();
                w.set_messages(ModelRc::new(VecModel::from(empty_messages)));
            }
        });
    });

    // ========== settings-clicked callback ==========
    let weak_window = window.as_weak();
    let auth_state_settings = auth_state.clone();
    let settings_repo = ctx.settings_repo.clone();

    window.on_settings_clicked(move || {
        let weak = weak_window.clone();
        let auth_state = auth_state_settings.clone();
        let settings_repo = settings_repo.clone();

        // Synchronous UI update - just open the modal
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                // Populate settings with current user info
                let state = auth_state.get();
                w.set_settings_display_name(
                    state.display_name.unwrap_or_default().into(),
                );
                w.set_settings_username(
                    state.current_user_id.map(|id| id.to_string()).unwrap_or_default().into(),
                );

                // Load privacy settings from repo
                if let Ok(Some(read_receipts)) = settings_repo.get::<bool>(
                    chatter_linux::storage::settings_keys::SEND_READ_RECEIPTS
                ) {
                    w.set_settings_read_receipts(read_receipts);
                }
                if let Ok(Some(typing_indicators)) = settings_repo.get::<bool>(
                    chatter_linux::storage::settings_keys::SHOW_TYPING_INDICATORS
                ) {
                    w.set_settings_typing_indicators(typing_indicators);
                }

                // Open settings modal
                w.set_active_modal("settings".into());
            }
        });
    });

    // ========== profile-clicked callback ==========
    let weak_window = window.as_weak();
    let auth_state_profile = auth_state;

    window.on_profile_clicked(move || {
        let weak = weak_window.clone();
        let auth_state = auth_state_profile.clone();

        // Synchronous UI update - open profile modal with self data
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let state = auth_state.get();

                // Populate profile modal with current user info
                w.set_profile_user_id(
                    state.current_user_id.map(|id| id.to_string()).unwrap_or_default().into(),
                );
                w.set_profile_display_name(
                    state.display_name.clone().unwrap_or_default().into(),
                );
                w.set_profile_username(
                    state.current_user_id.map(|id| id.to_string()).unwrap_or_default().into(),
                );
                w.set_profile_is_self(true);
                w.set_profile_is_friend(false);
                w.set_profile_is_blocked(false);
                w.set_profile_presence("online".into());

                // Open profile modal
                w.set_active_modal("user-profile".into());
            }
        });
    });
}

// ============================================================================
// Helper functions for UI data conversion
// ============================================================================

/// Load channel data and messages into UI properties.
async fn load_channel_into_ui(
    window_weak: slint::Weak<MainWindow>,
    channel_id: &ChannelId,
    channel_repo: Arc<ChannelRepository>,
    message_repo: Arc<MessageRepository>,
    user_repo: Arc<chatter_linux::storage::UserRepository>,
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

    // Convert messages to Slint model, resolving sender names
    let mut slint_messages: Vec<MessageItemData> = Vec::with_capacity(messages.len());
    for msg in &messages {
        let sender_name = if let Some(sender_id) = &msg.sender_id {
            // Try to get profile first (has display_name), fall back to username
            user_repo
                .get_profile(sender_id)
                .ok()
                .flatten()
                .map(|p| p.display_name)
                .or_else(|| {
                    user_repo
                        .get(sender_id)
                        .ok()
                        .flatten()
                        .map(|u| u.username)
                })
                .unwrap_or_else(|| "Unknown".to_string())
        } else {
            "Anonymous".to_string()
        };
        slint_messages.push(message_to_slint(msg, &sender_name));
    }

    // Update UI via invoke_from_event_loop
    let channel_name = channel.name.clone();
    let channel_type = channel_type_to_string(channel.channel_type);
    let member_count = 0i32; // TODO: Track member count
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

/// Convert a domain Channel to Slint ChannelListItemData.
#[allow(dead_code)] // Will be used in 71-05 for initial data loading
fn channel_to_slint(channel: &Channel) -> ChannelListItemData {
    ChannelListItemData {
        channel_id: channel.id.to_string().into(),
        name: channel.name.clone().into(),
        channel_type: channel_type_to_string(channel.channel_type).into(),
        unread_count: channel.unread_count as i32,
    }
}

/// Convert a domain Channel (DM) to Slint DMListItemData.
#[allow(dead_code)] // Will be used in 71-05 for initial data loading
fn dm_to_slint(channel: &Channel, other_user_name: &str, other_user_id: &str) -> DMListItemData {
    DMListItemData {
        user_id: other_user_id.to_string().into(),
        display_name: other_user_name.to_string().into(),
        avatar_url: "".into(),
        initials: get_initials(other_user_name).into(),
        presence_status: "offline".into(),
        last_message: "".into(),
        timestamp: "".into(),
        unread_count: channel.unread_count as i32,
    }
}

/// Convert a domain Message to Slint MessageItemData.
fn message_to_slint(msg: &Message, sender_name: &str) -> MessageItemData {
    MessageItemData {
        message_id: msg.id.to_string().into(),
        sender_id: msg.sender_id.map(|id| id.to_string()).unwrap_or_default().into(),
        sender_name: sender_name.to_string().into(),
        sender_initials: get_initials(sender_name).into(),
        sender_avatar_url: "".into(),
        content: msg.as_text().unwrap_or("").to_string().into(),
        timestamp: msg.created_at.format("%H:%M").to_string().into(),
        is_own: false, // TODO: Compare with current user ID
        is_edited: msg.is_edited,
        is_deleted: msg.is_deleted,
        is_system: false,
        is_ai: msg.targets_ai_bot,
        has_attachments: msg.has_attachments(),
        attachment_count: msg.attachments.len() as i32,
        has_reactions: !msg.reaction_counts.is_empty(),
        reaction_summary: msg
            .reaction_counts
            .iter()
            .map(|r| format!("{} {}", r.emoji, r.count))
            .collect::<Vec<_>>()
            .join(" ")
            .into(),
        reply_count: 0, // TODO: Track reply count
        is_thread_reply: msg.is_thread_reply(),
        show_sender: true, // TODO: Collapse consecutive messages from same sender
    }
}

/// Convert ChannelType enum to string for Slint.
fn channel_type_to_string(ct: ChannelType) -> &'static str {
    match ct {
        ChannelType::Public => "public",
        ChannelType::Private => "private",
        ChannelType::DirectMessage => "dm",
    }
}

/// Extract initials from a display name (first letter of each word, max 2).
fn get_initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|word| word.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase()
}

// ============================================================================
// Chat Action Callbacks (message interactions)
// ============================================================================

/// Wire message action callbacks (reply, react, thread, more, message-clicked).
fn wire_chat_action_callbacks(window: &MainWindow, ctx: &AppContext) {
    let chat_state = ctx.chat_state.clone();
    let message_repo = ctx.message_repo.clone();

    // ========== message-clicked callback ==========
    let weak_window = window.as_weak();
    window.on_message_clicked(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let weak = weak_window.clone();
        tracing::debug!("Message clicked: {}", message_id_str);

        // For now, just log the click - context menu or selection logic can be added later
        // This is the base event that could trigger multi-select or context menus
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(_w) = weak.upgrade() {
                // Future: Store selected message ID for context menu
                tracing::trace!("Message selected: {}", message_id_str);
            }
        });
    });

    // ========== reply-clicked callback ==========
    let weak_window = window.as_weak();
    let chat_state_reply = chat_state.clone();
    let message_repo_reply = message_repo.clone();

    window.on_reply_clicked(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let weak = weak_window.clone();
        let _chat_state = chat_state_reply.clone(); // Future: Store reply target in chat state
        let message_repo = message_repo_reply.clone();

        tokio::spawn(async move {
            // Parse message ID
            let msg_id: chatter_linux::domain::ids::MessageId = match message_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid message ID for reply: {}", e);
                    return;
                }
            };

            // Get the message to show in reply preview
            let reply_preview = match message_repo.get(&msg_id) {
                Ok(Some(msg)) => {
                    let sender = msg.sender_id.map(|id| id.to_string()).unwrap_or_default();
                    let content = msg.as_text().unwrap_or("").to_string();
                    let preview = if content.len() > 50 {
                        format!("{}...", &content[..47])
                    } else {
                        content
                    };
                    format!("Reply to {}: {}", sender, preview)
                }
                _ => "Reply to message".to_string(),
            };

            // Update chat state with reply target
            // Note: We'll need to extend ChatStateData to track reply_target
            // For now, update the composer placeholder to indicate reply mode
            let placeholder = reply_preview.clone();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    // Store the reply target message ID (using a data property we'll check on send)
                    // For now, update placeholder to show reply context
                    w.set_composer_placeholder(placeholder.into());

                    // Future: Store reply target in chat state
                    tracing::info!("Reply mode activated for message: {}", msg_id);
                }
            });
        });
    });

    // ========== react-clicked callback ==========
    let weak_window = window.as_weak();
    window.on_react_clicked(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let weak = weak_window.clone();
        tracing::debug!("React clicked for message: {}", message_id_str);

        // Store pending reaction target and show emoji picker
        // For now, log the event - emoji picker UI integration will come in a later phase
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(_w) = weak.upgrade() {
                // Future: Set emoji-picker-visible property and store pending reaction message
                // w.set_emoji_picker_visible(true);
                // The emoji picker selection will trigger add_reaction via reaction_service
                tracing::info!("Emoji picker requested for message: {}", message_id_str);
            }
        });
    });

    // ========== thread-clicked callback ==========
    let weak_window = window.as_weak();
    let chat_state_thread = chat_state.clone();

    window.on_thread_clicked(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let _weak = weak_window.clone(); // Future: Update thread panel UI
        let chat_state = chat_state_thread.clone();

        tokio::spawn(async move {
            // Parse message ID
            let msg_id: chatter_linux::domain::ids::MessageId = match message_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid message ID for thread: {}", e);
                    return;
                }
            };

            // Get channel from chat state
            let channel_id = match chat_state.get().active_channel_id {
                Some(id) => id,
                None => {
                    tracing::warn!("Cannot open thread: no active channel");
                    return;
                }
            };

            // Open thread panel in chat state (handles max 3 concurrent threads)
            chat_state.open_thread(channel_id, msg_id);

            tracing::info!("Thread opened for message: {} in channel: {}", msg_id, channel_id);

            // Future: Fetch thread messages and update UI
            // For now, the thread panel visibility will be handled by UI binding to chat state
        });
    });

    // ========== more-clicked callback ==========
    let weak_window = window.as_weak();
    window.on_more_clicked(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let weak = weak_window.clone();
        tracing::debug!("More clicked for message: {}", message_id_str);

        // Store selected message ID for context menu actions (edit, delete, forward, pin)
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(_w) = weak.upgrade() {
                // Future: Show context menu modal or dropdown
                // Context menu actions will be wired separately
                tracing::info!("Context menu requested for message: {}", message_id_str);
            }
        });
    });
}

// ============================================================================
// Header Callbacks (search, pinned, settings)
// ============================================================================

/// Wire header callbacks (search, pinned, chat-settings).
fn wire_header_callbacks(window: &MainWindow, ctx: &AppContext) {
    let chat_state = ctx.chat_state.clone();
    let message_repo = ctx.message_repo.clone();
    let message_state = ctx.message_state.clone();

    // ========== search-clicked callback ==========
    let weak_window = window.as_weak();
    window.on_search_clicked(move || {
        let weak = weak_window.clone();
        tracing::debug!("Search clicked");

        // Open search modal (placeholder for now - search modal not yet built)
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(_w) = weak.upgrade() {
                // Future: Set active-modal to "search" when search modal is implemented
                // w.set_active_modal("search".into());
                tracing::info!("Search modal requested (not yet implemented)");
            }
        });
    });

    // ========== pinned-clicked callback ==========
    let weak_window = window.as_weak();
    let chat_state_pinned = chat_state.clone();

    window.on_pinned_clicked(move || {
        let weak = weak_window.clone();
        let chat_state = chat_state_pinned.clone();

        tokio::spawn(async move {
            // Get active channel
            let channel_id = match chat_state.get().active_channel_id {
                Some(id) => id,
                None => {
                    tracing::warn!("Cannot show pinned messages: no active channel");
                    return;
                }
            };

            tracing::debug!("Pinned messages clicked for channel: {}", channel_id);

            // Future: Toggle pinned bar visibility and fetch pinned messages
            // For now, log the request
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(_w) = weak.upgrade() {
                    // Future: Toggle pinned bar visibility property
                    // w.set_pinned_bar_visible(!w.get_pinned_bar_visible());
                    tracing::info!("Pinned messages bar toggled for channel: {}", channel_id);
                }
            });
        });
    });

    // ========== chat-settings-clicked callback ==========
    let weak_window = window.as_weak();
    let chat_state_settings = chat_state.clone();

    window.on_chat_settings_clicked(move || {
        let weak = weak_window.clone();
        let chat_state = chat_state_settings.clone();

        let channel_id = chat_state.get().active_channel_id;

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(_w) = weak.upgrade() {
                if let Some(channel_id) = channel_id {
                    // Future: Open channel settings modal
                    // For now, just log the request
                    tracing::info!("Channel settings requested for: {}", channel_id);
                    // w.set_active_modal("channel-settings".into());
                } else {
                    tracing::warn!("Cannot open channel settings: no active channel");
                }
            }
        });
    });

    // ========== load-more callback ==========
    let weak_window = window.as_weak();
    let chat_state_load = chat_state.clone();
    let message_repo_load = message_repo.clone();
    let message_state_load = message_state.clone();
    let user_repo = ctx.user_repo.clone();

    window.on_load_more(move || {
        let weak = weak_window.clone();
        let chat_state = chat_state_load.clone();
        let message_repo = message_repo_load.clone();
        let message_state = message_state_load.clone();
        let user_repo = user_repo.clone();

        tokio::spawn(async move {
            // Get active channel
            let channel_id = match chat_state.get().active_channel_id {
                Some(id) => id,
                None => {
                    tracing::warn!("Cannot load more: no active channel");
                    return;
                }
            };

            // Check if already loading
            if message_state.is_loading(&channel_id) {
                tracing::trace!("Already loading messages for channel: {}", channel_id);
                return;
            }

            // Get current pagination cursor
            let cursor = message_state.get_cursor(&channel_id);
            if let Some(ref c) = cursor {
                if !c.has_more {
                    tracing::trace!("No more messages to load for channel: {}", channel_id);
                    return;
                }
            }

            // Set loading state
            message_state.set_loading(channel_id, true);

            // Determine the before cursor (oldest message ID)
            let before = cursor.and_then(|c| c.oldest_message_id);

            // Fetch older messages from local repository
            let older_messages = match message_repo.list_for_channel(&channel_id, before.as_ref(), 50) {
                Ok(msgs) => msgs,
                Err(e) => {
                    tracing::error!("Failed to load more messages: {}", e);
                    message_state.set_loading(channel_id, false);
                    return;
                }
            };

            // Update pagination cursor
            let has_more = older_messages.len() >= 50; // Assume more if we got a full page
            let new_cursor = if !older_messages.is_empty() {
                chatter_linux::state::PaginationCursor::new(
                    has_more,
                    older_messages.first().map(|m| m.id),
                    older_messages.first().map(|m| m.created_at),
                )
            } else {
                chatter_linux::state::PaginationCursor::exhausted()
            };
            message_state.set_cursor(channel_id, new_cursor);

            // Convert to Slint format with sender names
            let mut slint_messages: Vec<MessageItemData> = Vec::with_capacity(older_messages.len());
            for msg in &older_messages {
                let sender_name = if let Some(sender_id) = &msg.sender_id {
                    user_repo
                        .get_profile(sender_id)
                        .ok()
                        .flatten()
                        .map(|p| p.display_name)
                        .or_else(|| {
                            user_repo
                                .get(sender_id)
                                .ok()
                                .flatten()
                                .map(|u| u.username)
                        })
                        .unwrap_or_else(|| "Unknown".to_string())
                } else {
                    "Anonymous".to_string()
                };
                slint_messages.push(message_to_slint(msg, &sender_name));
            }

            // Prepend older messages to UI
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    // Get current messages
                    let current_model = w.get_messages();
                    let current_count = current_model.row_count();

                    // Create combined list (older first, then current)
                    let mut combined: Vec<MessageItemData> = slint_messages;
                    for i in 0..current_count {
                        if let Some(msg) = current_model.row_data(i) {
                            combined.push(msg);
                        }
                    }

                    w.set_messages(ModelRc::new(VecModel::from(combined)));
                    tracing::debug!("Loaded {} older messages for channel", older_messages.len());
                }
            });

            // Clear loading state
            message_state.set_loading(channel_id, false);
        });
    });
}

// ============================================================================
// Composer Callbacks (attachments, emoji, gif, mentions)
// ============================================================================

/// Wire composer callbacks (attachment, emoji, gif, mention).
fn wire_composer_callbacks(window: &MainWindow, _ctx: &AppContext) {
    // ========== attachment-clicked callback ==========
    let weak_window = window.as_weak();
    window.on_attachment_clicked(move || {
        let weak = weak_window.clone();
        tracing::debug!("Attachment clicked");

        // Use spawn_blocking for file picker dialog (requires native dialog)
        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create runtime for file picker");

            rt.block_on(async {
                use chatter_linux::media::file_picker::{FileFilter, FilePicker, FilePickerConfig};

                // Configure file picker with common attachment filters
                let config = FilePickerConfig::new()
                    .with_title("Select Attachment")
                    .with_filter(FileFilter::images())
                    .with_filter(FileFilter::documents())
                    .with_filter(FileFilter::videos())
                    .with_filter(FileFilter::audio())
                    .with_filter(FileFilter::all())
                    .with_multiple(true);

                // Open file picker dialog
                let selected_files = FilePicker::pick_files(config).await;

                if selected_files.is_empty() {
                    tracing::debug!("File picker cancelled or no files selected");
                    return;
                }

                tracing::info!("Selected {} file(s) for attachment", selected_files.len());
                for file in &selected_files {
                    tracing::debug!("  - {:?}", file);
                }

                // Update UI to show selected attachments
                // Files will be uploaded when the message is sent
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        // Future: Store pending attachments in chat state
                        // Future: Update composer UI to show attachment previews
                        // For now, update placeholder to indicate attachments pending
                        let count = selected_files.len();
                        let placeholder = if count == 1 {
                            "1 file attached - type a message...".to_string()
                        } else {
                            format!("{} files attached - type a message...", count)
                        };
                        w.set_composer_placeholder(placeholder.into());
                    }
                });
            });
        });
    });

    // ========== emoji-clicked callback ==========
    let weak_window = window.as_weak();
    window.on_emoji_clicked(move || {
        let weak = weak_window.clone();
        tracing::debug!("Emoji picker clicked");

        // Toggle emoji picker visibility
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(_w) = weak.upgrade() {
                // Future: Toggle emoji picker visibility property
                // When emoji is selected, it will be inserted into composer text
                // For now, log the request
                tracing::info!("Emoji picker toggled (UI component not yet implemented)");
            }
        });
    });

    // ========== gif-clicked callback ==========
    let weak_window = window.as_weak();
    window.on_gif_clicked(move || {
        let weak = weak_window.clone();
        tracing::debug!("GIF picker clicked");

        // Toggle GIF picker visibility
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(_w) = weak.upgrade() {
                // Future: Toggle GIF picker visibility property
                // GIF selection will insert the GIF as an attachment
                // For now, log the request
                tracing::info!("GIF picker toggled (UI component not yet implemented)");
            }
        });
    });

    // ========== mention-triggered callback ==========
    let weak_window = window.as_weak();

    window.on_mention_triggered(move |query: SharedString| {
        let query_str = query.to_string();
        let weak = weak_window.clone();

        if query_str.is_empty() {
            // Empty query - close mention autocomplete
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(_w) = weak.upgrade() {
                    // Future: Hide mention autocomplete dropdown
                    tracing::trace!("Mention autocomplete closed");
                }
            });
            return;
        }

        tracing::debug!("Mention triggered with query: {}", query_str);

        // Future: Search users matching the query in the current channel
        // For now, just log the request - full mention autocomplete requires:
        // 1. A user search endpoint or local cache of channel members
        // 2. UI component for displaying and selecting from suggestions
        // 3. Insertion logic for @mention into composer text
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(_w) = weak.upgrade() {
                tracing::info!("Mention search for '{}' (autocomplete not yet implemented)", query_str);
            }
        });
    });
}
