//! Login callback wiring.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_linux::app::AppContext;
use chatter_linux::services::auth_service::AuthService;
use chatter_linux::services::ChannelEventService;

use super::channel_events::spawn_channel_events_handler;
use super::inbox_events::handle_inbox_event;
use super::initial_data::load_initial_data;
use crate::MainWindow;

pub fn wire_login(window: &MainWindow, ctx: &AppContext) {
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
    let group_coordinator = ctx.group_coordinator.clone();
    let dm_bootstrap_service = ctx.dm_bootstrap_service.clone();
    let database = ctx.database.clone();
    let event_bus = ctx.event_bus.clone();
    let message_service = ctx.message_service.clone();
    let mailbox_manager = ctx.mailbox_manager.clone();
    let db_path = ctx.database.path().to_path_buf();
    // Use cached passphrase from context (avoids multiple keyring prompts)
    let db_passphrase = Some(ctx.db_passphrase.clone());

    let weak_window = window.as_weak();

    window.on_login(move |username: slint::SharedString, password: slint::SharedString| {
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
        let group_coordinator = group_coordinator.clone();
        let dm_bootstrap_service = dm_bootstrap_service.clone();
        let database = database.clone();
        let event_bus = event_bus.clone();
        let message_service = message_service.clone();
        let mailbox_manager = mailbox_manager.clone();
        let db_path = db_path.clone();
        let db_passphrase = db_passphrase.clone();
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

            // Create auth service with grpc_pool for internal client creation
            let auth_service = AuthService::new(
                grpc_pool.clone(),
                identity_manager,
                prekey_manager,
                user_repo.clone(),
                settings_repo.clone(),
                key_store,
                auth_interceptor,
            );

            // Attempt login using simplified method
            match auth_service
                .do_login(&username, &password, "linux-client")
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
                            let dm_bootstrap_service_for_events = dm_bootstrap_service.clone();
                            let attachment_repo_for_events = attachment_repo.clone();
                            let grpc_pool_for_events = grpc_pool.clone();
                            let database_for_events = database.clone();
                            let event_bus_for_events = event_bus.clone();
                            let message_service_for_events = message_service.clone();
                            let mailbox_manager_for_events = mailbox_manager.clone();

                            tokio::spawn(async move {
                                while let Ok(event) = event_rx.recv().await {
                                    handle_inbox_event(
                                        event,
                                        &weak_for_events,
                                        &user_repo_for_events,
                                        &dm_bootstrap_service_for_events,
                                        &attachment_repo_for_events,
                                        &grpc_pool_for_events,
                                        &database_for_events,
                                        &event_bus_for_events,
                                        &message_service_for_events,
                                        &mailbox_manager_for_events,
                                    )
                                    .await;
                                }
                            });
                        }
                    } else {
                        tracing::warn!("Inbox provider not available (offline mode)");
                    }

                    // Spawn DM bootstrap handler for processing incoming key exchanges
                    let auth_info = grpc_pool.auth_info();
                    let _bootstrap_handle = chatter_linux::inbox::spawn_dm_bootstrap_handler_from_bus(
                        db_path.clone(),
                        db_passphrase.clone().unwrap_or_default(),
                        grpc_pool.endpoint(),
                        auth_info.user_id,
                        auth_info.device_id,
                        auth_info.token,
                        &event_bus,
                    );
                    tracing::debug!("DM bootstrap handler spawned");

                    // Spawn channel events handler for member join/leave notifications
                    let channel_event_service = Arc::new(ChannelEventService::new(
                        message_repo.clone(),
                        channel_repo.clone(),
                        user_repo.clone(),
                    ));
                    let _channel_events_handle = spawn_channel_events_handler(
                        weak.clone(),
                        grpc_pool.clone(),
                        channel_repo.clone(),
                        channel_event_service,
                        signal_service.clone(),
                    );
                    tracing::debug!("Channel events handler spawned");

                    // Switch to main view and stop animation
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
                        group_coordinator,
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
