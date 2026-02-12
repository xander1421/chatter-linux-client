//! Login callback wiring.

use slint::ComponentHandle;

use chatter_linux::app::AppContext;
use chatter_linux::network::grpc::auth_client::AuthClient;
use chatter_linux::services::auth_service::AuthService;

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
    let database = ctx.database.clone();
    let mailbox_manager = ctx.mailbox_manager.clone();
    let event_bus = ctx.event_bus.clone();
    let group_coordinator = ctx.group_coordinator.clone();

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
        let database = database.clone();
        let mailbox_manager = mailbox_manager.clone();
        let event_bus = event_bus.clone();
        let group_coordinator = group_coordinator.clone();
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

                    // Spawn DM bootstrap handler for processing incoming key exchanges
                    let _bootstrap_handle = chatter_linux::inbox::spawn_dm_bootstrap_handler_from_bus(
                        database.clone(),
                        grpc_pool.clone(),
                        mailbox_manager.clone(),
                        &event_bus,
                    );
                    tracing::debug!("DM bootstrap handler spawned");

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
